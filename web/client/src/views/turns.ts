/**
 * Presentation-only grouping of a transcript into turns (ADR-0038 §2a,
 * §3: "grouping by turn" is a derivation the client may make).
 *
 * A turn starts at a person's message: a `user` event whose content is
 * text, not only tool results. Entries before the first one form the
 * opening turn. Each `tool_use` block is paired with the `tool_result` that
 * names its id, so the view can show a tool run as one line; a `user`
 * event that only carries paired results is folded into those lines.
 * A canonical permission fact presents its request; a native control request
 * with the same request_id is transport detail, folded out of presentation.
 * Uncorrelated native requests and unknown facts remain visible (#2559).
 *
 * Known limit: ADR-0036's events carry no exit code or duration for a tool
 * run, so the line shows the command and whether the result was an error
 * (#1751).
 *
 * @module
 */
import type { JsonValue } from "../gen/serde_json/JsonValue";
import type { SessionEntry } from "../transport";

type Block = { readonly [key: string]: JsonValue };

/** One tool run: the call, and its result once it arrived. */
export type ToolRun = { readonly id: string; readonly name: string; readonly command: string; readonly result: Block | null };

/**
 * A turn's entries, in seq order. `key` is its first kept entry's seq:
 * it changes only when correlation retires that opening entry.
 */
export type TurnGroup = { readonly key: number; readonly entries: readonly SessionEntry[] };

/** The grouping of a transcript. */
export type Turns = {
  readonly turns: readonly TurnGroup[];
  /** Tool runs by `tool_use` id; a lookup, in no meaningful order. */
  readonly runs: ReadonlyMap<string, ToolRun>;
  /** `tool_result` ids shown as part of their run's line; a lookup, in no meaningful order. */
  readonly absorbed: ReadonlySet<string>;
  /** The turns by {@link TurnGroup.key}. */
  readonly byKey: ReadonlyMap<number, TurnGroup>;
};

function blocks(entry: SessionEntry): Block[] {
  if (entry.kind !== "known" || (entry.event.type !== "assistant" && entry.event.type !== "user")) return [];
  const content = entry.event.message["content"];
  if (!Array.isArray(content)) return [];
  return content.filter((block): block is Block => typeof block === "object" && block !== null && !Array.isArray(block));
}

function startsTurn(entry: SessionEntry): boolean {
  if (entry.kind !== "known" || entry.event.type !== "user") return false;
  const content = entry.event.message["content"];
  return typeof content === "string" || blocks(entry).some((block) => block["type"] === "text");
}

const SUMMARY_KEYS = ["command", "file_path", "path", "pattern", "url"] as const;

/** The input's value under a well-known key, or `null`. */
function knownInput(input: JsonValue | undefined): string | null {
  if (typeof input !== "object" || input === null || Array.isArray(input)) return null;
  for (const key of SUMMARY_KEYS) {
    const value = input[key];
    if (typeof value === "string") return value;
  }
  return null;
}

/** The one-line command of a tool call: its command, path or first string input. */
export function commandOf(input: JsonValue | undefined): string {
  const known = knownInput(input);
  if (known !== null) return known;
  if (typeof input !== "object" || input === null || Array.isArray(input)) return "";
  return Object.values(input).find((value): value is string => typeof value === "string") ?? "";
}

/** The longest input summary a card shows, in characters; longer ones end in an ellipsis. */
export const SUMMARY_MAX = 200;
/** The longest card title or placeholder, in characters. */
export const TITLE_MAX = 80;

/** `text` cut to at most `max` code points (never inside a surrogate pair), ending in an ellipsis when cut. */
export function boundText(text: string, max: number): string {
  const points = [...text];
  return points.length > max ? `${points.slice(0, max - 1).join("")}…` : text;
}

/**
 * What a permission card shows for a tool's input: the well-known key's value
 * bounded to {@link SUMMARY_MAX}, or a typed placeholder naming the tool when
 * the input has none (a card never claims an empty input).
 */
export function inputSummary(tool: string, input: JsonValue | undefined): string {
  const known = knownInput(input);
  return known === null ? boundText(`(no summary for ${tool})`, TITLE_MAX) : boundText(known, SUMMARY_MAX);
}

/** A turn retains sparse slots until at least half have been retired. */
type FoldTurn = { key: number; entries: (SessionEntry | null)[]; first: number; removed: number };
/** Numeric locations survive Solid reconciliation without cross-store object aliases. */
type PermissionLocation = { turn: number; slot: number };

/**
 * The turns of a transcript in a form that grows in place, so a view can
 * keep it in a store and fold permission twins in O(1) amortised per entry
 * (#1782, #2575). Records
 * have no prototype, so an id such as `constructor` is an ordinary key.
 */
export type TurnFold = {
  readonly turns: FoldTurn[];
  readonly runs: Record<string, ToolRun>;
  readonly absorbed: Record<string, true>;
  /** Result ids a kept entry shows because no call preceded them. */
  readonly unpaired: Record<string, true>;
  /** Pending native slots or canonical presence, never permission state; scoped to this fold. */
  readonly permissionFacts: Record<string, number[] | "canonical">;
  /** Pending native locations by stream seq; no entries or turn objects are aliased. */
  readonly permissionLocations: Record<number, PermissionLocation>;
};

/** A fold of no entries. */
export function emptyFold(): TurnFold {
  return { turns: [], runs: Object.create(null), absorbed: Object.create(null), unpaired: Object.create(null), permissionFacts: Object.create(null), permissionLocations: Object.create(null) };
}

