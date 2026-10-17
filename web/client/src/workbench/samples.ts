/**
 * Fixture mode's agent rows (ADR-0038 §10): the recorded session as one
 * agent, plus sample rows that show every status mark. Only `fixture` has a
 * recording; focusing another sample row shows the replay's refusal.
 *
 * @module
 */
import type { AgentRow } from "../fleet";

/** The sidebar rows of the owner preview. */
export const SAMPLE_AGENTS: readonly AgentRow[] = [
  { id: "fixture", label: "fixture", state: "needs_human", activity: "Waiting for permission to run Bash", waitingOnHuman: ["sample-permission"] },
  { id: "atlas", label: "atlas", state: "working", activity: "Running the wire script suite", waitingOnHuman: [] },
  { id: "kite", label: "kite", state: "blocked", activity: "Blocked: rate limit", waitingOnHuman: [] },
  { id: "wren", label: "wren", state: "finished", activity: "Turn finished", waitingOnHuman: [] },
];
