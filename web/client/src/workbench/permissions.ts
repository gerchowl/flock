/**
 * The live inbox of open permission prompts (ADR-0038 §1, §2a, §3; #1749
 * PR 1, read-only).
 *
 * The card set is fleet-lite's `waiting_on_human`, the server's answer to
 * "what waits on a human". An id that is not listed never shows a card, even
 * when its stream has an open record: the server also opens a record for an
 * automatic request (one a session grant pre-approves), with no flag, and
 * that card would be one nobody can act on. An open record for an unlisted id
 * that never gets listed is an orphan; the server closes those with
 * `reason: session_ended` on restart (#2236). If a request turns manual, its
 * id is listed and its card shows.
 *
 * A listed id's details and `request_seq` come from the server's permission
 * record on `agent/<id>` (`wire/permission-record.ts`, #2224), and a listed id
 * with no open record yet shows a loading card (or, once the stream has
 * synced without one, says so); a listed id is never hidden. A record that
 * does not match the decided shape fails visibly: a listed id's card shows
 * what is wrong, and an unlisted or unreadable one gets a notice card, never
 * a silent drop. A card leaves on whichever server signal arrives first: the
 * closed record with its `request_id`, or the id dropping out of
 * `waiting_on_human`. A closed id does not come back as pending while the
 * fleet still lists it; a closed record for an unknown id is ignored. A
 * history `gap` marks that agent's cards stale until its next entry, `synced`
 * or `reset`. The next entry is the signal the transport guarantees: the wire
 * transport passes on only the first `synced` of a stream, so a gap after a
 * re-subscribe gets none, but the replay's entries follow the gap. A notice
 * for a record no listed id owns expires on the next `synced` or `reset`.
 *
 * One subscription per agent with listed ids, opened when its first id is
 * listed and closed when its list empties. The focused transcript tile's own
 * subscription is independent.
 *
 * Order: every permission prompt blocks its agent, so all cards block, and
 * they sort by each request's own age: the open record's server timestamp
 * (`ts_ms`), then the agent's first-listed age, then the per-stream seq. Ids
 * still without a record follow, oldest agent first, in the server's listed
 * order. Notice cards come last.
 *
 * @module
 */
import { createSignal, type Accessor } from "solid-js";

import type { AgentRow } from "../fleet";
import type { Subscription, Transport } from "../transport";
import { TITLE_MAX, boundText, inputSummary } from "../views/turns";
import { permissionRecord, type OpenPermissionRecord } from "../wire/permission-record";
import type { InboxCard } from "./inbox";

/** An open record, with the stream seq and server time it arrived at. */
type Open = { readonly seq: number; readonly tsMs: number; readonly record: OpenPermissionRecord };

