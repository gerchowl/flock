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
 * A turn's entries, in seq order. `key` is its first entry's seq: appends
 * never change it, so a view keyed by it keeps each turn mounted.
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

/**
 * The turns of a transcript in a form that grows in place, so a view can
 * keep it in a store and fold each new entry in at O(1) (#1782). Records
 * have no prototype, so an id such as `constructor` is an ordinary key.
 */
export type TurnFold = {
  readonly turns: { readonly key: number; readonly entries: SessionEntry[] }[];
  readonly runs: Record<string, ToolRun>;
  readonly absorbed: Record<string, true>;
  /** Result ids a kept entry shows because no call preceded them. */
  readonly unpaired: Record<string, true>;
  /** Seen request ids: native occurrence count or canonical presence, never permission state. */
  readonly permissionFacts: Record<string, number | "canonical">;
};

/** A fold of no entries. */
export function emptyFold(): TurnFold {
  return { turns: [], runs: Object.create(null), absorbed: Object.create(null), unpaired: Object.create(null), permissionFacts: Object.create(null) };
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

/** Indexes related facts; true when an earlier kept entry can now fold away. */
function pair(fold: TurnFold, entry: SessionEntry): boolean {
  let regroup = false;
  const fact = permission(entry);
  const seen = fact === null ? undefined : fold.permissionFacts[fact.key];
  if (fact?.canonical === true) {
    if (typeof seen === "number") {
      const last = fold.turns.at(-1);
      const previous = last?.entries.at(-1);
      const twin = previous === undefined ? null : permission(previous);
      // The producer stores native then canonical adjacently. Replace that tail in O(1).
      if (seen === 1 && twin?.canonical === false && twin.key === fact.key && last !== undefined) {
        last.entries.pop();
        if (last.entries.length === 0) fold.turns.pop();
      } else {
        regroup = true;
      }
    }
    fold.permissionFacts[fact.key] = "canonical";
  } else if (fact !== null && seen !== "canonical") {
    fold.permissionFacts[fact.key] = (seen ?? 0) + 1;
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
  const last = fold.turns.at(-1);
  if (last === undefined || (startsTurn(entry) && last.entries.length > 0)) fold.turns.push({ key: entry.seq, entries: [entry] });
  else last.entries.push(entry);
}

/**
 * Folds `entry`, the transcript's next one, into `fold` in place, reading
 * only `entry`, identity indexes and the fold's last turn. Returns false
 * when an earlier kept entry can now fold away: a late paired tool result,
 * or a nonadjacent canonical permission twin. Adjacent native/canonical
 * pairs replace the tail in O(1); other reordering uses {@link foldAll}.
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

/** Groups `entries` into turns and pairs tool calls with their results. */
export function groupTurns(entries: readonly SessionEntry[]): Turns {
  const fold = foldAll(entries);
  const groups = fold.turns.map((turn) => ({ key: turn.key, entries: turn.entries }));
  return {
    turns: groups,
    runs: new Map(Object.entries(fold.runs)),
    absorbed: new Set(Object.keys(fold.absorbed)),
    byKey: new Map(groups.map((group) => [group.key, group])),
  };
}