/** Only known permission facts with nonempty shared ids can correlate. */
function permission(entry: SessionEntry): { key: string; canonical: boolean } | null {
  if (entry.kind !== "known") return null;
  const event = entry.event;
  if (event.type !== "control_request" && event.type !== "flk_permission_request") return null;
  if (typeof event.request_id !== "string" || event.request_id.length === 0) return null;
  // Prefix the index key so even reserved object setters remain ordinary ids in Solid's store.
  return { key: `request:${event.request_id}`, canonical: event.type === "flk_permission_request" };
}

/**
 * Retire an indexed native twin. Scanning/compaction costs are charged to
 * retired slots: each leading hole is crossed once, and a compaction scans
 * at most twice the number of holes it removes. No later row is shifted.
 */
function retireNative(fold: TurnFold, seq: number): void {
  const location = fold.permissionLocations[seq];
  if (location === undefined) throw new Error("missing native permission location");
  const turn = fold.turns[location.turn];
  if (turn === undefined) throw new Error("missing native permission turn");
  turn.entries[location.slot] = null;
  delete fold.permissionLocations[seq];
  turn.removed++;
  while (turn.entries[turn.first] === null) turn.first++;
  const first = turn.entries[turn.first];
  if (first == null) {
    // Only the opening turn can lack a user message. Removing it can shift
    // later turns once per transcript, amortised over their earlier appends.
    if (location.turn !== 0) throw new Error("empty non-opening permission turn");
    fold.turns.shift();
    for (const pending of Object.values(fold.permissionLocations)) pending.turn--;
    return;
  }
  turn.key = first.seq;
  if (turn.removed * 2 >= turn.entries.length) {
    const kept: SessionEntry[] = [];
    for (const entry of turn.entries) {
      if (entry === null) continue;
      const pending = fold.permissionLocations[entry.seq];
      if (pending !== undefined) pending.slot = kept.length;
      kept.push(entry);
    }
    turn.entries = kept;
    turn.first = 0;
    turn.removed = 0;
  }
}

/** Index related facts; only late tool results can require a full regroup. */
function pair(fold: TurnFold, entry: SessionEntry): boolean {
  let regroup = false;
  const fact = permission(entry);
  if (fact?.canonical === true) {
    const pending = fold.permissionFacts[fact.key];
    if (Array.isArray(pending)) for (const seq of pending) retireNative(fold, seq);
    fold.permissionFacts[fact.key] = "canonical";
  }
  for (const block of blocks(entry)) {
    const id = block["type"] === "tool_use" ? block["id"] : block["tool_use_id"];
    if (typeof id !== "string") continue;
    if (block["type"] === "tool_use") {
      fold.runs[id] = { id, name: String(block["name"] ?? "tool"), command: commandOf(block["input"]), result: null };
    } else if (block["type"] === "tool_result" && fold.runs[id] !== undefined) {
      fold.runs[id] = { ...fold.runs[id], result: block };
      if (fold.unpaired[id] === true && fold.absorbed[id] !== true) regroup = true;
      fold.absorbed[id] = true;
    }
  }
  return regroup;
}

/** Places `entry` in its turn, unless another presentation already carries its fact. */
function place(fold: TurnFold, entry: SessionEntry): void {
  const fact = permission(entry);
  if (fact?.canonical === false && fold.permissionFacts[fact.key] === "canonical") return;
  const own = blocks(entry);
  if (own.length > 0 && own.every((block) => block["type"] === "tool_result" && fold.absorbed[String(block["tool_use_id"])] === true)) return;
  for (const block of own) {
    const id = String(block["tool_use_id"]);
    if (block["type"] === "tool_result" && fold.absorbed[id] !== true) fold.unpaired[id] = true;
  }
  let turn = fold.turns.at(-1);
  if (turn === undefined || startsTurn(entry)) {
    turn = { key: entry.seq, entries: [entry], first: 0, removed: 0 };
    fold.turns.push(turn);
  } else turn.entries.push(entry);
  if (fact?.canonical === false) {
    const pending = fold.permissionFacts[fact.key];
    fold.permissionLocations[entry.seq] = { turn: fold.turns.length - 1, slot: turn.entries.length - 1 };
    if (Array.isArray(pending)) pending.push(entry.seq);
    else fold.permissionFacts[fact.key] = [entry.seq];
  }
}

/**
 * Folds `entry`, the transcript's next one, into `fold` in place using
 * identity indexes and stable presentation slots. Returns false
 * when a late paired tool result needs regrouping. Permission twins use
 * indexed slots in any order: each native slot is retired once, and
 * compaction is O(1) amortised per entry, including repeated native ids.
 */
export function foldEntry(fold: TurnFold, entry: SessionEntry): boolean {
  const regroup = pair(fold, entry);
  place(fold, entry);
  return !regroup;
}

/**
 * Folds every entry at once: related facts are indexed before placement,
 * so native permission twins fold away even when they precede the canonical fact.
 */
export function foldAll(entries: readonly SessionEntry[]): TurnFold {
  const fold = emptyFold();
  for (const entry of entries) pair(fold, entry);
  for (const entry of entries) place(fold, entry);
  return fold;
}

/** Materialise kept entries for a snapshot; incremental rendering reads slots directly. */
export function keptEntries(turn: FoldTurn): SessionEntry[] {
  return turn.entries.filter((entry): entry is SessionEntry => entry !== null);
}

/** Groups `entries` into turns and pairs tool calls with their results. */
export function groupTurns(entries: readonly SessionEntry[]): Turns {
  const fold = foldAll(entries);
  const groups = fold.turns.map((turn) => ({ key: turn.key, entries: keptEntries(turn) }));
  return {
    turns: groups,
    runs: new Map(Object.entries(fold.runs)),
    absorbed: new Set(Object.keys(fold.absorbed)),
    byKey: new Map(groups.map((group) => [group.key, group])),
  };
}
