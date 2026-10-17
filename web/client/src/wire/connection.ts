/**
 * The wire connection: ADR-0038 §3's client lifecycle over ADR-0036 §5's
 * `/v1/ws`, for what S7-min serves (G3a, #1747).
 *
 * **Connection states:** Disconnected, Connecting, Hello, Syncing, Live,
 * Stale, Reconnecting and NeedsToken, with §3's tables as the contract and
 * ADR-0036 §5a's parameters: connect 10 s, handshake 5 s, liveness 45 s,
 * subscription progress 30 s, command ack 30 s, backoff 500 ms × 2ⁿ capped
 * at 30 s with equal jitter and reset after 60 s of continuous Live.
 *
 * **Stream states:** Replaying, Resyncing, Current and Refused, per `sub`.
 * A history stream's `gap` hands its consumer a missing-range marker and
 * continues from `to + 1`. A snapshot stream's `gap` (ADR-0036 §4, §5) is
 * Resyncing until its lines reach `snapshot_to`; the consumer then gets the
 * whole snapshot at once, so it is never half-applied, and the cursor moves
 * to `snapshot_to`. An empty snapshot (`snapshot_to == to`) completes at
 * once. A newer snapshot `gap` supersedes an unfinished one, and `synced`
 * before the end is a protocol error.
 *
 * **The outbox:** see `outbox.ts`. Commands go out only in Live.
 *
 * Every timer runs on the injected {@link Clock}; every socket comes from the
 * injected {@link SocketFactory}. The connection is synchronous: each socket
 * or timer callback runs to completion, with no microtasks of its own.
 *
 * Reading of S7a's scripts where ADR-0038 §3 is silent: a `sub` that has
 * not yet settled on this connection takes a Live connection back to
 * Syncing until it does (`subscription-deadline.jsonl`). A re-`sub` of a
 * stream that has settled, after `lagged` or `cursor_ahead`, keeps Live.
 * The ADR wording is tracked in #1752.
 *
 * Left for G3 #1595 (G3b), as seams: resume-only resends and window expiry
 * (`unknown_outcome`), persisted resync markers, ReadOnly, `*\/live`
 * streams, `operator_only` refusals, and cursor persistence (cursors are kept
 * in memory until the entity store is persisted with them). A re-`sub`
 * during Resyncing drops the unfinished snapshot: the cursor has not moved,
 * so the server starts it again. A server with an older protocol stops the
 * connection, because the client cannot yet enter ReadOnly.
 *
 * @module
 */
import type { ClientFrame } from "../gen/ClientFrame";
import type { ServerFrame } from "../gen/ServerFrame";
import type { SubfailCode } from "../gen/SubfailCode";
import type { WireClient } from "../gen/WireClient";
import type { WireError } from "../gen/WireError";
import type { WireEvent } from "../gen/WireEvent";
import type { JsonValue } from "../gen/serde_json/JsonValue";
import { decodeServerFrame } from "./decode";
import { backoffDelay, uuidv7, type Clock, type KeyValueStore, type SocketFactory, type SocketLike, type TimerHandle } from "./env";
import { Outbox, type OutboxEntry } from "./outbox";

/**
 * The JSON wire version this client speaks: `WIRE_JSON_VERSION` in
 * `src/structured_wire/mod.rs`, which a test keeps in step.
 */
export const WIRE_PROTOCOL = 1;

/** ADR-0036 §5a's parameters, in milliseconds. */
export const DEADLINES = {
  connect: 10_000,
  handshake: 5_000,
  liveness: 45_000,
  progress: 30_000,
  ack: 30_000,
  healthy: 60_000,
} as const;

/** ADR-0038 §3's connection states that G3a reaches. */
export type ConnectionState =
  | "Disconnected"
  | "Connecting"
  | "Hello"
  | "Syncing"
  | "Live"
  | "Stale"
  | "Reconnecting"
  | "NeedsToken";

