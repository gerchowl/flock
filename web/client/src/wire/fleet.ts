/**
 * The wire {@link FleetSource}: S7-min's fleet-lite, the `fleet` snapshot
 * stream (ADR-0036 §5, #1746), parsed into {@link FleetChange}s for the fleet
 * store (`store/fleet.ts`, ADR-0038 §3), which owns the entities.
 *
 * Every `ev` on `fleet`, snapshot line or later delta, carries one
 * {@link FleetDelta}: an agent, node or project upsert, or a remove of one
 * entity. This module parses and validates and holds no rows. A completed
 * snapshot is handed over whole, so an entity of any kind missing from it is
 * gone; deltas after it change one entity each. A malformed agent, node or
 * project delta ends the subscription with a visible failure, as a refusal
 * does. An unknown `type`, including the sender's lossless `unknown` wrapper
 * and future discriminators, is skipped. The connection advances its cursor
 * after every accepted callback, including skipped deltas, and after complete
 * snapshots.
 *
 * @module
 */
import type { FleetAgentRow } from "../gen/FleetAgentRow";
import type { FleetNodeRow } from "../gen/FleetNodeRow";
import type { FleetProjectRow } from "../gen/FleetProjectRow";
import type { Phase } from "../gen/Phase";
import type { WireEvent } from "../gen/WireEvent";
import type { AgentRow, FleetChange, FleetSource } from "../fleet";
import type { WireConnection } from "./connection";

/** Parse the generated fields presentation consumes; newer S4 axes stay opaque. */
type RowView = Pick<FleetAgentRow, "id" | "label" | "activity" | "waiting_on_human" | "node_id" | "project"> & { readonly state: { readonly phase?: unknown } };

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
const isNumber = (value: unknown): value is number => typeof value === "number";
const isStrings = (value: unknown): value is string[] => Array.isArray(value) && value.every((item) => typeof item === "string");

const isString = (value: unknown): value is string => typeof value === "string";
const isNullable = (value: unknown, is: (value: unknown) => boolean): boolean => value === null || is(value);

function isRow(row: unknown): row is RowView {
  return (
    isObject(row) &&
    isString(row["id"]) &&
    isString(row["label"]) &&
    isObject(row["state"]) &&
    (row["activity"] === undefined || isString(row["activity"])) &&
    isStrings(row["waiting_on_human"]) &&
    (row["node_id"] === undefined || isString(row["node_id"])) &&
    (row["project"] === undefined || isString(row["project"]))
  );
}

/** A node row: every field is present, with an explicit null where an observation is unknown. */
function isNodeRow(row: unknown): row is FleetNodeRow {
  return (
    isObject(row) &&
    isString(row["node_id"]) &&
    isString(row["name"]) &&
    typeof row["online"] === "boolean" &&
    isNullable(row["health"], isObject) &&
    isNullable(row["latency_ms"], isNumber) &&
    isNullable(row["last_ok"], isNumber) &&
    isNullable(row["version"], isString) &&
    isNullable(row["error"], (error) => isObject(error) && isString(error["code"]) && isString(error["message"])) &&
    isNullable(row["route"], isObject) &&
    isNullable(row["counts"], isObject) &&
    isNullable(row["unbound_configured_peers"], isNumber)
  );
}

function isProjectRow(row: unknown): row is FleetProjectRow {
  return isObject(row) && isString(row["id"]) && isString(row["node_id"]) && isString(row["name"]);
}

/** Read one delta as a change; skip unknown kinds (`"ignored"`), refuse a malformed one (`null`). */
export function fleetDelta(ev: WireEvent, tsMs?: number): FleetChange | "ignored" | null {
  if (!isObject(ev)) return null;
  const type = ev["type"];
  if (type === "fleet.upsert") return isRow(ev["row"]) ? { op: "upsert", kind: "agent", row: agentRow(ev["row"]) } : null;
  if (type === "fleet.remove") return isString(ev["id"]) ? { op: "remove", kind: "agent", id: ev["id"] } : null;
  if (type === "fleet.node.upsert") return isNodeRow(ev["row"]) ? { op: "upsert", kind: "node", row: ev["row"], ...(tsMs === undefined ? {} : { tsMs }) } : null;
  if (type === "fleet.node.remove") return isString(ev["node_id"]) ? { op: "remove", kind: "node", node_id: ev["node_id"] } : null;
  if (type === "fleet.project.upsert") return isProjectRow(ev["row"]) ? { op: "upsert", kind: "project", row: ev["row"] } : null;
  if (type === "fleet.project.remove") return isString(ev["id"]) && isString(ev["node_id"]) ? { op: "remove", kind: "project", node_id: ev["node_id"], id: ev["id"] } : null;
  if (!isString(type)) return null;
  // Includes the sender's lossless `unknown` wrapper and future discriminators.
  return "ignored";
}

function agentRow(row: RowView): AgentRow {
  const phase = row.state.phase;
  const state = typeof phase === "string" && Object.hasOwn(PHASES, phase) ? phase as Phase : "unknown";
  return {
    id: row.id,
    label: row.label,
    state,
    activity: row.activity ?? "",
    waitingOnHuman: row.waiting_on_human,
    ...(row.node_id === undefined ? {} : { nodeId: row.node_id }),
    ...(row.project === undefined ? {} : { project: row.project }),
  };
}

/** Creates a {@link FleetSource} whose subscriptions run on `connection`. */
export function createWireFleet(connection: WireConnection): FleetSource {
  return {
    subscribe(observer) {
      let open = true;
      let unsubscribe = (): void => undefined;
      const end = (message: string): void => {
        if (!open) return;
        open = false;
        unsubscribe();
        observer.failed(message);
      };
      /** The change `ev` carries, `undefined` when skipped; ends the subscription when `ev` is malformed. */
      const read = (ev: WireEvent, tsMs: number): FleetChange | undefined | null => {
        const delta = fleetDelta(ev, tsMs);
        if (delta === null) {
          end(`The fleet stream sent something that is not a fleet delta: ${JSON.stringify(ev).slice(0, 200)}`);
          return null;
        }
        return delta === "ignored" ? undefined : delta;
      };
      unsubscribe = connection.subscribe("fleet", {
        event: (_seq, ev, tsMs) => {
          if (!open) return;
          const change = read(ev, tsMs);
          if (change !== undefined && change !== null) observer.change(change);
        },
        snapshot: (lines) => {
          if (!open) return;
          const changes: FleetChange[] = [];
          for (const { ev, tsMs } of lines) {
            const change = read(ev, tsMs);
            if (change === null) return;
            if (change !== undefined) changes.push(change);
          }
          observer.snapshot(changes);
        },
        // `fleet` is a snapshot stream; the connection refuses a history gap on one.
        gap: () => undefined,
        synced: () => undefined,
        reset: () => {
          if (open) observer.reset();
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
