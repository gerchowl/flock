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
 * re-subscribe gets none, and the transport cannot tell when a replay ends.
 * Stale therefore clears on the first entry after the gap, not at the replay's
 * end; the server's `request_seq` check is the backstop, refusing an answer to
 * a request that changed meanwhile, and the card shows that refusal. A notice
 * for a record no listed id owns expires on the next `synced` or `reset`.
 *
 * One reader per agent with listed ids, opened when its first id is listed
 * and released when its list empties. The records come from the session store
 * (`store/sessions.ts`, ADR-0038 §3), which holds one subscription per stream
 * for all its readers: the focused transcript tile shares it, and this module
 * keeps no subscription and no copy of the stream.
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
import { sessionStore, type StreamReader } from "../store/sessions";
import type { Transport } from "../transport";
import { TITLE_MAX, boundText, inputSummary } from "../views/turns";
import type { InboxCard } from "./inbox";

type Tracked = {
  label: string;
  ids: readonly string[];
  /** Orders agents: assigned when the list first became non-empty. */
  readonly listedAt: number;
  /** The store's record of the agent's stream: the permission fold and the progress live there. */
  reader: StreamReader | null;
};

const CHOICES = ["Allow once", "Always allow", "Deny"] as const;

/** The live permission inbox over `transport`. */
export type PermissionInbox = {
  readonly cards: Accessor<readonly InboxCard[]>;
  /** Applies the fleet's current rows: reads and releases agent streams. */
  readonly track: (rows: readonly AgentRow[]) => void;
  /** Releases every stream. */
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
    ...(tracked.reader?.permissions.stale ? { stale: true as const } : {}),
  });

  const card = (agent: string, tracked: Tracked, id: string, index: number): Keyed => {
    const fold = tracked.reader?.permissions;
    const progress = tracked.reader?.progress();
    const found = fold?.open.get(id);
    const bad = fold?.invalid.get(id);
    const pendingTitle = `${tracked.label} · permission ${id}`;
    const common = { tsMs: found?.tsMs ?? Number.POSITIVE_INFINITY, listedAt: tracked.listedAt, seq: found?.seq ?? Number.POSITIVE_INFINITY, index };
    if (found !== undefined && bad === undefined) {
      const { request, request_seq: requestSeq } = found.record;
      return { ...common, card: { ...base(agent, tracked, id), title: boundText(typeof request.display_name === "string" ? request.display_name : request.tool_name, TITLE_MAX), detail: inputSummary(request.tool_name, request.input), requestSeq } };
    }
    const pending =
      bad !== undefined
        ? `Unusable permission record: ${bad.problem}`
        : progress?.phase === "failed"
          ? `Details unavailable: ${progress.failure.message}`
          : progress?.phase === "synced"
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
      },
  });

  const publish = (): void => {
    const keyed: Keyed[] = [];
    const notices: Keyed[] = [];
    for (const [agent, tracked] of agents) {
      const fold = tracked.reader?.permissions;
      tracked.ids.forEach((id, index) => {
        if (fold?.open.has(id) || !fold?.closed.has(id)) keyed.push(card(agent, tracked, id, index));
      });
      for (const [key, bad] of fold?.invalid ?? []) {
        if (bad.id === null || !tracked.ids.includes(bad.id)) notices.push(notice(agent, tracked, key, bad.id, bad.problem, notices.length));
      }
    }
    // Records first by their server age; ids without a record follow by agent age and listed order.
    keyed.sort((a, b) => a.tsMs - b.tsMs || a.listedAt - b.listedAt || a.seq - b.seq || a.index - b.index);
    setCards([...keyed, ...notices].map((entry) => entry.card));
  };

  /** Drops the notices for records no listed id owns: they live until the next `reset` or `synced`. */
  const expireNotices = (tracked: Tracked): void => {
    for (const [key, bad] of tracked.reader?.permissions.invalid ?? []) if (bad.id === null || !tracked.ids.includes(bad.id)) tracked.reader?.permissions.invalid.delete(key);
  };

  const open = (agent: string, tracked: Tracked): void => {
    let joined = false;
    tracked.reader = sessionStore(transport).read(`agent/${agent}`, {
      // A notice lives until the next `synced`, not until a reader joins an already synced stream.
      current: (current) => {
        if (current && joined) expireNotices(tracked);
      },
      change: publish,
    });
    joined = true;
  };

  return {
    cards,
    track: (rows) => {
      if (closed) return;
      const waiting = new Map(rows.filter((row) => row.waitingOnHuman.length > 0).map((row) => [row.id, row]));
      for (const [agent, tracked] of agents) {
        if (waiting.has(agent)) continue;
        tracked.reader?.release();
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
          reader: null,
        };
        agents.set(agent, fresh);
        open(agent, fresh);
      }
      publish();
    },
    close: () => {
      closed = true;
      for (const tracked of agents.values()) tracked.reader?.release();
      agents.clear();
      setCards([]);
    },
  };
}