/** ADR-0038 §3's stream states that G3a reaches. */
export type StreamState = "Replaying" | "Resyncing" | "Current" | "Refused";

/** Why a stream was refused: the server's `subfail`. */
export type Refusal = { readonly code: SubfailCode; readonly message: string };

/** A stream name the client can subscribe to (ADR-0036 §5), minus `*\/live`. */
export type StreamName = `agent/${string}` | `queue/${string}` | `terminal/${string}` | "events" | "fleet" | "inbox" | "artifacts" | "work";

/** History streams have no snapshot; every other stream does (ADR-0038 §3). */
export function isHistoryStream(stream: string): boolean {
  return stream.startsWith("agent/") || stream === "events";
}

/**
 * Receives one stream's progress. Each callback runs synchronously, and may
 * {@link WireConnection.unsubscribe} its own subscription; nothing of that
 * subscription follows.
 */
export interface StreamHandler {
  /** An event to apply. The cursor advances once this returns. */
  event(seq: number, ev: WireEvent): void;
  /** History-stream `gap`: seqs `from`–`to` are no longer stored. */
  gap(from: number, to: number): void;
  /** The `synced` barrier: the stream is Current. Once per `sub`. */
  synced(): void;
  /**
   * A completed snapshot: these lines are now every entity of the stream,
   * and anything not among them is gone. Snapshot streams only.
   */
  snapshot(lines: readonly WireEvent[]): void;
  /** Drop every entity of the stream: `cursor_ahead`. */
  reset(): void;
  /** The stream is Refused and ends here. */
  refused(refusal: Refusal): void;
}

/** One subscription, as {@link WireConnection.streams} reports it. */
export type StreamStatus = { readonly stream: string; readonly state: StreamState; readonly cursor: number | null };

/** What the connection was created with. */
export type ConnectionOptions = {
  readonly url: string;
  readonly token: string;
  readonly client: WireClient;
  readonly clock: Clock;
  readonly sockets: SocketFactory;
  readonly random: () => number;
  readonly store: KeyValueStore;
  /** Applied cursors by stream, to resume from (in memory only, see above). */
  readonly cursors?: { readonly [stream: string]: number };
  /** Mints `cmd_id`s; UUIDv7 from the wall clock by default. */
  readonly newCommandId?: () => string;
};

type Sub = {
  readonly id: string;
  readonly stream: string;
  readonly handler: StreamHandler;
  state: StreamState;
  cursor: number | null;
  /** Reached Current or Refused on the current connection. */
  settled: boolean;
  progress: TimerHandle | null;
  refusal: Refusal | null;
  /** The snapshot being received while Resyncing. */
  snapshot: Snapshot | null;
};

type Snapshot = {
  /** The gap that announced it, to recognise an identical repeat. */
  readonly from: number;
  readonly to: number;
  /** Its last seq, the gap's `snapshot_to`. */
  readonly end: number;
  /** The last seq received so far. */
  last: number;
  readonly lines: WireEvent[];
  /** The state to return to at the end: Replaying, or Current. */
  readonly resume: StreamState;
};

type Welcome = Extract<ServerFrame, { t: "welcome" }>;

type Timer = "connect" | "handshake" | "liveness" | "backoff" | "healthy";

const NORMAL = 1000;
const PROTOCOL_ERROR = 1002;
const AUTH_REFUSED = 4401;
const TOKEN_REVOKED = 4403;

/** The client lifecycle; see the module doc. */
export class WireConnection {
  #state: ConnectionState = "Disconnected";
  #token: string | null;
  #socket: SocketLike | null = null;
  #generation = 0;
  #attempt = 0;
  #timers = new Map<Timer, TimerHandle>();
  #subs = new Map<string, Sub>();
  /** Every sub id ever opened here: one is never reused (ADR-0038 §3). */
  #used = new Set<string>();
  #nextSub = 0;
  /** Sub ids ended by `unsub` on this socket, whose late frames are not errors. */
  #ended = new Set<string>();
  #cursors: Map<string, number>;
  #welcome: Welcome | null = null;
  #error: string | null = null;
  #listeners = new Set<() => void>();
  #transitions = new Set<(state: ConnectionState) => void>();
  readonly #options: ConnectionOptions;
  readonly #outbox: Outbox;

