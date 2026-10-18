/**
 * Presentation-only grouping of a transcript into turns (ADR-0038 §2a,
 * §3: "grouping by turn" is a derivation the client may make).
 *
 * A turn starts at a person's message: a `user` event whose content is
 * text, not only tool results. Entries before the first one form the
 * opening turn. Each `tool_use` block is paired with the `tool_result` that
 * names its id, so the view can show a tool run as one line; a `user`
 * event that only carries paired results is folded into those lines.
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
  /** Tool runs by `tool_use` id. */
  readonly runs: ReadonlyMap<string, ToolRun>;
  /** `tool_result` ids shown as part of their run's line. */
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

/** Groups `entries` into turns and pairs tool calls with their results. */
export function groupTurns(entries: readonly SessionEntry[]): Turns {
  const runs = new Map<string, ToolRun>();
  const absorbed = new Set<string>();
  for (const entry of entries) {
    for (const block of blocks(entry)) {
      const id = block["type"] === "tool_use" ? block["id"] : block["tool_use_id"];
      if (typeof id !== "string") continue;
      if (block["type"] === "tool_use") {
        runs.set(id, { id, name: String(block["name"] ?? "tool"), command: commandOf(block["input"]), result: null });
      } else if (block["type"] === "tool_result" && runs.has(id)) {
        const run = runs.get(id);
        if (run !== undefined) runs.set(id, { ...run, result: block });
        absorbed.add(id);
      }
    }
  }
  const turns: SessionEntry[][] = [[]];
  for (const entry of entries) {
    const resultsOnly = blocks(entry).length > 0 && blocks(entry).every((block) => block["type"] === "tool_result" && absorbed.has(String(block["tool_use_id"])));
    if (resultsOnly) continue;
    if (startsTurn(entry) && (turns.at(-1)?.length ?? 0) > 0) turns.push([]);
    turns.at(-1)?.push(entry);
  }
  const groups = turns.flatMap((turn) => (turn[0] === undefined ? [] : [{ key: turn[0].seq, entries: turn }]));
  return { turns: groups, runs, absorbed, byKey: new Map(groups.map((group) => [group.key, group])) };
}
