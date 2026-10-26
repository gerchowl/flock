/**
 * A shell's token provider, or its bridge, driven by the connection's
 * NeedsToken (ADR-0038 §6, "The shell config contract" and "Lifetime").
 *
 * **One state machine, two sources.** A bridge's `openBridge` (#2500) is
 * driven by the same rules as a token provider: each call is one session
 * instead of one token, handed over with `setSocket` instead of `setToken`.
 * Two things differ, both in what the connection does: a bridged session is
 * used by one attempt, so every reconnect also asks (`awaitingSession`,
 * outside the streak, which counts only the server's ends), and a refusal
 * then takes the connection to NeedsToken. Only the renewal passes
 * `renew: true`. Below, "token" reads "session" for a bridge.
 *
 * - **When it is asked.** When the person connects (the initial connect,
 *   and {@link ShellTokens.ask}, which NeedsToken offers instead of a
 *   prompt; a pending backoff wait is cancelled for it), and whenever the
 *   server ends this shell's token: a close 4403, or a 4401 at `hello`. A
 *   4401 for a token that once got a `welcome` is what a server restart
 *   leaves (app-session tokens live in its memory only), so it re-mints like
 *   a revocation. A 4401 for a token that never got one counts the same, so a
 *   broken mint is bounded too.
 * - **Before expiry.** A provider token that carries `expiresInMs` (the
 *   lifetime the server reported at mint) is renewed at {@link RENEW_AT} of
 *   it, by one timer, and the connection switches to the new token through
 *   §3's resume path. The timer is armed in steps of at most
 *   {@link MAX_TIMER_DELAY} until the renewal time, so no lifetime is too long.
 *   It is cleared on Disconnect, and it renews only a running session: in
 *   NeedsToken (a spent cap included) the reactive path owns the provider.
 *   A failed renewal is not retried; the session stays Live, the shell's
 *   message is {@link ShellTokens.notice}, and the reactive path takes over
 *   when the server ends the token. Without `expiresInMs` there is no timer.
 * - One call at a time: a request while one is in flight, or waiting, is
 *   the same request.
 * - Its token goes to the connection with `setToken`, which connects only
 *   from NeedsToken; after a Disconnect it is kept for the next Connect.
 * - A rejection, a synchronous throw or a value that is not a
 *   `{ token, expiresInMs? }` is NeedsToken with that message, and is not
 *   retried.
 * - **Bounded re-mints.** Consecutive re-mints back off on §3's reconnect
 *   backoff: the first is immediate, the next ones wait `backoffDelay(0…)`.
 *   After {@link MAX_REMINTS} in a row the provider is not asked again until
 *   the person asks. A session that stayed Live for §3's healthy period (the
 *   one that resets its backoff), or an `ack`ed command since the streak
 *   began, ends the streak.
 *
 * @module
 */
import type { BridgeSession, BridgeSocket, OpenBridge, ShellToken, TokenProvider } from "../config";
import { DEADLINES, type WireConnection } from "./connection";
import { backoffDelay, type Clock, type TimerHandle } from "./env";

/**
 * Consecutive re-mints before the provider is no longer asked: one at once,
 * then §3's backoff for attempts 0 to 5 (ceilings 0.5 s to 16 s). The next
 * wait would be the backoff's 30 s ceiling, so the shell stops instead.
 */
export const MAX_REMINTS = 7;

/** NeedsToken's reason once the re-mints are spent. */
export const REMINTS_SPENT = `The server ended ${MAX_REMINTS} tokens from this shell in a row. Connect to try again.`;

/**
 * The fraction of a token's reported lifetime after which it is renewed:
 * three quarters, so a 12 h token renews 3 h early and even a short one
 * leaves a quarter of its life for the renewal and the reconnect.
 */
export const RENEW_AT = 3 / 4;

/**
 * The longest delay one timer is given: 2³¹ − 1 ms (about 24.8 days).
 * Browsers fire a longer `setTimeout` at once, so a later renewal is armed
 * in steps of this until it is due.
 */
export const MAX_TIMER_DELAY = 2 ** 31 - 1;

/** What the app reads and does; `changed` runs on every change of `asking` or `error`. */
export type ShellTokens = {
  /** A provider call is in flight or waiting on its backoff. */
  readonly asking: boolean;
  /** Why the last call gave no token, or why the provider is no longer asked. */
  readonly error: string | null;
  /** Why the last renewal failed while the session ran on, until dismissed or the next token. */
  readonly notice: string | null;
  dismissNotice(): void;
  /** The person's Connect: asks at once, with a fresh streak. */
  ask(): void;
  dispose(): void;
};

