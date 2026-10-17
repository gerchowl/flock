/**
 * The transcript as turns (ADR-0038 §2a): tool runs as one line, older turns
 * folded. Grouping is presentation only (§3).
 */
import { afterEach, describe, expect, test } from "bun:test";
import { render } from "solid-js/web";

import type { KnownSessionEvent } from "../src/gen/KnownSessionEvent";
import type { SessionEntry, Transport } from "../src/transport";
import { Transcript } from "../src/views/Transcript";
import { groupTurns } from "../src/views/turns";

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
