/**
 * The fleet-lite seam: where the sidebar's agent rows come from
 * (ADR-0038 §2a, S7-min #1746).
 *
 * {@link AgentRow} is a **presentation** type, not a wire shape: the wire
 * source (`wire/fleet.ts`) maps S7-min's fleet-lite rows to it, and the
 * fixture-backed source below serves tests and fixture mode.
 *
 * @module
 */
import type { Phase } from "./gen/Phase";

/** One sidebar row: what fleet-lite carries for a structured agent (#1746). */
export type AgentRow = {
  /** The agent id, as in `agent/<id>`. */
  readonly id: string;
  /** Short label for the row. */
  readonly label: string;
  /** The agent's S4 state summary (#1427), as the server reports it. */
  readonly state: Phase;
  /** The agent's current activity text, as peer summaries carry it (#1730). */
  readonly activity: string;
  /** Open permission-request ids waiting on a human (S6-MVP #1429). */
  readonly waitingOnHuman: readonly string[];
};

/** Receives the full row list each time it changes, or why the list ended. */
export type FleetObserver = {
  rows(rows: readonly AgentRow[]): void;
  /** The source ended visibly; no call follows. */
  failed(message: string): void;
};

/** A source of agent rows. `subscribe` delivers asynchronously; the returned function unsubscribes. */
export interface FleetSource {
  subscribe(observer: FleetObserver): () => void;
}

/** A fixed list of rows, delivered once after `subscribe` returns. */
export function createFixtureFleet(rows: readonly AgentRow[]): FleetSource {
  return {
    subscribe(observer) {
      let open = true;
      queueMicrotask(() => open && observer.rows(rows));
      return () => {
        open = false;
      };
    },
  };
}