/** What the driver asks and hands over: a provider's tokens, or a bridge's sessions. */
type Source<T> = {
  /** One call; `renew` only for the renewal before expiry. */
  readonly call: (renew: boolean) => unknown;
  /** The call's value, or why it is not one (a refused value is closed here). */
  readonly read: (value: unknown) => { readonly value: T; readonly expiresInMs?: number } | string;
  /** Gives the connection `value`. */
  readonly hand: (value: T) => void;
  /** A value nobody will use, because the app is gone. */
  readonly discard: (value: T) => void;
  /** The message for a rejection without one. */
  readonly refused: string;
};

/** The provider's rejection as the person's message: Tauri rejects with the command's error string. */
function reason(error: unknown, fallback: string): string {
  const message = typeof error === "string" ? error : error instanceof Error ? error.message : "";
  return message === "" ? fallback : message;
}

/** A value's `expiresInMs`, or why `what` is refused for it. */
function lifetime(expiresInMs: unknown, what: string): { readonly expiresInMs?: number } | string {
  if (expiresInMs === undefined) return {};
  const valid = typeof expiresInMs === "number" && Number.isFinite(expiresInMs) && expiresInMs > 0;
  return valid ? { expiresInMs } : `The shell's ${what} has an invalid expiresInMs: it must be a positive, finite number of milliseconds.`;
}

/** The provider's value as a {@link ShellToken}, or why it is not one. */
function shellToken(value: unknown): { readonly value: string; readonly expiresInMs?: number } | string {
  if (typeof value !== "object" || value === null) return "The shell gave no token.";
  const { token, expiresInMs } = value as Record<string, unknown>;
  if (typeof token !== "string" || token === "") return "The shell gave no token.";
  const life = lifetime(expiresInMs, "token");
  return typeof life === "string" ? life : { value: token, ...life };
}

/** Whether `value` has the {@link BridgeSocket} methods the client calls. */
function isBridgeSocket(value: unknown): value is BridgeSocket {
  if (typeof value !== "object" || value === null) return false;
  const { send, close } = value as Record<string, unknown>;
  return typeof send === "function" && typeof close === "function";
}

/** The bridge's value as a {@link BridgeSession}, or why it is not one; a refused session's socket is closed. */
function bridgeSession(value: unknown): { readonly value: BridgeSocket; readonly expiresInMs?: number } | string {
  if (typeof value !== "object" || value === null) return "The shell gave no bridge session.";
  const { socket, expiresInMs } = value as Partial<Record<keyof BridgeSession, unknown>>;
  if (!isBridgeSocket(socket)) return "The shell gave no bridge session.";
  const life = lifetime(expiresInMs, "bridge session");
  if (typeof life !== "string") return { value: socket, ...life };
  socket.close(1000, "refused");
  return life;
}

/** Drives a shell's token provider on a token connection. */
export function driveTokenProvider(connection: WireConnection, provider: TokenProvider, clock: Clock, random: () => number, changed: () => void): ShellTokens {
  return driveShell(
    connection,
    { call: () => provider(), read: shellToken, hand: (token) => connection.setToken(token), discard: () => undefined, refused: "The shell could not provide a token." },
    clock,
    random,
    changed,
  );
}

/** Drives a shell's `openBridge` on a bridged connection: one call per session. */
export function driveBridge(connection: WireConnection, openBridge: OpenBridge, clock: Clock, random: () => number, changed: () => void): ShellTokens {
  return driveShell(
    connection,
    { call: (renew) => openBridge({ renew }), read: bridgeSession, hand: (socket) => connection.setSocket(socket), discard: (socket) => socket.close(1000, "disposed"), refused: "The shell could not open a session." },
    clock,
    random,
    changed,
  );
}

