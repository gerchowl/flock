/**
 * A shell's token provider, driven by the connection's NeedsToken
 * (ADR-0038 §6, "The shell config contract" and "Lifetime").
 *
 * - It is asked when the person connects (the initial connect included, and
 *   {@link ShellTokens.ask}, which NeedsToken offers instead of a prompt),
 *   and after a close 4403 revoked a working token. A token refused at
 *   `hello` (4401) is not asked for again: that would loop.
 * - One call at a time: a request while one is in flight, or waiting, is
 *   the same request.
 * - Its token goes to the connection with `setToken`, which connects only
 *   from NeedsToken; after a Disconnect it is kept for the next Connect.
 * - A rejection, a synchronous throw or a value that is not a non-empty
 *   string is NeedsToken with that message, and is not retried.
 * - **Bounded re-mints.** Consecutive 4403 re-mints back off on §3's
 *   reconnect backoff: the first is immediate, the next ones wait
 *   `backoffDelay(0…)`. After {@link MAX_REMINTS} in a row the provider is
 *   not asked again until the person asks. A session that stayed Live for
 *   §3's healthy period (the one that resets its backoff), or an `ack`ed
 *   command since the streak began, ends the streak.
 *
 * @module
 */
import type { TokenProvider } from "../config";
import { DEADLINES, type WireConnection } from "./connection";
import { backoffDelay, type Clock, type TimerHandle } from "./env";

/**
 * Consecutive 4403 re-mints before the provider is no longer asked: one at
 * once, then §3's backoff for attempts 0 to 5 (ceilings 0.5 s to 16 s). The
 * next wait would be the backoff's 30 s ceiling, so the shell stops instead.
 */
export const MAX_REMINTS = 7;

/** NeedsToken's reason once the re-mints are spent. */
export const REMINTS_SPENT = `The server ended ${MAX_REMINTS} tokens from this shell in a row. Connect to try again.`;

/** What the app reads and does; `changed` runs on every change of `asking` or `error`. */
export type ShellTokens = {
  /** A provider call is in flight or waiting on its backoff. */
  readonly asking: boolean;
  /** Why the last call gave no token, or why the provider is no longer asked. */
  readonly error: string | null;
  /** The person's Connect: asks once more, with a fresh streak. */
  ask(): void;
  dispose(): void;
};

/** The provider's rejection as the person's message: Tauri rejects with the command's error string. */
function reason(error: unknown): string {
  const message = typeof error === "string" ? error : error instanceof Error ? error.message : "";
  return message === "" ? "The shell could not provide a token." : message;
}

export function driveTokenProvider(connection: WireConnection, provider: TokenProvider, clock: Clock, random: () => number, changed: () => void): ShellTokens {
  let inFlight = false;
  let waiting: TimerHandle | null = null;
  let error: string | null = null;
  let disposed = false;
  let streak = 0;
  let ackedAtStreak = 0;
  let liveSince: number | null = null;
  let previous = connection.state;
  const acked = (): number => connection.commands().filter((entry) => entry.status === "acked").length;

  const call = (): void => {
    waiting = null;
    inFlight = true;
    error = null;
    changed();
    const settle = (token: string | null, failure: string | null): void => {
      inFlight = false;
      if (disposed) return;
      error = failure;
      if (token !== null) connection.setToken(token);
      changed();
    };
    Promise.resolve()
      .then(provider)
      .then(
        (token: unknown) => (typeof token === "string" && token !== "" ? settle(token, null) : settle(null, "The shell gave no token.")),
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
    if (acked() > ackedAtStreak) streak = 0;
    if (streak === 0) ackedAtStreak = acked();
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
      clock.clearTimeout(waiting);
      waiting = null;
      changed();
    }
    if (state === "NeedsToken") {
      if (previous === "Disconnected") {
        streak = 0;
        request(0);
      } else if (connection.revoked) remint();
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
      streak = 0;
      error = null;
      request(0);
      changed();
    },
    dispose() {
      disposed = true;
      stop();
      if (waiting !== null) clock.clearTimeout(waiting);
      waiting = null;
    },
  };
}
