/**
 * The wire transport: G1's {@link Transport} over a {@link WireConnection}
 * (ADR-0038 §3–§4, G3a #1747), so the views run unchanged on live data.
 *
 * Each `subscribe` opens one `agent/<id>` subscription from the caller's
 * cursor. The connection owns reconnects, `lagged`, deadlines and re-`sub`s;
 * the observer sees only entries, one `synced`, a `reset` and `failed`:
 *
 * - `ev` → an entry. The server classifies agent events (ADR-0036 §8): a
 *   payload is either the explicit `UnknownEvent` wrapper, delivered as
 *   `unknown` with its `raw` and `shape` unchanged, or a known catalogue
 *   event, delivered as `known`.
 * - a history `gap` → a `gap` entry carrying the missing range;
 * - the first `synced` → `synced`; the barriers of later re-`sub`s are not
 *   repeated, as G1's contract allows only one;
 * - `cursor_ahead` → `reset`, after which entries restart from the stream's
 *   first retained seq;
 * - a refusal → `failed`, ending the subscription.
 *
 * @module
 */
import type { KnownSessionEvent } from "../gen/KnownSessionEvent";
import type { WireEvent } from "../gen/WireEvent";
import type { Cursor, SessionEntry, SessionObserver, SessionStream, Subscription, Transport } from "../transport";
import type { WireConnection } from "./connection";

/** Maps one agent-stream payload to an entry (see the module doc). */
export function sessionEntry(seq: number, ev: WireEvent): SessionEntry {
  if (typeof ev === "object" && ev !== null && !Array.isArray(ev) && ev["type"] === "unknown") {
    const { raw, shape } = ev as { raw?: unknown; shape?: unknown };
    if (typeof raw === "string" && typeof shape === "string") return { seq, kind: "unknown", unknown: { raw, shape } };
  }
  // The server classified this payload as a known catalogue event (ADR-0036 §8).
  return { seq, kind: "known", event: ev as KnownSessionEvent };
}

/** Creates a {@link Transport} whose subscriptions run on `connection`. */
export function createWireTransport(connection: WireConnection): Transport {
  return {
    subscribe(stream: SessionStream, after: Cursor, observer: SessionObserver): Subscription {
      let open = true;
      let synced = false;
      let unsubscribe = (): void => undefined;
      const close = (): void => {
        if (!open) return;
        open = false;
        unsubscribe();
      };
      unsubscribe = connection.subscribe(
        connection.nextSubId("t"),
        stream,
        {
          event: (seq, ev) => open && observer.entry(sessionEntry(seq, ev)),
          gap: (from, to) => open && observer.entry({ seq: to, kind: "gap", from, to }),
          synced: () => {
            if (!open || synced) return;
            synced = true;
            observer.synced();
          },
          // Agent streams are history streams; the connection refuses a snapshot on one.
          snapshot: () => undefined,
          reset: () => open && observer.reset?.(),
          refused: (refusal) => {
            if (!open) return;
            close();
            observer.failed({ message: refusal.message });
          },
        },
        after,
      );
      return { close };
    },
  };
}
