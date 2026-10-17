/**
 * The wire connection: ADR-0038 §3's client lifecycle over ADR-0036 §5's
 * `/v1/ws`, for what S7-min serves (G3a, #1747).
 *
 * **Connection states:** Disconnected, Connecting, Hello, Syncing, Live,
 * Stale, Reconnecting and NeedsToken, with §3's tables as the contract and
 * ADR-0036 §5a's parameters: connect 10 s, handshake 5 s, liveness 45 s,
 * backoff 500 ms × 2ⁿ capped at 30 s with equal jitter and reset after 60 s
 * of continuous Live.
 *
 * Streams and commands are #1747's second and third slices. Until they
 * land, a `welcome` leads straight from Syncing to Live (there is no stream
 * to settle), and any stream frame or `ack` answers nothing this client
 * sent, so it is a protocol error.
 *
 * Every timer runs on the injected {@link Clock}; every socket comes from the
 * injected {@link SocketFactory}. The connection is synchronous: each socket
 * or timer callback runs to completion, with no microtasks of its own.
 *
 * Left for G3 #1595 (G3b): ReadOnly. A server with an older protocol stops
 * the connection with a visible reason, because the client cannot yet enter
 * ReadOnly.
 *
 * @module
 */
import type { ClientFrame } from "../gen/ClientFrame";
import type { ServerFrame } from "../gen/ServerFrame";
import type { WireClient } from "../gen/WireClient";
import { decodeServerFrame } from "./decode";
import { backoffDelay, type Clock, type SocketFactory, type SocketLike, type TimerHandle } from "./env";

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

/** What the connection was created with. */
export type ConnectionOptions = {
  readonly url: string;
  readonly token: string;
  readonly client: WireClient;
  readonly clock: Clock;
  readonly sockets: SocketFactory;
  readonly random: () => number;
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
  #welcome: Welcome | null = null;
  #error: string | null = null;
  #listeners = new Set<() => void>();
  #transitions = new Set<(state: ConnectionState) => void>();
  readonly #options: ConnectionOptions;

  constructor(options: ConnectionOptions) {
    this.#options = options;
    this.#token = options.token;
  }

  /** The connection state. */
  get state(): ConnectionState {
    return this.#state;
  }

  /** The latest failure, shown while reconnecting or stopped; never a token. */
  get error(): string | null {
    return this.#error;
  }

  /** The principal of the last `welcome`. */
  get principal(): string | null {
    return this.#welcome?.principal ?? null;
  }

  /** Calls `listener` after any change of state. */
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

  /** The user's `disconnect`: closes any socket and cancels retries. */
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
        return this.#protocolError(`ack for ${frame.cmd_id}, but no command was sent`);
      default:
        return this.#protocolError(`${frame.t} for unknown subscription ${frame.sub_id}`);
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
    this.#welcome = frame;
    this.#renewLiveness();
    this.#enter("Syncing");
    this.#enter("Live");
  }
}