type Tracked = {
  label: string;
  ids: readonly string[];
  /** Orders agents: assigned when the list first became non-empty. */
  readonly listedAt: number;
  /** Open records by `request_id`; a closed record removes its entry. */
  readonly open: Map<string, Open>;
  /** Ids whose closed record arrived: not shown as pending while the fleet still lists them. */
  readonly closed: Set<string>;
  /** Records that did not match the decided shape, by `request_id` (or a per-entry key when unreadable). */
  readonly invalid: Map<string, { readonly id: string | null; readonly problem: string }>;
  synced: boolean;
  /** A history gap arrived and nothing has followed it yet. */
  stale: boolean;
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

/** One card with the keys it sorts by. */
type Keyed = { readonly card: InboxCard; readonly tsMs: number; readonly listedAt: number; readonly seq: number; readonly index: number };

/** Creates the inbox; call {@link PermissionInbox.track} with each fleet update. */
export function createPermissionInbox(transport: Transport): PermissionInbox {
  const [cards, setCards] = createSignal<readonly InboxCard[]>([]);
  const agents = new Map<string, Tracked>();
  let listed = 0;
  let closed = false;

  const base = (agent: string, tracked: Tracked, id: string) => ({
    id,
    kind: "permission" as const,
    agent,
    agentLabel: tracked.label,
    choices: CHOICES,
    blocking: true,
    waitsFor: WAITS_FOR,
    ...(tracked.stale ? { stale: true as const } : {}),
  });

  const card = (agent: string, tracked: Tracked, id: string, index: number): Keyed => {
    const found = tracked.open.get(id);
    const bad = tracked.invalid.get(id);
    const pendingTitle = `${tracked.label} · permission ${id}`;
    const common = { tsMs: found?.tsMs ?? Number.POSITIVE_INFINITY, listedAt: tracked.listedAt, seq: found?.seq ?? Number.POSITIVE_INFINITY, index };
    if (found !== undefined && bad === undefined) {
      const { request, request_seq: requestSeq } = found.record;
      return { ...common, card: { ...base(agent, tracked, id), title: boundText(typeof request.display_name === "string" ? request.display_name : request.tool_name, TITLE_MAX), detail: inputSummary(request.tool_name, request.input), requestSeq } };
    }
    const pending =
      bad !== undefined
        ? `Unusable permission record: ${bad.problem}`
        : tracked.failure !== null
          ? `Details unavailable: ${tracked.failure}`
          : tracked.synced
            ? "This agent's stream has no open permission record for it."
            : "Loading details…";
    return { ...common, card: { ...base(agent, tracked, id), title: pendingTitle, detail: pending } };
  };

  /** A visible notice for a record that cannot be attached to a listed id. */
  const notice = (agent: string, tracked: Tracked, key: string, id: string | null, problem: string, index: number): Keyed => ({
    tsMs: Number.POSITIVE_INFINITY,
    listedAt: tracked.listedAt,
    seq: Number.POSITIVE_INFINITY,
    index,
    card: {
      id: `${agent}:invalid:${key}`,
      kind: "permission",
      agent,
      agentLabel: tracked.label,
      title: id === null ? `${tracked.label} · unreadable permission record` : `${tracked.label} · permission ${id} (not awaiting you)`,
      detail: `Unusable permission record: ${problem}`,
      choices: [],
      blocking: false,
      waitsFor: WAITS_FOR,
    },
  });

  const publish = (): void => {
    const keyed: Keyed[] = [];
    const notices: Keyed[] = [];
    for (const [agent, tracked] of agents) {
      tracked.ids.forEach((id, index) => {
        if (tracked.open.has(id) || !tracked.closed.has(id)) keyed.push(card(agent, tracked, id, index));
      });
      for (const [key, bad] of tracked.invalid) {
        if (bad.id === null || !tracked.ids.includes(bad.id)) notices.push(notice(agent, tracked, key, bad.id, bad.problem, notices.length));
      }
    }
    // Records first by their server age; ids without a record follow by agent age and listed order.
    keyed.sort((a, b) => a.tsMs - b.tsMs || a.listedAt - b.listedAt || a.seq - b.seq || a.index - b.index);
    setCards([...keyed, ...notices].map((entry) => entry.card));
  };

  /** Drops the notices for records no listed id owns: they live until the next `reset` or `synced`. */
  const expireNotices = (tracked: Tracked): void => {
    for (const [key, bad] of tracked.invalid) if (bad.id === null || !tracked.ids.includes(bad.id)) tracked.invalid.delete(key);
  };

  const open = (agent: string, tracked: Tracked): void => {
    tracked.subscription = transport.subscribe(`agent/${agent}`, null, {
      entry: (entry) => {
        if (entry.kind === "gap") {
          tracked.stale = true;
          return publish();
        }
        tracked.stale = false;
        if (entry.kind !== "known") return publish();
        const record = permissionRecord(entry.event);
        if (record === null) return publish();
        if (record.state === "invalid") {
          tracked.invalid.set(record.request_id ?? `seq-${entry.seq}`, { id: record.request_id, problem: record.problem });
        } else if (record.state === "open") {
          tracked.invalid.delete(record.request_id);
          tracked.open.set(record.request_id, { seq: entry.seq, tsMs: entry.tsMs ?? 0, record });
          tracked.closed.delete(record.request_id);
        } else {
          tracked.invalid.delete(record.request_id);
          tracked.open.delete(record.request_id);
          tracked.closed.add(record.request_id);
        }
        publish();
      },
      synced: () => {
        tracked.synced = true;
        tracked.stale = false;
        expireNotices(tracked);
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
        tracked.invalid.clear();
        tracked.synced = false;
        tracked.stale = false;
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
        const fresh: Tracked = {
          label: row.label,
          ids: row.waitingOnHuman,
          listedAt: listed++,
          open: new Map(),
          closed: new Set(),
          invalid: new Map(),
          synced: false,
          stale: false,
          failure: null,
          subscription: null,
        };
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
