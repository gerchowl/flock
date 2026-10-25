/**
 * The `Transport` interface: how views receive a session's events
 * (ADR-0038 §3–§4, ADR-0036 §5).
 *
 * A caller subscribes to one stream from a cursor and receives that stream's
 * entries, already classified as known or unknown, in seq order. The caller
 * never sees a wire frame. Two adapters satisfy it:
 *
 * - `transport/fixture.ts` (G1) replays a recorded session from the shell
 *   configuration; it classifies the adapter's stored form itself.
 * - `wire/transport.ts` (G3a, #1747) maps ADR-0036 §5's `ev` frames on
 *   `agent/<id>` to {@link SessionEntry}: a known event as `known`, and the
 *   wire's `UnknownEvent { raw, shape }` as `unknown`. It owns everything
 *   §3 puts in the connection lifecycle (reconnects, `lagged`, `gap`,
 *   deadlines); callers only ever see entries, `synced`, `reset` and a
 *   final failure.
 *
 * Contract every adapter keeps:
 * - Delivery is asynchronous: no observer callback runs inside `subscribe`.
 * - Entries arrive in strictly increasing `seq`, each at most once, and only
 *   entries with `seq > after`, until a `reset`.
 * - `reset` (wire only) means the server lost the stream's state
 *   (`cursor_ahead`): the caller drops every entry it holds, and entries
 *   restart from the stream's first retained seq.
 * - `synced` is called at most once, after the entries that existed when the
 *   subscription was made; later entries may follow it.
 * - `failed` ends the subscription: no callback follows it.
 * - After `close()`, no callback runs.
 *
 * @module
 */
import type { KnownSessionEvent } from "./gen/KnownSessionEvent";
import type { UnknownInput } from "./unknown";

/** A session stream's name, as in ADR-0036 §5 (`agent/<id>`). */
export type SessionStream = `agent/${string}`;

/**
 * Position in a stream: the seq of the last entry the caller has applied.
 * `null` subscribes from the beginning.
 */
export type Cursor = number | null;

/**
 * One delivered entry of a session stream (client-side; not a wire frame).
 * A `gap` entry marks seqs `from`–`to` as no longer stored (ADR-0038 §3); its
 * `seq` is `to`, and the stream continues after it. `tsMs` is the `ev` frame's
 * server timestamp, where the adapter has one (the wire does; a recording does not).
 */
export type SessionEntry =
  | { readonly seq: number; readonly tsMs?: number; readonly kind: "known"; readonly event: KnownSessionEvent }
  | { readonly seq: number; readonly tsMs?: number; readonly kind: "unknown"; readonly unknown: UnknownInput }
  | { readonly seq: number; readonly kind: "gap"; readonly from: number; readonly to: number };

/** Why a subscription ended; shown to the user as-is. */
export type TransportFailure = { readonly message: string };

/** Callbacks for one subscription; see the module contract. */
export type SessionObserver = {
  readonly entry: (entry: SessionEntry) => void;
  readonly synced: () => void;
  readonly failed: (failure: TransportFailure) => void;
  readonly reset?: () => void;
};

/** A live subscription. `close()` is idempotent. */
export type Subscription = { readonly close: () => void };

/** A source of session streams; see the module contract. */
export interface Transport {
  subscribe(stream: SessionStream, after: Cursor, observer: SessionObserver): Subscription;
}