  constructor(options: ConnectionOptions) {
    this.#options = options;
    this.#token = options.token;
    this.#cursors = new Map(Object.entries(options.cursors ?? {}));
    this.#outbox = new Outbox(options.store, () => this.#changed());
  }

  /** The connection state. */
  get state(): ConnectionState {
    return this.#state;
  }

  /** The latest failure, shown while reconnecting or stopped; never a token. */
  get error(): string | null {
    return this.#error;
  }

  /** What restoring the stored outbox had to drop, or null (shown, never fatal). */
  get outboxProblem(): string | null {
    return this.#outbox.problem;
  }

  /** The principal of the last `welcome`, or of the restored outbox. */
  get principal(): string | null {
    return this.#welcome?.principal ?? this.#outbox.principal;
  }

  /** Every subscription's state and applied cursor, by sub id. */
  streams(): ReadonlyMap<string, StreamStatus> {
    return new Map([...this.#subs].map(([id, sub]) => [id, { stream: sub.stream, state: sub.state, cursor: sub.cursor }]));
  }

  /** The outbox, oldest first. */
  commands(): readonly OutboxEntry[] {
    return this.#outbox.entries();
  }

  /** Calls `listener` after any change of state, streams or outbox. */
  onChange(listener: () => void): () => void {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  }

  /** Calls `listener` with every connection state entered, in order. */
  onTransition(listener: (state: ConnectionState) => void): () => void {
    this.#transitions.add(listener);
    return () => this.#transitions.delete(listener);
  }

  /** The user's `connect`: from Disconnected only. */
  connect(): void {
    if (this.#state === "Disconnected") this.#attemptNow();
  }

  /** The user's `disconnect`: closes any socket, cancels retries, holds the outbox. */
  disconnect(): void {
    this.#abandon(NORMAL, "disconnect");
    this.#clear("backoff");
    this.#enter("Disconnected");
  }

  /**
   * A new token, the only exit from NeedsToken. In any other state it is
   * kept for the next attempt.
   */
  setToken(token: string): void {
    this.#token = token;
    if (this.#state === "NeedsToken") this.#attemptNow();
  }

  /** A network change: ends a backoff wait with one immediate attempt. */
  networkChange(): void {
    if (this.#state === "Reconnecting") this.#attemptNow();
  }

  /**
   * The page or app came back to the foreground. Frozen time is not
   * liveness: a socket that may have been suspended is closed, and a
   * backoff wait ends, with one immediate attempt either way.
   */
  foreground(): void {
    switch (this.#state) {
      case "Hello":
      case "Syncing":
      case "Live":
        this.#abandon(NORMAL, "foreground");
        this.#enter("Reconnecting");
        this.#attemptNow();
        return;
      case "Reconnecting":
        this.#attemptNow();
        return;
      default:
        return;
    }
  }

  /**
   * A sub id never used on this connection, as `<prefix><n>`. The counter
   * lives here, not in a view or adapter, so remounting one cannot reuse an
   * id whose late frames may still be in flight (ADR-0038 §3).
   */
  nextSubId(prefix: string): string {
    let id: string;
    do id = `${prefix}${++this.#nextSub}`;
    while (this.#used.has(id));
    return id;
  }

  /**
   * Subscribes `id` to `stream` from `after`, or from the applied cursor
   * this connection holds for the stream. Sent at once when connected,
   * otherwise after the next `welcome`.
   *
   * @throws if `id` was already opened on this connection, even if closed since.
   */
  subscribe(id: string, stream: StreamName, handler: StreamHandler, after?: number | null): void {
    if (this.#used.has(id)) throw new Error(`sub id ${id} was already used on this connection`);
    this.#used.add(id);
    const cursor = after === undefined ? (this.#cursors.get(stream) ?? null) : after;
    const sub: Sub = { id, stream, handler, state: "Replaying", cursor, settled: false, progress: null, refusal: null, snapshot: null };
    this.#subs.set(id, sub);
    if (this.#connected()) this.#sendSub(sub);
    this.#settle();
  }

  /** Ends a subscription; no handler call follows. */
  unsubscribe(id: string): void {
    const sub = this.#subs.get(id);
    if (sub === undefined) return;
    this.#stopProgress(sub);
    this.#subs.delete(id);
    if (this.#connected()) {
      this.#ended.add(id);
      // A refused sub was already ended by the server's `subfail` (ADR-0036 §5a): nothing to unsub.
      if (sub.state !== "Refused") this.#send({ t: "unsub", sub_id: id });
    }
    this.#settle();
  }

  /**
   * Queues a command (ADR-0038 §3's outbox). It is persisted as `pending`
   * before any send, and sent in Live. Returns its `cmd_id`.
   *
   * @throws if no principal is known yet: a command is stored with the
   *   principal that queued it, so none can be queued before a first
   *   `welcome`.
   */
  command(verb: string, args: JsonValue, cmdId?: string): string {
    const principal = this.principal;
    if (principal === null) throw new Error("no principal yet: connect once before sending commands");
    const id = cmdId ?? this.#options.newCommandId?.() ?? uuidv7(this.#options.clock.wallNow());
    this.#outbox.add({ cmd_id: id, verb, args, principal, created_ms: this.#options.clock.wallNow(), status: "pending" });
    this.#flush();
    return id;
  }

  // --- lifecycle --------------------------------------------------------

  #connected(): boolean {
    return this.#state === "Syncing" || this.#state === "Live";
  }

  #enter(state: ConnectionState): void {
    if (state === this.#state) return;
    if (this.#state === "Live") this.#clear("healthy");
    this.#state = state;
    for (const listener of this.#transitions) listener(state);
    if (state === "Live") {
      this.#error = null;
      this.#arm("healthy", DEADLINES.healthy, () => (this.#attempt = 0));
      this.#flush();
    }
    this.#changed();
  }

  #changed(): void {
    for (const listener of this.#listeners) listener();
  }

  #arm(timer: Timer, ms: number, callback: () => void): void {
    this.#clear(timer);
    this.#timers.set(
      timer,
      this.#options.clock.setTimeout(() => {
        this.#timers.delete(timer);
        callback();
      }, ms),
    );
  }

  #clear(timer: Timer): void {
    const handle = this.#timers.get(timer);
    if (handle !== undefined) this.#options.clock.clearTimeout(handle);
    this.#timers.delete(timer);
  }

  #attemptNow(): void {
    this.#clear("backoff");
    if (this.#token === null) {
      this.#enter("NeedsToken");
      return;
    }
    const generation = ++this.#generation;
    this.#enter("Connecting");
    const socket = this.#options.sockets(this.#options.url);
    this.#socket = socket;
    const current = (): boolean => generation === this.#generation;
    socket.onopen = () => current() && this.#opened();
    socket.onmessage = (event) => current() && this.#frame(event.data);
    socket.onclose = (event) => current() && this.#closed(event.code, event.reason);
    socket.onerror = () => undefined;
    this.#arm("connect", DEADLINES.connect, () => this.#lost(NORMAL, "the connect deadline (10 s) elapsed"));
  }

  #opened(): void {
    if (this.#state !== "Connecting") return;
    this.#clear("connect");
    this.#enter("Hello");
    const token = this.#token;
    this.#send({ t: "hello", protocol: WIRE_PROTOCOL, client: this.#options.client, ...(token === null ? {} : { token }) });
    this.#arm("handshake", DEADLINES.handshake, () => this.#lost(NORMAL, "no welcome within the handshake deadline (5 s)"));
  }

  #closed(code: number, reason: string): void {
    this.#socket = null;
    if (code === AUTH_REFUSED || code === TOKEN_REVOKED) {
      this.#abandon(null, "");
      this.#token = null;
      this.#error = code === AUTH_REFUSED ? "The server refused the token." : "The token was revoked.";
      this.#clear("backoff");
      this.#enter("NeedsToken");
      return;
    }
    this.#lost(null, `the socket closed (${code}${reason === "" ? "" : `: ${reason}`})`);
  }

  /** Socket lost, a deadline or a protocol error: → Reconnecting under backoff. */
  #lost(closeCode: number | null, reason: string): void {
    this.#abandon(closeCode, reason);
    this.#error = `Connection lost: ${reason}.`;
    this.#enter("Reconnecting");
    this.#arm("backoff", backoffDelay(this.#attempt, this.#options.random), () => this.#attemptNow());
    this.#attempt = Math.min(this.#attempt + 1, 64);
  }

  #protocolError(reason: string): void {
    this.#lost(PROTOCOL_ERROR, `protocol error: ${reason}`);
  }

  /** Leaves the current socket: its later callbacks are discarded. */
  #abandon(closeCode: number | null, reason: string): void {
    this.#generation++;
    for (const timer of ["connect", "handshake", "liveness"] as const) this.#clear(timer);
    for (const sub of this.#subs.values()) this.#stopProgress(sub);
    this.#outbox.socketLost();
    this.#ended.clear();
    const socket = this.#socket;
    this.#socket = null;
    if (socket !== null && closeCode !== null) socket.close(closeCode, reason);
  }

  #send(frame: ClientFrame): void {
    this.#socket?.send(JSON.stringify(frame));
  }

