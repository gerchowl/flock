/**
 * Fixture mode's agent rows (ADR-0038 §10): the recorded session as one
 * agent, plus sample rows that show every status mark. Only `fixture` has a
 * recording; focusing another sample row shows the replay's refusal.
 *
 * @module
 */
import type { AgentRow, FleetChange } from "../fleet";
import type { FleetNodeRow } from "../gen/FleetNodeRow";

/** The sidebar rows of the owner preview. */
export const SAMPLE_AGENTS: readonly AgentRow[] = [
  { id: "fixture", label: "fixture", state: "needs_human", activity: "Waiting for permission to run Bash", waitingOnHuman: ["sample-permission"], nodeId: "node-a", project: "/work/app" },
  { id: "atlas", label: "atlas", state: "working", activity: "Running the wire script suite", waitingOnHuman: [], nodeId: "node-a", project: "/work/app" },
  { id: "kite", label: "kite", state: "blocked", activity: "Blocked: rate limit", waitingOnHuman: [], nodeId: "node-a" },
  { id: "wren", label: "wren", state: "finished", activity: "Turn finished", waitingOnHuman: [], nodeId: "node-b", project: "/work/app" },
];

const NODE = { health: null, latency_ms: null, version: null, error: null, unbound_configured_peers: null } as const;
const ROUTE = { node: "node-b", name: "node-b", next_hop: "node-b", hops: 1 } as const;

/** When the preview's node rows were "sent"; their ages are read against it. */
const SENT = Date.now();

/** The owner preview's nodes and projects: a home node, and a remote one that is offline with stale counts. */
export const SAMPLE_ENTITIES: readonly FleetChange[] = [
  { op: "upsert", kind: "node", tsMs: SENT, row: { ...NODE, node_id: "node-a", name: "node-a", online: true, route: null, last_ok: SENT - 2_000, version: "1.0.0", counts: { total: 3, by_phase: { working: 1, needs_human: 1, blocked: 1 }, waiting_on_human: 1 } } satisfies FleetNodeRow },
  { op: "upsert", kind: "node", tsMs: SENT, row: { ...NODE, node_id: "node-b", name: "node-b", online: false, route: ROUTE, last_ok: SENT - 600_000, error: { code: "timeout", message: "No answer within the limit." }, counts: { total: 1, by_phase: { finished: 1 }, waiting_on_human: null } } satisfies FleetNodeRow },
  { op: "upsert", kind: "project", row: { id: "/work/app", node_id: "node-a", name: "app" } },
  { op: "upsert", kind: "project", row: { id: "/work/app", node_id: "node-b", name: "app" } },
];
