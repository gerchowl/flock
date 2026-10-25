/**
 * The transcript as turns (ADR-0038 §2a): tool runs as one line, older turns
 * folded. Grouping is presentation only (§3).
 */
import { afterEach, describe, expect, test } from "bun:test";
import { render } from "solid-js/web";

import type { JsonValue } from "../src/gen/serde_json/JsonValue";
import type { KnownSessionEvent } from "../src/gen/KnownSessionEvent";
import type { SessionEntry, Transport } from "../src/transport";
import { classifyStoredLine, storedLines } from "../src/transport/fixture";
import { Transcript } from "../src/views/Transcript";
import { emptyFold, foldAll, foldEntry, groupTurns, type ToolRun, type TurnFold } from "../src/views/turns";
import { readFixture } from "./fixture-replay";

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  document.body.innerHTML = "";
});

let seq = 0;
const known = (event: unknown): SessionEntry => ({ seq: ++seq, kind: "known", event: event as KnownSessionEvent });
const said = (text: string): SessionEntry => known({ type: "user", message: { role: "user", content: text } });
const reply = (...content: unknown[]): SessionEntry => known({ type: "assistant", message: { role: "assistant", content } });
const result = (id: string, content: string, isError = false): SessionEntry =>
  known({ type: "user", message: { role: "user", content: [{ type: "tool_result", tool_use_id: id, content, is_error: isError }] } });
const bash = (id: string, command: string): unknown => ({ type: "tool_use", id, name: "Bash", input: { command, description: "x" } });

function session(turns: number): SessionEntry[] {
  seq = 0;
  const entries: SessionEntry[] = [];
  for (let turn = 1; turn <= turns; turn++) {
    entries.push(said(`question ${turn}`), reply({ type: "text", text: `on it ${turn}` }, bash(`tu${turn}`, `just check ${turn}`)));
    entries.push(result(`tu${turn}`, turn === 2 ? "exit 1" : "ok", turn === 2), reply({ type: "text", text: `done ${turn}` }));
  }
  return entries;
}

describe("groupTurns", () => {
  test("starts a turn at each message from the person and pairs tool calls with results", () => {
    const grouped = groupTurns(session(2));
    expect(grouped.turns.map((turn) => turn.entries.map((entry) => entry.seq))).toEqual([[1, 2, 4], [5, 6, 8]]);
    expect(grouped.runs.get("tu2")).toEqual({
      id: "tu2",
      name: "Bash",
      command: "just check 2",
      result: { type: "tool_result", tool_use_id: "tu2", content: "exit 1", is_error: true },
    });
    expect([...grouped.absorbed]).toEqual(["tu1", "tu2"]);
  });

  test("an unpaired result stays visible, and a call without a result is running", () => {
    seq = 0;
    const grouped = groupTurns([reply(bash("a", "ls")), result("other", "x")]);
    expect(grouped.turns[0]?.entries.length).toBe(2);
    expect(grouped.runs.get("a")?.result).toBeNull();
  });
});

describe("Transcript turns", () => {
  test("a tool run is one line that expands; earlier turns fold until asked", () => {
    const entries = session(5);
    const transport: Transport = {
      subscribe: (_stream, _after, observer) => {
        for (const entry of entries) observer.entry(entry);
        observer.synced();
        return { close: () => undefined };
      },
    };
    const root = document.createElement("div");
    document.body.append(root);
    dispose = render(() => <Transcript transport={transport} stream="agent/a" source="replay" />, root);
    const runs = [...root.querySelectorAll<HTMLDetailsElement>('[data-testid="tool-run"]')];
    expect(runs.map((run) => run.querySelector("summary")?.textContent)).toEqual(["Bashjust check 3ok", "Bashjust check 4ok", "Bashjust check 5ok"]);
    expect(runs.every((run) => !run.open)).toBe(true);
    expect(root.querySelectorAll('[data-testid="turn"]').length).toBe(3);
    const fold = root.querySelector<HTMLButtonElement>('[data-testid="earlier-turns"]');
    expect(fold?.textContent).toBe("2 earlier turns");
    fold?.click();
    expect(root.querySelectorAll('[data-testid="turn"]').length).toBe(5);
    expect(root.querySelector('[data-testid="earlier-turns"]')).toBeNull();
    expect(root.querySelector(".tool-run__outcome--error")?.textContent).toBe("error");
    expect(root.textContent).not.toContain("Tool result");
  });

  test("appends keep turns mounted: an expanded tool run stays open, and its result fills in", () => {
    seq = 0;
    let observer: Parameters<Transport["subscribe"]>[2] | undefined;
    const transport: Transport = {
      subscribe: (_stream, _after, next) => {
        observer = next;
        return { close: () => undefined };
      },
    };
    const root = document.createElement("div");
    document.body.append(root);
    dispose = render(() => <Transcript transport={transport} stream="agent/a" source="live" />, root);
    observer?.entry(said("question 1"));
    observer?.entry(reply({ type: "text", text: "on it" }, bash("tu1", "just check")));
    const run = root.querySelector<HTMLDetailsElement>('[data-testid="tool-run"]');
    expect(run?.querySelector(".tool-run__outcome")?.textContent).toBe("running");
    if (run) run.open = true;
    const turn = root.querySelector('[data-testid="turn"]');
    observer?.entry(result("tu1", "ok"));
    observer?.entry(reply({ type: "text", text: "done" }));
    observer?.entry(said("question 2"));
    expect(root.querySelector('[data-testid="tool-run"]')).toBe(run);
    expect(root.querySelector('[data-testid="turn"]')).toBe(turn);
    expect(run?.open).toBe(true);
    expect(run?.querySelector(".tool-run__outcome")?.textContent).toBe("ok");
    expect(root.querySelectorAll('[data-testid="turn"]').length).toBe(2);
  });
});

