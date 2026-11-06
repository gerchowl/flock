/** GUI-P0 counts are server-owned waiting_on_human totals, never agent-row estimates. */
import type { FleetNodeRow } from "../gen/FleetNodeRow";

export type Waiting = {
  readonly total: number;
  readonly token: string;
  readonly text: string;
  readonly description: string;
  readonly title: string;
  readonly badgeLabel: string | null;
};

/** Offline inventories are retained observations, not a complete current count. */
export function waitingOnYou(nodes: readonly FleetNodeRow[]): Waiting {
  let known = 0;
  let total = 0;
  for (const node of nodes) {
    const count = node.counts?.waiting_on_human;
    if (node.online && count != null) { known++; total += count; }
  }
  const unknown = nodes.length - known;
  const token = known === 0 ? "?" : unknown > 0 ? `${total}+` : total === 0 ? "" : `${total}`;
  const text = token === "" ? "Agents waiting on you" : `${token} ${token === "1" ? "agent" : "agents"} waiting on you`;
  const missing = nodes.length === 0 ? "no nodes reporting" : `${unknown} ${unknown === 1 ? "node" : "nodes"} not reporting`;
  const description = known === 0 ? `agents waiting: unknown; ${missing}` : unknown > 0 ? `${total} or more agents waiting; ${missing}` : total === 0 ? "No agents waiting on you" : text;
  return { total, token, text, description, title: token === "" ? "flock" : `(${token}) flock`, badgeLabel: known === 0 || unknown > 0 ? token : null };
}
