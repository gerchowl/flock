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
 *   §3's resume path. A failed renewal is not retried: the reactive path
 *   above takes over when the server ends the token. Without `expiresInMs`
 *   there is no timer.
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

/** What the app reads and does; `changed` runs on every change of `asking` or `error`. */
export type ShellTokens = {
  /** A provider call is in flight or waiting on its backoff. */
  readonly asking: boolean;
  /** Why the last call gave no token, or why the provider is no longer asked. */
  readonly error: string | null;
  /** The person's Connect: asks at once, with a fresh streak. */
  ask(): void;
  dispose(): void;
};

/** The provider's rejection as the person's message: Tauri rejects with the command's error string. */
function reason(error: unknown): string {
  const message = typeof error === "string" ? error : error instanceof Error ? error.message : "";
  return message === "" ? "The shell could not provide a token." : message;
}

/** The provider's value as a {@link ShellToken}, or null when it is not one. */
function shellToken(value: unknown): ShellToken | null {
  if (typeof value !== "object" || value === null) return null;
  const { token, expiresInMs } = value as Record<string, unknown>;
  if (typeof token !== "string" || token === "") return null;
  if (expiresInMs === undefined) return { token };
  return typeof expiresInMs === "number" && Number.isFinite(expiresInMs) && expiresInMs > 0 ? { token, expiresInMs } : null;
}

export function driveTokenProvider(connection: WireConnection, provider: TokenProvider, clock: Clock, random: () => number, changed: () => void): ShellTokens {
  let inFlight = false;
  let waiting: TimerHandle | null = null;
  let renewal: TimerHandle | null = null;
  let error: string | null = null;
  let disposed = false;
  let streak = 0;
  let acksAtStreak = 0;
  let liveSince: number | null = null;
  let previous = connection.state;

  const cancel = (timer: TimerHandle | null): null => {
    if (timer !== null) clock.clearTimeout(timer);
    return null;
  };
  /** Hands `token` over: from NeedsToken or Disconnected `setToken` is enough; a running session reconnects on it. */
  const adopt = ({ token, expiresInMs }: ShellToken): void => {
    const running = connection.state !== "NeedsToken" && connection.state !== "Disconnected";
    connection.setToken(token);
    if (running) {
      connection.disconnect();
      connection.connect();
    }
    renewal = cancel(renewal);
    if (expiresInMs !== undefined) renewal = clock.setTimeout(() => ((renewal = null), request(0)), Math.floor(expiresInMs * RENEW_AT));
  };
  const call = (): void => {
    waiting = null;
    inFlight = true;
    error = null;
    changed();
    const settle = (token: ShellToken | null, failure: string | null): void => {
      inFlight = false;
      if (disposed) return;
      error = failure;
      if (token !== null) adopt(token);
      changed();
    };
    Promise.resolve()
      .then(provider)
      .then(
        (value: unknown) => {
          const token = shellToken(value);
          settle(token, token === null ? "The shell gave no token." : null);
        },
        (failure: unknown) => settle(null, reason(failure)),
      );
  };
  const request = (delay: number): void => {
    if (inFlight || waiting !== null || disposed) return;
    if (delay === 0) call();
    else {
      waiting = clock.setTimeout(call, delay);
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
    if (state === "Disconnected" && waiting !== null) {
      waiting = cancel(waiting);
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