  #renewLiveness(): void {
    this.#arm("liveness", DEADLINES.liveness, () => {
      this.#enter("Stale");
      this.#lost(NORMAL, "no server frame within the liveness window (45 s)");
    });
  }

  // --- frames -----------------------------------------------------------

  #frame(data: unknown): void {
    const decoded = decodeServerFrame(data);
    if (!decoded.ok) return this.#protocolError(decoded.reason);
    const frame = decoded.frame;
    if (this.#state === "Hello") {
      return frame.t === "welcome" ? this.#welcomed(frame) : this.#protocolError(`${frame.t} before welcome`);
    }
    if (!this.#connected()) return;
    this.#renewLiveness();
    switch (frame.t) {
      case "welcome":
        return this.#protocolError("a second welcome");
      case "ping":
        return this.#send({ t: "pong", nonce: frame.nonce });
      case "ack":
        return this.#acked(frame);
      default:
        return this.#streamFrame(frame);
    }
  }

  #welcomed(frame: Welcome): void {
    this.#clear("handshake");
    if (frame.protocol < WIRE_PROTOCOL) {
      this.#abandon(NORMAL, "server too old");
      this.#error = `The server speaks wire protocol ${frame.protocol}; this client needs ${WIRE_PROTOCOL} until read-only mode lands (#1595).`;
      this.#enter("Disconnected");
      return;
    }
    const previous = this.#welcome;
    this.#welcome = frame;
    this.#outbox.adoptPrincipal(frame.principal);
    this.#renewLiveness();
    this.#enter("Syncing");
    for (const sub of this.#subs.values()) {
      sub.settled = false;
      if (sub.refusal === null || this.#cures(sub.refusal, previous, frame)) this.#sendSub(sub);
      else sub.settled = true;
    }
    this.#settle();
  }

  /** Whether a new `welcome` can cure a refusal (ADR-0038 §3, Refused). */
  #cures(refusal: Refusal, before: Welcome | null, after: Welcome): boolean {
    if (before === null) return true;
    switch (refusal.code) {
      case "scope":
        return before.principal !== after.principal || before.capabilities.join() !== after.capabilities.join();
      case "operator_only":
        return before.principal !== after.principal;
      case "unknown_stream":
        return before.server_version !== after.server_version;
      case "cursor_ahead":
        return true;
    }
  }

  #sendSub(sub: Sub): void {
    sub.state = "Replaying";
    sub.refusal = null;
    sub.snapshot = null;
    this.#send({ t: "sub", sub_id: sub.id, stream: sub.stream, ...(sub.cursor === null ? {} : { after: sub.cursor }) });
    this.#startProgress(sub);
  }

  #startProgress(sub: Sub): void {
    this.#stopProgress(sub);
    sub.progress = this.#options.clock.setTimeout(() => {
      sub.progress = null;
      this.#send({ t: "unsub", sub_id: sub.id });
      this.#sendSub(sub);
      this.#changed();
    }, DEADLINES.progress);
  }

  #stopProgress(sub: Sub): void {
    if (sub.progress !== null) this.#options.clock.clearTimeout(sub.progress);
    sub.progress = null;
  }

  /** Syncing ⇄ Live: Live once every subscription has settled. */
  #settle(): void {
    if (!this.#connected()) return this.#changed();
    const settled = [...this.#subs.values()].every((sub) => sub.settled);
    this.#enter(settled ? "Live" : "Syncing");
    this.#changed();
  }

  #streamFrame(frame: Exclude<ServerFrame, { t: "welcome" | "ping" | "ack" }>): void {
    const sub = this.#subs.get(frame.sub_id);
    if (sub === undefined) {
      if (this.#ended.has(frame.sub_id)) return;
      return this.#protocolError(`${frame.t} for unknown subscription ${frame.sub_id}`);
    }
    if (sub.state === "Refused") return;
    switch (frame.t) {
      case "ev": {
        if (frame.stream !== sub.stream) return this.#protocolError(`ev for ${frame.stream} on ${sub.stream}'s subscription`);
        if (frame.seq === undefined) return this.#protocolError(`ev without seq on ${sub.stream}`);
        if (sub.snapshot !== null) return this.#snapshotLine(sub, sub.snapshot, frame.seq, frame.ev);
        if (sub.cursor !== null && frame.seq <= sub.cursor) return;
        sub.handler.event(frame.seq, frame.ev);
        if (this.#subs.get(sub.id) !== sub) return this.#changed();
        this.#advance(sub, frame.seq);
        return;
      }
      case "gap":
        return this.#gap(sub, frame);
      case "synced":
        if (sub.state === "Current") return this.#protocolError(`a second synced on ${sub.stream}`);
        if (sub.snapshot !== null) return this.#protocolError(`synced before ${sub.stream}'s snapshot ended`);
        this.#stopProgress(sub);
        sub.state = "Current";
        sub.settled = true;
        sub.handler.synced();
        return this.#settle();
      case "lagged":
        this.#sendSub(sub);
        return this.#changed();
      case "subfail":
        return this.#subfail(sub, frame.code, frame.message);
    }
  }

  /** Moves the applied cursor; only a real advance resets the progress deadline. */
  #advance(sub: Sub, cursor: number): void {
    const advanced = sub.cursor === null || cursor > sub.cursor;
    sub.cursor = cursor;
    this.#cursors.set(sub.stream, cursor);
    if (advanced && sub.state === "Replaying") this.#startProgress(sub);
    if (sub.state === "Current") this.#stopProgress(sub);
    this.#changed();
  }

  #gap(sub: Sub, frame: Extract<ServerFrame, { t: "gap" }>): void {
    const history = isHistoryStream(sub.stream);
    if (history !== (frame.snapshot_to === undefined)) {
      return this.#protocolError(`a ${history ? "snapshot" : "history"} gap on ${sub.stream}`);
    }
    /*
     * A gap already behind the applied cursor is a duplicate (ADR-0036 §5a):
     * dropped, so it neither repeats a marker nor resets the progress
     * deadline. So is an identical repeat of the snapshot being received.
     */
    if (frame.snapshot_to === undefined) {
      if (sub.cursor !== null && frame.to <= sub.cursor) return;
      sub.handler.gap(sub.cursor === null ? frame.from : Math.max(frame.from, sub.cursor + 1), frame.to);
      if (this.#subs.get(sub.id) !== sub) return this.#changed();
      return this.#advance(sub, frame.to);
    }
    if (frame.snapshot_to < frame.to) return this.#protocolError(`a snapshot on ${sub.stream} ending before its gap`);
    const current = sub.snapshot;
    if (current !== null && current.from === frame.from && current.to === frame.to && current.end === frame.snapshot_to) return;
    if (current === null && sub.cursor !== null && frame.snapshot_to <= sub.cursor) return;
    const resume = current?.resume ?? sub.state;
    sub.snapshot = { from: frame.from, to: frame.to, end: frame.snapshot_to, last: frame.to, lines: [], resume };
    if (frame.snapshot_to === frame.to) return this.#endSnapshot(sub, sub.snapshot);
    sub.state = "Resyncing";
    this.#startProgress(sub);
    this.#changed();
  }

  /** Lines are contiguous from the gap's `to + 1`; a repeat is dropped. */
  #snapshotLine(sub: Sub, snapshot: Snapshot, seq: number, ev: WireEvent): void {
    if (seq <= snapshot.last) return;
    if (seq !== snapshot.last + 1) return this.#protocolError(`snapshot line ${seq} on ${sub.stream} after ${snapshot.last}`);
    snapshot.lines.push(ev);
    snapshot.last = seq;
    if (seq === snapshot.end) return this.#endSnapshot(sub, snapshot);
    this.#startProgress(sub);
    this.#changed();
  }

  #endSnapshot(sub: Sub, snapshot: Snapshot): void {
    sub.snapshot = null;
    sub.state = snapshot.resume;
    sub.handler.snapshot(snapshot.lines);
    if (this.#subs.get(sub.id) !== sub) return this.#changed();
    this.#advance(sub, snapshot.end);
  }

  #subfail(sub: Sub, code: SubfailCode, message: string): void {
    if (code !== "cursor_ahead") return this.#refuse(sub, { code, message });
    sub.handler.reset();
    if (this.#subs.get(sub.id) !== sub) return this.#changed();
    sub.cursor = null;
    this.#cursors.delete(sub.stream);
    this.#sendSub(sub);
    this.#changed();
  }

  #refuse(sub: Sub, refusal: Refusal): void {
    this.#stopProgress(sub);
    sub.state = "Refused";
    sub.refusal = refusal;
    sub.settled = true;
    sub.handler.refused(refusal);
    this.#settle();
  }

  // --- commands ---------------------------------------------------------

  #flush(): void {
    if (this.#state !== "Live") return;
    for (const entry of this.#outbox.pending()) {
      this.#send({ t: "cmd", cmd_id: entry.cmd_id, verb: entry.verb, args: entry.args, resume_only: false });
      this.#outbox.sent(entry.cmd_id, this.#options.clock.wallNow(), this.#options.clock.setTimeout(() => this.#outbox.uncertain(entry.cmd_id), DEADLINES.ack), (handle) => this.#options.clock.clearTimeout(handle));
    }
  }

  #acked(frame: Extract<ServerFrame, { t: "ack" }>): void {
    const error: WireError | null = frame.error ?? null;
    if (frame.ok === (error !== null)) return this.#protocolError(`ack ${frame.cmd_id} with ok=${frame.ok} and ${error === null ? "no" : "an"} error`);
    if (!this.#outbox.settle(frame.cmd_id, error)) return this.#protocolError(`ack for ${frame.cmd_id}, which is not awaiting one on this socket`);
  }
}
