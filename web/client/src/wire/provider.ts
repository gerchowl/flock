/**
 * A shell's token provider, driven by the connection's NeedsToken
 * (ADR-0038 §6, "The shell config contract" and "Lifetime").
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
import type { ShellToken, TokenProvider } from "../config";
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

/** The provider's rejection as the person's message: Tauri rejects with the command's error string. */
function reason(error: unknown): string {
  const message = typeof error === "string" ? error : error instanceof Error ? error.message : "";
  return message === "" ? "The shell could not provide a token." : message;
}

/** The provider's value as a {@link ShellToken}, or why it is not one. */
function shellToken(value: unknown): ShellToken | string {
  if (typeof value !== "object" || value === null) return "The shell gave no token.";
  const { token, expiresInMs } = value as Record<string, unknown>;
  if (typeof token !== "string" || token === "") return "The shell gave no token.";
  if (expiresInMs === undefined) return { token };
  const valid = typeof expiresInMs === "number" && Number.isFinite(expiresInMs) && expiresInMs > 0;
  return valid ? { token, expiresInMs } : "The shell's token has an invalid expiresInMs: it must be a positive, finite number of milliseconds.";
}

export function driveTokenProvider(connection: WireConnection, provider: TokenProvider, clock: Clock, random: () => number, changed: () => void): ShellTokens {
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
  /** Hands `token` over: from NeedsToken or Disconnected `setToken` is enough; a running session reconnects on it. */
  const adopt = ({ token, expiresInMs }: ShellToken): void => {
    const reconnect = running();
    connection.setToken(token);
    if (reconnect) {
      connection.disconnect();
      connection.connect();
    }
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
    const settle = (token: ShellToken | null, failure: string | null): void => {
      inFlight = false;
      if (disposed) return;
      if (renewing && failure !== null) notice = failure;
      else error = failure;
      if (token !== null) {
        notice = null;
        adopt(token);
      }
      changed();
    };
    Promise.resolve()
      .then(provider)
      .then(
        (value: unknown) => {
          const token = shellToken(value);
          if (typeof token === "string") settle(null, token);
          else settle(token, null);
        },
        (failure: unknown) => settle(null, reason(failure)),
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
    if (state === "NeedsToken") {
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
