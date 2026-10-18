/**
 * The live inbox of open permission prompts (ADR-0038 §1, §2a, §3; #1749
 * PR 1, read-only).
 *
 * The source of truth is fleet-lite's `waiting_on_human` list per agent: a
 * card exists exactly while its request id is listed, and goes only when the
 * server drops it. Details come from that agent's `agent/<id>` stream, from
 * the `control_request` event whose `request_id` matches and whose
 * `request.subtype` is `can_use_tool`. A listed id whose details have not
 * arrived shows a loading card, and one the stream never matches says so; a
 * listed id is never hidden. An event with no listed id shows nothing.
 *
 * One subscription per agent with listed ids, opened when its first id is
 * listed and closed when its list empties. The focused transcript tile's own
 * subscription is independent.
 *
 * Order: every permission prompt blocks its agent, so all cards block. Agents
 * sort by when each one's list first became non-empty (oldest first); within
 * an agent, cards sort by the matched event's stream seq, and ids still
 * without details follow, in the server's listed order.
 *
 * @module
 */
import { createSignal, type Accessor } from "solid-js";

import type { AgentRow } from "../fleet";
import type { Subscription, Transport } from "../transport";
import { commandOf } from "../views/turns";
import type { InboxCard } from "./inbox";

/** The prompt's tool and input summary, from its `control_request` event. */
type Details = { readonly seq: number; readonly tool: string; readonly summary: string };

type Tracked = {
  label: string;
  ids: readonly string[];
  /** Orders agents by age: assigned when the list first became non-empty. */
  readonly listedAt: number;
  readonly details: Map<string, Details>;
  synced: boolean;
  failure: string | null;
  subscription: Subscription | null;
};

const CHOICES = ["Allow once", "Always allow", "Deny"] as const;
const WAITS_FOR = "#1749 PR 2 (answering prompts)";

/** The live permission inbox over `transport`. */
export type PermissionInbox = {
  readonly cards: Accessor<readonly InboxCard[]>;
  /** Applies the fleet's current rows: opens and closes agent subscriptions. */
  readonly track: (rows: readonly AgentRow[]) => void;
  /** Closes every subscription. */
  readonly close: () => void;
};

/** Creates the inbox; call {@link PermissionInbox.track} with each fleet update. */
export function createPermissionInbox(transport: Transport): PermissionInbox {
  const [cards, setCards] = createSignal<readonly InboxCard[]>([]);
  const agents = new Map<string, Tracked>();
  let listed = 0;
  let closed = false;

  const card = (agent: string, tracked: Tracked, id: string): InboxCard => {
    const found = tracked.details.get(id);
    const pending =
      tracked.failure !== null
        ? `Details unavailable: ${tracked.failure}`
        : tracked.synced
          ? "This agent's history has no matching permission request."
          : "Loading details…";
    return {
      id,
      kind: "permission",
      agent,
      agentLabel: tracked.label,
      title: found?.tool ?? `${tracked.label} · permission ${id}`,
      detail: found?.summary ?? pending,
      choices: CHOICES,
      blocking: true,
      waitsFor: WAITS_FOR,
    };
  };

  const publish = (): void => {
    const ordered = [...agents.entries()].sort(([, a], [, b]) => a.listedAt - b.listedAt);
    setCards(
      ordered.flatMap(([agent, tracked]) => {
        const rank = (id: string): number => tracked.details.get(id)?.seq ?? Number.POSITIVE_INFINITY;
        // `sort` is stable, so unmatched ids keep the server's listed order.
        return [...tracked.ids].sort((a, b) => rank(a) - rank(b)).map((id) => card(agent, tracked, id));
      }),
    );
  };

  const open = (agent: string, tracked: Tracked): void => {
    tracked.subscription = transport.subscribe(`agent/${agent}`, null, {
      entry: (entry) => {
        if (entry.kind !== "known" || entry.event.type !== "control_request") return;
        const { request_id: id, request } = entry.event;
        if (request?.subtype !== "can_use_tool") return;
        tracked.details.set(id, { seq: entry.seq, tool: request.display_name ?? request.tool_name, summary: commandOf(request.input) });
        publish();
      },
      synced: () => {
        tracked.synced = true;
        publish();
      },
      failed: (failure) => {
        tracked.failure = failure.message;
        tracked.subscription = null;
        publish();
      },
      reset: () => {
        tracked.details.clear();
        tracked.synced = false;
        publish();
      },
    });
  };

  return {
    cards,
    track: (rows) => {
      if (closed) return;
      const waiting = new Map(rows.filter((row) => row.waitingOnHuman.length > 0).map((row) => [row.id, row]));
      for (const [agent, tracked] of agents) {
        if (waiting.has(agent)) continue;
        tracked.subscription?.close();
        agents.delete(agent);
      }
      for (const [agent, row] of waiting) {
        const tracked = agents.get(agent);
        if (tracked !== undefined) {
          tracked.label = row.label;
          tracked.ids = row.waitingOnHuman;
          continue;
        }
        const fresh: Tracked = { label: row.label, ids: row.waitingOnHuman, listedAt: listed++, details: new Map(), synced: false, failure: null, subscription: null };
        agents.set(agent, fresh);
        open(agent, fresh);
      }
      publish();
    },
    close: () => {
      closed = true;
      for (const tracked of agents.values()) tracked.subscription?.close();
      agents.clear();
      setCards([]);
    },
  };
}
