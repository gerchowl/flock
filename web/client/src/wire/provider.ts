/**
 * A shell's token provider, or its bridge, driven by the connection's
 * NeedsToken (ADR-0038 §6, "The shell config contract" and "Lifetime").
 *
 * **One state machine, two sources.** A bridge's `openBridge` (#2500) is
 * driven by the same rules as a token provider: each call is one session
 * instead of one token, handed over with `setSocket` instead of `setToken`.
 * Three things differ. A bridged session is used by one attempt, so every
 * reconnect also asks (`awaitingSession`, outside the streak, which counts
 * only the server's ends), and a refusal then takes the connection to
 * NeedsToken. And the renewal's due time belongs to the bearer, not the
 * session: a reconnect's session keeps the native side's bearer, so its
 * `expiresInMs` can only bring the due time earlier, while a new bearer (a
 * re-mint after an end, a person's Connect from NeedsToken, the renewal, or
 * a session the native side marks `fresh`) replaces it.
 * Only the renewal passes `renew: true`. Below, "token" reads "session" for
 * a bridge.
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
 *   It is cleared on Disconnect. A late result's deadline waits without a timer
 *   until Connect resumes it. If a user Disconnect/Connect invalidates an
 *   in-flight renewal, token mode reuses the old bearer without a new lifetime
 *   report; its renewal is reactive on an auth end. Bridge mode derives renewal
 *   from the fresh open's reported remaining lifetime, never the discarded
 *   result. It renews only a running session: in
 *   NeedsToken (a spent cap included) the reactive path owns the provider.
 *   A failed renewal is not retried; the session stays Live, the shell's
 *   message is {@link ShellTokens.notice}, and the reactive path takes over
 *   when the server ends the token. Without `expiresInMs` there is no timer.
 * - One call at a time: a request while one is in flight, or waiting, is
 *   the same request. A request skipped for the wait or call in flight (a re-mint
 *   after an end, or a bridged reconnect's session) is owed: when the call
 *   settles and the state still needs it, a value counts as that call, and a
 *   failure issues it on its own streak delay. A request coalesced with a
 *   wait keeps that wait's due time even if renewal cancels it. There is one
 *   owed slot: the latest request kind wins, its due time is the maximum of
 *   all coalesced waits, and `stillNeeded` decides whether it is served.
 * - Its token goes to the connection with `setToken`, which connects only
 *   from NeedsToken; after a Disconnect it is kept for the next Connect.
 * - A rejection, a synchronous throw or a value that is not a
 *   `{ token, expiresInMs? }` is NeedsToken with that message, and is not
 *   retried.
 * - **Bounded re-mints.** Consecutive re-mints back off on §3's reconnect
 *   backoff: the first is immediate, the next ones wait `backoffDelay(0…)`.
 *   After {@link MAX_REMINTS} in a row, pending waits and owed work are cleared:
 *   unresolved results are discarded until the person asks. Only a user Connect
 *   starts a fresh streak; internal renewal handoffs preserve it. A session that stayed
 *   Live for §3's healthy period (the one that resets its backoff), or an
 *   `ack`ed command since the streak began, ends the streak.
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
  readonly read: (value: unknown) => { readonly value: T; readonly expiresInMs?: number; readonly fresh?: boolean } | string;
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
function bridgeSession(value: unknown): { readonly value: BridgeSocket; readonly expiresInMs?: number; readonly fresh?: boolean } | string {
  if (typeof value !== "object" || value === null) return "The shell gave no bridge session.";
  const { socket, expiresInMs, fresh } = value as Partial<Record<keyof BridgeSession, unknown>>;
  if (!isBridgeSocket(socket)) return "The shell gave no bridge session.";
  const life = lifetime(expiresInMs, "bridge session");
  const problem = typeof life === "string" ? life : fresh !== undefined && typeof fresh !== "boolean" ? "The shell's bridge session has an invalid fresh: it must be a boolean." : null;
  if (problem === null) return { value: socket, ...(life as { readonly expiresInMs?: number }), ...(fresh === true ? { fresh } : {}) };
  socket.close(1000, "refused");
  return problem;
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
  let waiting: { readonly timer: TimerHandle; readonly due: number } | null = null;
  let renewal: TimerHandle | null = null;
  /** The current bearer's renewal time on the monotonic clock, or null for none. */
  let due: number | null = null;
  /** The current bearer's renewal was asked for: it is not asked again, even when a reconnect reports its lifetime. */
  let spent = false;
  /** One slot, merged by owe; stillNeeded decides at settlement. */
  let owed: { readonly kind: "mint" | "reconnect"; readonly due: number } | null = null;
  /** Latest kind wins; neither a newer request nor cancellation shortens a wait. */
  const owe = (kind: "mint" | "reconnect", at: number): void => {
    owed = { kind, due: Math.max(at, waiting?.due ?? at, owed?.due ?? at) };
  };
  let error: string | null = null;
  let notice: string | null = null;
  let disposed = false;
  /** A cap or explicit Connect invalidates calls admitted in an older epoch. */
  let capEpoch = 0;
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
  const cancelWaiting = (): null => {
    if (waiting !== null) clock.clearTimeout(waiting.timer);
    return null;
  };
  const running = (): boolean => connection.state !== "NeedsToken" && connection.state !== "Disconnected";
  /** Forgets the renewal: Disconnect, dispose and a spent re-mint cap. */
  const forget = (): void => {
    renewal = cancel(renewal);
    due = null;
    spent = false;
  };
  /** Arms the renewal for `at` on the monotonic clock, in steps of at most {@link MAX_TIMER_DELAY}. */
  const renewAt = (at: number): void => {
    const delay = Math.max(0, at - clock.now());
    renewal = clock.setTimeout(() => {
      renewal = null;
      if (delay > MAX_TIMER_DELAY) renewAt(at);
      else if (running()) call("renew");
    }, Math.min(delay, MAX_TIMER_DELAY));
  };
  /**
   * Hands `value` over: from NeedsToken or Disconnected, or to an attempt
   * awaiting a session, handing it is enough; a running session reconnects
   * on it. `kept` says the value runs on the bearer already due, so its
   * lifetime can only bring the renewal earlier.
   */
  const adopt = ({ value, expiresInMs }: { readonly value: T; readonly expiresInMs?: number; readonly fresh?: boolean }, kept: boolean): void => {
    const before = kept ? { due, spent } : { due: null, spent: false };
    const reconnect = running() && !connection.awaitingSession;
    if (reconnect) connection.replaceSession(() => source.hand(value));
    else source.hand(value);
    renewal = cancel(renewal);
    ({ due, spent } = before);
    if (expiresInMs !== undefined && !spent) {
      const at = clock.now() + Math.floor(expiresInMs * RENEW_AT);
      due = due === null ? at : Math.min(due, at);
    }
    // A late result may be kept for the next Connect, but Disconnect owns no timers.
    if (due !== null && !spent && running()) renewAt(due);
  };
  /**
   * One provider call: a `renew`al, a `mint` (a new bearer: the connect from
   * NeedsToken or Disconnected, and every re-mint), or a bridged
   * `reconnect`'s session on the bearer the native side keeps. A renewal's
   * failure is a notice, since the session runs on.
   */
  const call = (kind: "renew" | "mint" | "reconnect"): void => {
    if (inFlight || disposed) return;
    const renewing = kind === "renew";
    if (waiting !== null) {
      // Only renewal interrupts a wait: request queues competitors, timer callbacks
      // clear waiting first. running excludes NeedsToken, so the old mint is obsolete.
      // Any later demand enters owed through request, already keeping the max due.
      waiting = cancelWaiting();
    }
    inFlight = true;
    const admittedEpoch = capEpoch;
    if (renewing) spent = true;
    else error = null;
    changed();
    const settle = (token: { readonly value: T; readonly expiresInMs?: number; readonly fresh?: boolean } | null, failure: string | null): void => {
      inFlight = false;
      const skipped = owed;
      owed = null;
      const resumeOwed = (): void => {
        if (skipped !== null && stillNeeded(skipped.kind)) request(Math.max(0, skipped.due - clock.now()), skipped.kind);
      };
      // Discard only this call's obsolete result, preserving a later Connect's work.
      if (disposed || admittedEpoch !== capEpoch) {
        if (token !== null) source.discard(token.value);
        if (!disposed) {
          resumeOwed();
          changed();
        }
        return;
      }
      if (renewing && failure !== null) notice = failure;
      else error = failure;
      if (failure !== null && !renewing) {
        refusing = true;
        connection.sessionRefused();
        refusing = false;
      }
      // A session the native side marks fresh runs on a bearer it minted for this open.
      const kept = kind === "reconnect" && token?.fresh !== true;
      if (token !== null) {
        // A failed renewal's notice stands until a new bearer: a reconnect's session runs on the same one.
        if (!kept) notice = null;
        adopt(token, kept);
      } else if (renewing) resumeOwed();
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
  /** Whether the state still needs a skipped request: a reconnect waiting for its session, or a re-mint in NeedsToken. */
  const stillNeeded = (kind: "mint" | "reconnect"): boolean => (kind === "reconnect" ? connection.awaitingSession : connection.state === "NeedsToken" && error === null);
  const request = (delay: number, kind: "mint" | "reconnect"): void => {
    if (disposed) return;
    // A renewal can run before a waiting mint; it must inherit this request too.
    if (inFlight || waiting !== null) {
      owe(kind, clock.now() + delay);
      return;
    }
    if (delay === 0) call(kind);
    else {
      const due = clock.now() + delay;
      const timer = clock.setTimeout(() => {
        waiting = null;
        call(kind);
      }, delay);
      waiting = { timer, due };
      changed();
    }
  };
  const remint = (): void => {
    if (connection.acks > acksAtStreak) streak = 0;
    if (streak === 0) acksAtStreak = connection.acks;
    if (streak >= MAX_REMINTS) {
      capEpoch++;
      waiting = cancelWaiting();
      owed = null;
      error = REMINTS_SPENT;
      forget();
      return changed();
    }
    streak++;
    request(streak === 1 ? 0 : backoffDelay(streak - 2, random), "mint");
  };

  const freshEpoch = (): void => {
    capEpoch++;
    streak = 0;
    error = null;
  };
  const stopUserConnect = connection.onUserConnect(freshEpoch);
  const stop = connection.onTransition((state) => {
    // A late result keeps an absolute deadline without owning a timer while
    // Disconnected. Connect resumes it, immediately if it is already overdue.
    if (previous === "Disconnected" && running() && due !== null && !spent && renewal === null) renewAt(due);
    if (state === "Live") liveSince = clock.now();
    else if (liveSince !== null) {
      if (clock.now() - liveSince >= DEADLINES.healthy) streak = 0;
      liveSince = null;
    }
    if (state === "Disconnected") {
      const armed = waiting !== null || renewal !== null;
      waiting = cancelWaiting();
      owed = null;
      forget();
      if (armed) changed();
    }
    // A bridged attempt with no session asks for one: the person's Connect starts a fresh streak.
    if (state === "Connecting" && connection.awaitingSession) {
      request(0, "reconnect");
    }
    if (state === "NeedsToken" && !refusing) {
      if (previous === "Disconnected") {
        request(0, "mint");
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
      waiting = cancelWaiting();
      freshEpoch();
      request(0, "mint");
      changed();
    },
    dispose() {
      disposed = true;
      stopUserConnect();
      stop();
      waiting = cancelWaiting();
      // Release the record; the disposed guard already makes this work unobservable.
      owed = null;
      forget();
    },
  };
}
