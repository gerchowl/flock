/** Attention is projected only from structured fleet rows and typed server phases. */
import type { AgentRow } from "../fleet";
import type { InboxCard } from "./inbox";

/** Count each agent once, even when it has several open requests. */
export function needsYou(row: AgentRow): boolean {
  return row.waitingOnHuman.length > 0 || row.state === "blocked" || row.state === "limit_reached";
}

/** State-only attention cards have no permission choices to invent. */
export function attentionCards(rows: readonly AgentRow[]): InboxCard[] {
  return rows.filter((row) => row.state === "blocked" || row.state === "limit_reached").map((row) => ({
    id: `attention/${row.id}`,
    kind: "attention",
    agent: row.id,
    agentLabel: row.label,
    title: row.state === "limit_reached" ? "Agent reached a limit" : "Agent is blocked",
    detail: row.activity,
    choices: [],
    blocking: true,
  }));
}
