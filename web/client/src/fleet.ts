/**
 * The fleet-lite seam: where the sidebar's agent rows come from
 * (ADR-0038 §2a, S7-min #1746).
 *
 * {@link AgentRow} is a **presentation** type, not a wire shape: the wire
 * source (`wire/fleet.ts`) maps S7-min's fleet-lite rows to it, and the
 * fixture-backed source below serves tests and fixture mode. A source holds
 * no rows: it delivers {@link FleetChange}s to the fleet store
 * (`store/fleet.ts`, ADR-0038 §3), which owns the entities of every kind.
 *
 * @module
 */
import type { FleetNodeRow } from "./gen/FleetNodeRow";
import type { FleetProjectRow } from "./gen/FleetProjectRow";
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
  /** The owning node, as the server sent it; absent in legacy or not-yet-bound rows (ADR-0038 §9). */
  readonly nodeId?: string;
  /** The exact project key (the agent's cwd), as the server sent it; absent in legacy rows. */
  readonly project?: string;
};

/**
 * One change to one entity of the fleet. A node is keyed by `node_id`, an
 * agent by `id`, and a project by (`node_id`, `id`): its `id` is a cwd on one
 * owning node.
 */
export type FleetChange =
  | { readonly op: "upsert"; readonly kind: "agent"; readonly row: AgentRow }
  | { readonly op: "upsert"; readonly kind: "node"; readonly row: FleetNodeRow }
  | { readonly op: "upsert"; readonly kind: "project"; readonly row: FleetProjectRow }
  | { readonly op: "remove"; readonly kind: "agent"; readonly id: string }
  | { readonly op: "remove"; readonly kind: "node"; readonly node_id: string }
  | { readonly op: "remove"; readonly kind: "project"; readonly node_id: string; readonly id: string };

/** What a source tells the fleet store; the store applies it and every reader sees the result. */
export type FleetObserver = {
  /** A complete agent list, for a source that carries only agents: every other kind is emptied. */
  rows(rows: readonly AgentRow[]): void;
  /** A complete snapshot: an entity of any kind it omits is gone. */
  snapshot(changes: readonly FleetChange[]): void;
  /** One later change. */
  change(change: FleetChange): void;
  /** The stream restarts: every kind is emptied until the next snapshot. */
  reset(): void;
  /** The source ended visibly; no call follows. */
  failed(message: string): void;
};

/** A source of fleet entities. `subscribe` delivers asynchronously; the returned function unsubscribes. */
export interface FleetSource {
  subscribe(observer: FleetObserver): () => void;
}

/** A fixed list of rows, and optionally nodes and projects, delivered once after `subscribe` returns. */
export function createFixtureFleet(rows: readonly AgentRow[], entities: readonly FleetChange[] = []): FleetSource {
  return {
    subscribe(observer) {
      let open = true;
      queueMicrotask(() => {
        if (!open) return;
        if (entities.length === 0) observer.rows(rows);
        else observer.snapshot([...rows.map((row): FleetChange => ({ op: "upsert", kind: "agent", row })), ...entities]);
      });
      return () => {
        open = false;
      };
    },
  };
}
