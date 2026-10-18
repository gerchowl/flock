/**
 * The live inbox of open permission prompts (ADR-0038 §1, §2a, §3; #1749
 * PR 1, read-only).
 *
 * The card set is fleet-lite's `waiting_on_human`, the server's answer to
 * "what waits on a human". An id that is not listed never shows a card, even
 * when its stream has an open record: the server opens a record for an
 * automatic request (one a session grant pre-approves) too, with no flag, and
 * that card would be one nobody can act on. A listed id's details and
 * `request_seq` come from the server's permission record on `agent/<id>`
 * (`wire/permission-record.ts`, #2224), and a listed id with no open record
 * yet shows a loading card (or, once the stream has synced without one, says
 * so); a listed id is never hidden. A card leaves on whichever server signal
 * arrives first: the closed record with its `request_id`, or the id dropping
 * out of `waiting_on_human`. A closed id does not come back as pending while
 * the fleet still lists it; a closed record for an unknown id is ignored. An
 * open record for an unlisted id waits: if the request turns manual, the id
 * is listed and its card shows.
 *
 * One subscription per agent with listed ids, opened when its first id is
 * listed and closed when its list empties. The focused transcript tile's own
 * subscription is independent.
 *
 * Order: every permission prompt blocks its agent, so all cards block. Agents
 * sort by when each one's list first became non-empty (oldest first); within
 * an agent, cards sort by the open record's stream seq, and listed ids still
 * without a record follow, in the server's listed order.
 *
 * @module
 */
import { createSignal, type Accessor } from "solid-js";

import type { AgentRow } from "../fleet";
import type { Subscription, Transport } from "../transport";
import { permissionRecord, type OpenPermissionRecord } from "../wire/permission-record";
import { commandOf } from "../views/turns";
import type { InboxCard } from "./inbox";

/** An open record, with the stream seq it arrived at. */
type Open = { readonly seq: number; readonly record: OpenPermissionRecord };

type Tracked = {
  label: string;
  ids: readonly string[];
  /** Orders agents by age: assigned when the list first became non-empty. */
  readonly listedAt: number;
  /** Open records by `request_id`; a closed record removes its entry. */
  readonly open: Map<string, Open>;
  /** Ids whose closed record arrived: not shown as pending while the fleet still lists them. */
  readonly closed: Set<string>;
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

  const card = (agent: string, tracked: Tracked, id: string, found: Open | undefined): InboxCard => {
    const pending =
      tracked.failure !== null
        ? `Details unavailable: ${tracked.failure}`
        : tracked.synced
          ? "This agent's stream has no open permission record for it."
          : "Loading details…";
    return {
      id,
      kind: "permission",
      agent,
      agentLabel: tracked.label,
      title: found === undefined ? `${tracked.label} · permission ${id}` : (found.record.request.display_name ?? found.record.request.tool_name),
      detail: found === undefined ? pending : commandOf(found.record.request.input),
      choices: CHOICES,
      blocking: true,
      waitsFor: WAITS_FOR,
      ...(found === undefined ? {} : { requestSeq: found.record.request_seq }),
    };
  };

  const publish = (): void => {
    const ordered = [...agents.entries()].sort(([, a], [, b]) => a.listedAt - b.listedAt);
    setCards(
      ordered.flatMap(([agent, tracked]) => {
        const shown = tracked.ids.filter((id) => tracked.open.has(id) || !tracked.closed.has(id));
        const rank = (id: string): number => tracked.open.get(id)?.seq ?? Number.POSITIVE_INFINITY;
        // `sort` is stable, so ids without a record keep the server's listed order.
        return [...shown].sort((a, b) => rank(a) - rank(b)).map((id) => card(agent, tracked, id, tracked.open.get(id)));
      }),
    );
  };

  const open = (agent: string, tracked: Tracked): void => {
    tracked.subscription = transport.subscribe(`agent/${agent}`, null, {
      entry: (entry) => {
        if (entry.kind !== "known") return;
        const record = permissionRecord(entry.event);
        if (record === null) return;
        if (record.state === "open") {
          tracked.open.set(record.request_id, { seq: entry.seq, record });
          tracked.closed.delete(record.request_id);
        } else {
          tracked.open.delete(record.request_id);
          tracked.closed.add(record.request_id);
        }
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
        tracked.open.clear();
        tracked.closed.clear();
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
        const fresh: Tracked = { label: row.label, ids: row.waitingOnHuman, listedAt: listed++, open: new Map(), closed: new Set(), synced: false, failure: null, subscription: null };
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