function driveShell<T>(connection: WireConnection, source: Source<T>, clock: Clock, random: () => number, changed: () => void): ShellTokens {
  let inFlight = false;
  let waiting: TimerHandle | null = null;
  let renewal: TimerHandle | null = null;
  let error: string | null = null;
  let notice: string | null = null;
  let disposed = false;
  let streak = 0;
  let acksAtStreak = 0;
  let liveSince: number | null = null;
  let previous = connection.state;
  /** True while a refusal moves the connection to NeedsToken: no server ended anything, so nothing re-mints. */
  let refusing = false;

  const cancel = (timer: TimerHandle | null): null => {
    if (timer !== null) clock.clearTimeout(timer);
    return null;
  };
  const running = (): boolean => connection.state !== "NeedsToken" && connection.state !== "Disconnected";
  /** Arms the renewal for `due` on the monotonic clock, in steps of at most {@link MAX_TIMER_DELAY}. */
  const renewAt = (due: number): void => {
    const delay = Math.max(0, due - clock.now());
    renewal = clock.setTimeout(() => {
      renewal = null;
      if (delay > MAX_TIMER_DELAY) renewAt(due);
      else if (running()) call(true);
    }, Math.min(delay, MAX_TIMER_DELAY));
  };
  /**
   * Hands `value` over: from NeedsToken or Disconnected, or to an attempt
   * awaiting a session, handing it is enough; a running session reconnects on it.
   */
  const adopt = ({ value, expiresInMs }: { readonly value: T; readonly expiresInMs?: number }): void => {
    const reconnect = running() && !connection.awaitingSession;
    if (reconnect) connection.disconnect();
    source.hand(value);
    if (reconnect) connection.connect();
    renewal = cancel(renewal);
    if (expiresInMs !== undefined) renewAt(clock.now() + Math.floor(expiresInMs * RENEW_AT));
  };
  /** One provider call; a renewal's failure is a notice, since the session runs on. */
  const call = (renewing = false): void => {
    if (inFlight || disposed) return;
    waiting = null;
    inFlight = true;
    if (!renewing) error = null;
    changed();
    const settle = (token: { readonly value: T; readonly expiresInMs?: number } | null, failure: string | null): void => {
      inFlight = false;
      if (disposed) {
        if (token !== null) source.discard(token.value);
        return;
      }
      if (renewing && failure !== null) notice = failure;
      else error = failure;
      if (failure !== null && !renewing) {
        refusing = true;
        connection.sessionRefused();
        refusing = false;
      }
      if (token !== null) {
        notice = null;
        adopt(token);
      }
      changed();
    };
    Promise.resolve(renewing)
      .then(source.call)
      .then(
        (value: unknown) => {
          const token = source.read(value);
          if (typeof token === "string") settle(null, token);
          else settle(token, null);
        },
        (failure: unknown) => settle(null, reason(failure, source.refused)),
      );
  };
  const request = (delay: number): void => {
    if (inFlight || waiting !== null || disposed) return;
    if (delay === 0) call();
    else {
      waiting = clock.setTimeout(() => call(), delay);
      changed();
    }
  };
  const remint = (): void => {
    if (connection.acks > acksAtStreak) streak = 0;
    if (streak === 0) acksAtStreak = connection.acks;
    if (streak >= MAX_REMINTS) {
      error = REMINTS_SPENT;
      return changed();
    }
    streak++;
    request(streak === 1 ? 0 : backoffDelay(streak - 2, random));
  };

  const stop = connection.onTransition((state) => {
    if (state === "Live") liveSince = clock.now();
    else if (liveSince !== null) {
      if (clock.now() - liveSince >= DEADLINES.healthy) streak = 0;
      liveSince = null;
    }
    if (state === "Disconnected" && (waiting !== null || renewal !== null)) {
      waiting = cancel(waiting);
      renewal = cancel(renewal);
      changed();
    }
    // A bridged attempt with no session asks for one: the person's Connect starts a fresh streak.
    if (state === "Connecting" && connection.awaitingSession) {
      if (previous === "Disconnected") streak = 0;
      request(0);
    }
    if (state === "NeedsToken" && !refusing) {
      if (previous === "Disconnected") {
        streak = 0;
        request(0);
      } else remint();
    }
    previous = state;
  });

  return {
    get asking() {
      return inFlight || waiting !== null;
    },
    get error() {
      return error;
    },
    get notice() {
      return notice;
    },
    dismissNotice() {
      notice = null;
      changed();
    },
    ask() {
      waiting = cancel(waiting);
      streak = 0;
      error = null;
      request(0);
      changed();
    },
    dispose() {
      disposed = true;
      stop();
      waiting = cancel(waiting);
      renewal = cancel(renewal);
    },
  };
}