type Block = { readonly [key: string]: JsonValue };

/** `groupTurns` before #1782, verbatim: the fold must group every transcript exactly as it did. */
function groupedBefore(entries: readonly SessionEntry[]): { turns: number[][]; runs: Map<string, ToolRun>; absorbed: Set<string> } {
  const blocks = (entry: SessionEntry): Block[] => {
    if (entry.kind !== "known" || (entry.event.type !== "assistant" && entry.event.type !== "user")) return [];
    const content = entry.event.message["content"];
    if (!Array.isArray(content)) return [];
    return content.filter((block): block is Block => typeof block === "object" && block !== null && !Array.isArray(block));
  };
  const startsTurn = (entry: SessionEntry): boolean =>
    entry.kind === "known" && entry.event.type === "user" && (typeof entry.event.message["content"] === "string" || blocks(entry).some((block) => block["type"] === "text"));
  const commandOf = (input: JsonValue | undefined): string => {
    if (typeof input !== "object" || input === null || Array.isArray(input)) return "";
    for (const key of ["command", "file_path", "path", "pattern", "url"]) {
      const value = input[key];
      if (typeof value === "string") return value;
    }
    return Object.values(input).find((value): value is string => typeof value === "string") ?? "";
  };
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
  return { turns: turns.filter((turn) => turn.length > 0).map((turn) => turn.map((entry) => entry.seq)), runs, absorbed };
}

/** Folds `entries` one at a time, as the view does, regrouping when told to. */
function foldedOneByOne(entries: readonly SessionEntry[]): TurnFold {
  const applied: SessionEntry[] = [];
  let fold = emptyFold();
  for (const entry of entries) {
    applied.push(entry);
    if (!foldEntry(fold, entry)) fold = foldAll(applied);
  }
  return fold;
}

const sorted = <T,>(map: ReadonlyMap<string, T>): [string, T][] => [...map].sort(([a], [b]) => (a < b ? -1 : 1));

describe("the turn fold (#1782)", () => {
  const callBefore = (): SessionEntry[] => {
    seq = 0;
    // A result before its call stays visible until the call and a second result pair it.
    return [said("q"), result("late", "first"), reply(bash("late", "ls")), result("late", "second"), said("next"), reply(bash("constructor", "pwd")), result("constructor", "ok")];
  };
  const transcripts: [string, () => Promise<SessionEntry[]> | SessionEntry[]][] = [
    ["the recorded session", async () => storedLines(await readFixture()).map((line, index) => classifyStoredLine(index + 1, line))],
    ["five turns of tool runs", () => session(5)],
    ["an unpaired result and a running call", () => ((seq = 0), [reply(bash("a", "ls")), result("other", "x")])],
    ["a result before its call, and an id named like a prototype key", callBefore],
    ["nothing", () => []],
  ];
  for (const [name, entries] of transcripts) {
    test(`groups ${name} exactly as groupTurns did, folded at once or one entry at a time`, async () => {
      const all = await entries();
      const before = groupedBefore(all);
      const now = groupTurns(all);
      expect(now.turns.map((turn) => turn.entries.map((entry) => entry.seq))).toEqual(before.turns);
      expect(sorted(now.runs)).toEqual(sorted(before.runs));
      expect([...now.absorbed].sort()).toEqual([...before.absorbed].sort());
      const one = foldedOneByOne(all);
      expect(one.turns.map((turn) => turn.entries.map((entry) => entry.seq))).toEqual(before.turns);
      expect(sorted(new Map(Object.entries(one.runs)))).toEqual(sorted(before.runs));
      expect(Object.keys(one.absorbed).sort()).toEqual([...before.absorbed].sort());
    });
  }

  test("only a result paired after an earlier one was shown asks for a regroup", () => {
    const fold = emptyFold();
    const entries = callBefore();
    expect(entries.map((entry) => foldEntry(fold, entry))).toEqual([true, true, true, false, true, true, true]);
  });

  test("replaying 5,000 entries visits each entry a bounded number of times, not once per later entry", () => {
    const turns = 1000;
    let visits = 0;
    // Everything that looks at an entry reads its `kind` first. A getter counts each read, also through the view's store, which caches no getter.
    const counted = (entry: SessionEntry): SessionEntry =>
      Object.defineProperty({ ...entry }, "kind", {
        get: () => {
          visits++;
          return entry.kind;
        },
        enumerable: true,
      });
    const entries = session(turns).map(counted);
    entries.push(counted(said("last")));
    expect(entries.length).toBe(4 * turns + 1);
    let observer: Parameters<Transport["subscribe"]>[2] | undefined;
    const transport: Transport = {
      subscribe: (_stream, _after, next) => {
        observer = next;
        return { close: () => undefined };
      },
    };
    const root = document.createElement("div");
    document.body.append(root);
    dispose = render(() => <Transcript transport={transport} stream="agent/a" source="replay" />, root);
    // Delivered after mounting, one entry at a time, as a transport does (`transport.ts`).
    for (const entry of entries) observer?.entry(entry);
    observer?.synced();
    expect(root.querySelectorAll('[data-testid="turn"]').length).toBe(3);
    expect(root.querySelector('[data-testid="earlier-turns"]')?.textContent).toBe(`${turns + 1 - 3} earlier turns`);
    expect(root.querySelector('[data-testid="transcript-synced"]')?.textContent).toBe(`End of recording · ${entries.length} events`);
    // Regrouping on every append visited each entry again for every later one: millions of visits here.
    expect(visits / entries.length).toBeLessThan(20);
  });
});
