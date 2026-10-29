/**
 * The wire {@link FleetSource}: S7-min's fleet-lite, the `fleet` snapshot
 * stream (ADR-0036 §5, #1746), mapped to presentation {@link AgentRow}s.
 *
 * Every `ev` on `fleet`, snapshot line or later delta, carries one
 * {@link FleetDelta}. A completed snapshot replaces every row, so an agent
 * missing from it is gone; deltas after it upsert or remove one row. A
 * malformed agent delta ends the subscription with a visible failure, as a
 * refusal does. Node/project and future kinds are deliberately ignored until
 * G4 renders them. The connection advances its cursor after every accepted
 * callback, including ignored deltas, and after complete snapshots.
 *
 * @module
 */
import type { FleetAgentRow } from "../gen/FleetAgentRow";
import type { FleetDelta } from "../gen/FleetDelta";
import type { Phase } from "../gen/Phase";
import type { WireEvent } from "../gen/WireEvent";
import type { AgentRow, FleetSource } from "../fleet";
import type { WireConnection } from "./connection";

/** Parse the generated fields presentation consumes; newer S4 axes stay opaque. */
type RowView = Pick<FleetAgentRow, "id" | "label" | "activity" | "waiting_on_human"> & { readonly state: { readonly phase?: unknown } };
type IgnoredKind = Exclude<FleetDelta, { type: "fleet.upsert" | "fleet.remove" }>["type"];
const IGNORED: Record<IgnoredKind, true> = {
  "fleet.node.upsert": true, "fleet.node.remove": true,
  "fleet.project.upsert": true, "fleet.project.remove": true,
};
type DeltaView = (Omit<Extract<FleetDelta, { type: "fleet.upsert" }>, "row"> & { readonly row: RowView }) | Extract<FleetDelta, { type: "fleet.remove" }>;

/** Typed against {@link Phase}, so a new state fails `tsc` until listed. */
const PHASES: { readonly [P in Phase]: true } = {
  gone: true,
  hibernated: true,
  offline: true,
  limit_reached: true,
  blocked: true,
  needs_human: true,
  working: true,
  waiting_on_event: true,
  finished_with_background: true,
  finished: true,
  settling: true,
  unknown: true,
};

type Json = { readonly [key: string]: unknown };
const isObject = (value: unknown): value is Json => typeof value === "object" && value !== null && !Array.isArray(value);
const isStrings = (value: unknown): value is string[] => Array.isArray(value) && value.every((item) => typeof item === "string");

function isRow(row: unknown): row is RowView {
  return (
    isObject(row) &&
    typeof row["id"] === "string" &&
    typeof row["label"] === "string" &&
    isObject(row["state"]) &&
    (row["activity"] === undefined || typeof row["activity"] === "string") &&
    isStrings(row["waiting_on_human"])
  );
}

/** Read agent deltas; skip typed entity and unknown kinds, refuse malformed agents. */
export function fleetDelta(ev: WireEvent): DeltaView | { readonly type: "ignored" } | null {
  if (!isObject(ev)) return null;
  if (ev["type"] === "fleet.upsert") return isRow(ev["row"]) ? { type: "fleet.upsert", row: ev["row"] } : null;
  if (ev["type"] === "fleet.remove") return typeof ev["id"] === "string" ? { type: "fleet.remove", id: ev["id"] } : null;
  if (typeof ev["type"] !== "string") return null;
  if (Object.hasOwn(IGNORED, ev["type"])) return { type: "ignored" };
  // Includes the sender's lossless `unknown` wrapper and future discriminators.
  return { type: "ignored" };
}

function agentRow(row: RowView): AgentRow {
  const phase = row.state.phase;
  const state = typeof phase === "string" && Object.hasOwn(PHASES, phase) ? phase as Phase : "unknown";
  return { id: row.id, label: row.label, state, activity: row.activity ?? "", waitingOnHuman: row.waiting_on_human };
}

/** Creates a {@link FleetSource} whose subscriptions run on `connection`. */
export function createWireFleet(connection: WireConnection): FleetSource {
  return {
    subscribe(observer) {
      let open = true;
      let unsubscribe = (): void => undefined;
      let rows = new Map<string, AgentRow>();
      const end = (message: string): void => {
        if (!open) return;
        open = false;
        unsubscribe();
        observer.failed(message);
      };
      /** Applies `ev` to `into`; false once the subscription has ended. */
      const apply = (into: Map<string, AgentRow>, ev: WireEvent): boolean => {
        const delta = fleetDelta(ev);
        if (delta === null) {
          end(`The fleet stream sent something that is not a fleet delta: ${JSON.stringify(ev).slice(0, 200)}`);
          return false;
        }
        if (delta.type === "ignored") return true;
        if (delta.type === "fleet.upsert") into.set(delta.row.id, agentRow(delta.row));
        else into.delete(delta.id);
        return true;
      };
      unsubscribe = connection.subscribe("fleet", {
        event: (_seq, ev) => open && apply(rows, ev) && observer.rows([...rows.values()]),
        snapshot: (lines) => {
          const fresh = new Map<string, AgentRow>();
          if (!open || !lines.every((ev) => apply(fresh, ev))) return;
          rows = fresh;
          observer.rows([...rows.values()]);
        },
        // `fleet` is a snapshot stream; the connection refuses a history gap on one.
        gap: () => undefined,
        synced: () => undefined,
        reset: () => {
          if (!open) return;
          rows = new Map();
          observer.rows([]);
        },
        refused: (refusal) => end(refusal.message),
      }).close;
      return () => {
        if (!open) return;
        open = false;
        unsubscribe();
      };
    },
  };
}
