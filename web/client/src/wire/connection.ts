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
 * **Sub ids** are never reused on a connection (ADR-0038 §3, "Sub ids and
 * closed subs"), and the connection allocates every one of them, `s1`,
 * `s2`, … from a single counter: callers never name an id, so no caller's
 * id can collide with one allocated for an automatic re-`sub` (#1816). A
 * re-`sub` on the same socket, after the progress deadline, `lagged` or
 * `cursor_ahead`, moves the stream to the next id and closes the old one,
 * whose late frames are dropped and touch nothing. A reconnect re-sends
 * each `sub` under its current id on the new socket.
 *
 * **The outbox:** see `outbox.ts`. Commands go out only in Live.
 *
 * Every timer runs on the injected {@link Clock}; every socket comes from the
 * injected {@link SocketFactory}, or, bridged, from the shell. The connection
 * is synchronous: each socket or timer callback runs to completion, with no
 * microtasks of its own.
 *
 * **Bridged** (ADR-0038 §6, #2500): the shell's native side carries `/v1/ws`
 * and holds the bearer, so `hello` carries no token, and each attempt runs on
 * a session the shell opened: {@link WireConnection.setSocket} hands one over,
 * open, and an attempt without one waits for it in Connecting under the
 * connect deadline ({@link WireConnection.awaitingSession}). Everything after
 * the socket is the same code: a 1006 or 1008 close is Socket lost, and 4401
 * or 4403 is NeedsToken.
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
import type { BridgeSocket } from "../config";
import { decodeServerFrame } from "./decode";
import { backoffDelay, bridgeSocket, uuidv7, type Clock, type KeyValueStore, type SocketFactory, type SocketLike, type TimerHandle } from "./env";
import { Outbox, type OutboxEntry } from "./outbox";

// Only this module can name the connection's internal handoff method.
const providerSessionHandoff = Symbol("provider session handoff");

/** Public handoff helper; CI's source scan restricts its use to provider.ts. */
export function replaceProviderSession(connection: WireConnection, install: () => void): void {
  connection[providerSessionHandoff](install);
}

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

/** A subscription the connection opened, as {@link WireConnection.subscribe} returns it. */
export type Subscription = {
  /**
   * Its sub id on the wire now. A re-sub on the same socket moves it to a
   * fresh one, so hold the subscription, never this id.
   */
  readonly id: string;
  /** Ends it, whatever its id has become; no handler call follows. */
  readonly close: () => void;
};

/** A stream name the client can subscribe to (ADR-0036 §5), minus `*\/live`. */
export type StreamName = `agent/${string}` | `queue/${string}` | `terminal/${string}` | "events" | "fleet" | "inbox" | "artifacts" | "work";

/** History streams have no snapshot; every other stream does (ADR-0038 §3). */
export function isHistoryStream(stream: string): boolean {
  return stream.startsWith("agent/") || stream === "events";
}

/**
 * Receives one stream's progress. Each callback runs synchronously, and may
 * close its own subscription ({@link Subscription.close}); nothing of that
 * subscription follows.
 */
export interface StreamHandler {
  /** An event to apply. The cursor advances once this returns. */
  event(seq: number, ev: WireEvent, tsMs: number): void;
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

/** Where each attempt's socket comes from. */
export type Endpoint =
  | {
      readonly url: string;
      /** The token for `hello`; null connects straight into NeedsToken. */
      readonly token: string | null;
      readonly sockets: SocketFactory;
    }
  /** The shell's bridge: sessions arrive through {@link WireConnection.setSocket}, and `hello` has no token. */
  | { readonly bridged: true };

/** What the connection was created with. */
export type ConnectionOptions = Endpoint & {
  readonly client: WireClient;
  readonly clock: Clock;
  readonly random: () => number;
  readonly store: KeyValueStore;
  /** Applied cursors by stream, to resume from (in memory only, see above). */
  readonly cursors?: { readonly [stream: string]: number };
  /** Mints `cmd_id`s; UUIDv7 from the wall clock by default. */
  readonly newCommandId?: () => string;
};

type Sub = {
  /** The id on the wire now: a re-sub on the same socket moves it to a fresh one. */
  id: string;
  readonly stream: string;
  readonly handler: StreamHandler;
  state: StreamState;
  cursor: number | null;
  /** Reached Current or Refused on the current connection. */
  settled: boolean;
  progress: TimerHandle | null;
  /** Automatic contention retries since the last synced barrier, capped at three. */
  busyRetries: number;
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
  /** Bridged: a session the shell opened for the next attempt. */
  #pending: SocketLike | null = null;
  /** Bridged: the attempt in Connecting waits for the shell's session. */
  #awaiting = false;
  #generation = 0;
  #attempt = 0;
  #acks = 0;
  #timers = new Map<Timer, TimerHandle>();
  #subs = new Map<string, Sub>();
  /** The last sub id allocated: ids are `s<n>`, and n only grows (ADR-0038 §3). */
  #nextSub = 0;
  /** Sub ids ended by `unsub` on this socket, whose late frames are not errors. */
  #ended = new Set<string>();
  #cursors: Map<string, number>;
  #welcome: Welcome | null = null;
  #error: string | null = null;
  #listeners = new Set<() => void>();
  #transitions = new Set<(state: ConnectionState) => void>();
  #userConnects = new Set<() => void>();
  readonly #options: ConnectionOptions;
  readonly #outbox: Outbox;

  constructor(options: ConnectionOptions) {
    this.#options = options;
    this.#token = "bridged" in options ? null : options.token;
    this.#cursors = new Map(Object.entries(options.cursors ?? {}));
    this.#outbox = new Outbox(options.store, () => this.#changed());
  }

  /** The connection state. */
  get state(): ConnectionState {
    return this.#state;
  }

  /** Bridged: an attempt waits in Connecting for the shell to open a session. */
  get awaitingSession(): boolean {
    return this.#awaiting;
  }

  /** The latest failure, shown while reconnecting or stopped; never a token. */
  get error(): string | null {
    return this.#error;
  }

  /** How many commands an ok `ack` settled on this connection: it only grows, whatever the outbox drops. */
  get acks(): number {
    return this.#acks;
  }

  /** What a changed principal dropped from the outbox, until dismissed or the next ok `ack`. */
  get outboxDropped(): string | null {
    return this.#outbox.dropped;
  }

  /** The person has seen {@link outboxDropped}. */
  dismissOutboxDropped(): void {
    this.#outbox.dismissDropped();
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

  /** User Connect actions before their attempt; internal session replacement never emits this. */
  onUserConnect(listener: () => void): () => void {
    this.#userConnects.add(listener);
    return () => this.#userConnects.delete(listener);
  }

  /** The user's `connect`: from Disconnected only. */
  connect(): void {
    if (this.#state !== "Disconnected") return;
    for (const listener of this.#userConnects) listener();
    this.#attemptNow();
  }

  /**
   * Install a replacement credential/session while Disconnected, then resume.
   * This internal handoff preserves the provider's auth budget: it is not a
   * user Connect. If installation throws, the connection stays Disconnected.
   */
  [providerSessionHandoff](install: () => void): void {
    this.disconnect();
    install();
    this.#attemptNow();
  }

  /** The user's `disconnect`: closes any socket, cancels retries, holds the outbox. */
  disconnect(): void {
    this.#abandon(NORMAL, "disconnect");
    this.#pending?.close(NORMAL, "disconnect");
    this.#pending = null;
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

  /**
   * Bridged: a session the shell opened, for the attempt waiting on one or,
   * from NeedsToken, the attempt it starts. Otherwise it is held for the
   * next attempt, until it closes or a Disconnect closes it.
   */
  setSocket(socket: BridgeSocket): void {
    const held = bridgeSocket(socket);
    held.onclose = () => {
      if (this.#pending === held) this.#pending = null;
    };
    this.#pending?.close(NORMAL, "replaced");
    this.#pending = held;
    if (this.#awaiting || this.#state === "NeedsToken") this.#attemptNow();
  }

  /**
   * Bridged: the shell could not open a session (a refusal, never an
   * outage): NeedsToken, until {@link setSocket}. A session the person
   * stopped, or one already in NeedsToken, stays as it is.
   */
  sessionRefused(): void {
    if (!("bridged" in this.#options) || this.#state === "Disconnected" || this.#state === "NeedsToken") return;
    this.#abandon(NORMAL, "the shell refused the session");
    this.#clear("backoff");
    this.#enter("NeedsToken");
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
   * The next sub id. The counter lives here, not in a view or adapter, so
   * remounting one cannot reuse an id whose late frames may still be in
   * flight, and nothing outside allocates one (ADR-0038 §3).
   */
  #allocate(): string {
    return `s${++this.#nextSub}`;
  }

  /**
   * Subscribes to `stream` from `after`, or from the applied cursor this
   * connection holds for the stream, under the next sub id. Sent at once
   * when connected, otherwise after the next `welcome`.
   *
   * A re-sub on the same socket (the progress deadline, `lagged`,
   * `cursor_ahead`) moves the subscription to the next id (ADR-0038 §3,
   * ADR-0036 §5a); the returned {@link Subscription} follows it.
   */
  subscribe(stream: StreamName, handler: StreamHandler, after?: number | null): Subscription {
    const cursor = after === undefined ? (this.#cursors.get(stream) ?? null) : after;
    const sub: Sub = { id: this.#allocate(), stream, handler, state: "Replaying", cursor, settled: false, progress: null, busyRetries: 0, refusal: null, snapshot: null };
    this.#subs.set(sub.id, sub);
    if (this.#connected()) this.#sendSub(sub);
    this.#settle();
    return {
      get id() {
        return sub.id;
      },
      close: () => this.#unsubscribe(sub),
    };
  }

  #unsubscribe(sub: Sub): void {
    if (this.#subs.get(sub.id) !== sub) return;
    this.#stopProgress(sub);
    this.#subs.delete(sub.id);
    if (this.#connected()) {
      this.#ended.add(sub.id);
      // A refused sub was already ended by the server's `subfail` (ADR-0036 §5a): nothing to unsub.
      if (sub.state !== "Refused") this.#send({ t: "unsub", sub_id: sub.id });
    }
    this.#settle();
  }

  /**
   * Queues a command (ADR-0038 §3's outbox). It is persisted as `pending`
   * before any send, and sent in Live. Returns its `cmd_id`. `echoAfter`
   * is stored with it as `echo_after` (`outbox.ts`), and never sent.
   *
   * @throws if no principal is known yet: a command is stored with the
   *   principal that queued it, so none can be queued before a first
   *   `welcome`.
   */
  command(verb: string, args: JsonValue, cmdId?: string, echoAfter?: number): string {
    const principal = this.principal;
    if (principal === null) throw new Error("no principal yet: connect once before sending commands");
    const id = cmdId ?? this.#options.newCommandId?.() ?? uuidv7(this.#options.clock.wallNow());
    this.#outbox.add({ cmd_id: id, verb, args, principal, created_ms: this.#options.clock.wallNow(), status: "pending", ...(echoAfter === undefined ? {} : { echo_after: echoAfter }) });
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
    const endpoint = this.#options;
    if ("bridged" in endpoint) {
      // The shell opened it already: the attempt starts on it, open, or waits for one.
      const session = this.#pending;
      this.#pending = null;
      this.#begin(() => session, session === null);
      if (session !== null) this.#opened();
      return;
    }
    if (this.#token === null) {
      this.#enter("NeedsToken");
      return;
    }
    this.#begin(() => endpoint.sockets(endpoint.url), false);
  }

  /** Enters Connecting under the connect deadline, on the socket `open` gives, or `awaiting` the shell's session. */
  #begin(open: () => SocketLike | null, awaiting: boolean): void {
    const generation = ++this.#generation;
    this.#awaiting = awaiting;
    this.#enter("Connecting");
    const socket = open();
    this.#arm("connect", DEADLINES.connect, () => this.#lost(NORMAL, "the connect deadline (10 s) elapsed"));
    if (socket === null) return;
    this.#socket = socket;
    const current = (): boolean => generation === this.#generation;
    socket.onopen = () => current() && this.#opened();
    socket.onmessage = (event) => current() && this.#frame(event.data);
    socket.onclose = (event) => current() && this.#closed(event.code, event.reason);
    socket.onerror = () => undefined;
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
    this.#awaiting = false;
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
    // A reused sub id is the client's own fault, which no new `welcome` cures (ADR-0036 §5a).
    if (refusal.code === "sub_id_reused") return false;
    if (before === null) return true;
    switch (refusal.code) {
      case "scope":
        return before.principal !== after.principal || before.capabilities.join() !== after.capabilities.join();
      case "operator_only":
        return before.principal !== after.principal;
      case "invalid_agent":
        return false;
      case "unknown_stream":
        return before.server_version !== after.server_version;
      case "cursor_ahead":
      case "session_busy":
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

  /**
   * Re-subscribes on the same socket under a fresh id (ADR-0038 §3, "Sub
   * ids and closed subs"; ADR-0036 §5a). The old id is closed: its late
   * frames are dropped. `unsub` is sent for it only when the server still
   * holds it, not after `lagged` or `subfail`, which closed it there.
   * Handler, cursor and state move with the subscription.
   */
  #resub(sub: Sub, unsubOld: boolean): void {
    const old = sub.id;
    if (unsubOld) this.#send({ t: "unsub", sub_id: old });
    this.#subs.delete(old);
    this.#ended.add(old);
    sub.id = this.#allocate();
    this.#subs.set(sub.id, sub);
    this.#sendSub(sub);
  }

  /** The progress deadline: on expiry, `unsub`, then a re-sub from the applied cursor. */
  #startProgress(sub: Sub): void {
    this.#stopProgress(sub);
    sub.progress = this.#options.clock.setTimeout(() => {
      sub.progress = null;
      this.#resub(sub, true);
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
        sub.handler.event(frame.seq, frame.ev, frame.ts_ms);
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
        sub.busyRetries = 0;
        sub.settled = true;
        sub.handler.synced();
        return this.#settle();
      case "lagged":
        // `skipped_to` is diagnostic only. Resume from the applied cursor.
        // `lagged` closes the id on the server, like `subfail`.
        this.#resub(sub, false);
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
    if (code === "sub_id_reused") {
      /*
       * ADR-0036 §5a: the server refuses a `sub` whose id this connection
       * already used. That is never retried: the stream is Refused, says so,
       * and its id stays closed (Refused frames are dropped, and no `welcome`
       * cures it; see `#cures`).
       */
      const reason = `The server refused sub id ${sub.id} as already used on this connection, so this stream is not retried. Server: ${message}`;
      return this.#refuse(sub, { code, message: reason });
    }
    if (code === "session_busy") {
      this.#refuse(sub, { code, message });
      if (this.#subs.get(sub.id) !== sub || !this.#connected() || sub.busyRetries === 3) return;
      // Three per-stream retries: 250, 500, 1000 ms, reset only by synced.
      // Reuse the cancellable progress timer; never touch sibling subscriptions.
      const delay = 250 * 2 ** sub.busyRetries++;
      sub.progress = this.#options.clock.setTimeout(() => {
        sub.progress = null;
        sub.settled = false;
        this.#resub(sub, false);
        this.#settle();
      }, delay);
      return;
    }
    if (code !== "cursor_ahead") return this.#refuse(sub, { code, message });
    sub.handler.reset();
    if (this.#subs.get(sub.id) !== sub) return this.#changed();
    sub.cursor = null;
    this.#cursors.delete(sub.stream);
    this.#resub(sub, false);
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
    if (error === null) this.#acks++;
  }
}
