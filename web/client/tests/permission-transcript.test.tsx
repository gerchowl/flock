/** Permission facts reach the real transcript through its transport; no timers or server login. */
import { afterEach, expect, test } from "bun:test";
import { render } from "solid-js/web";

import type { CanUseTool } from "../src/gen/CanUseTool";
import { permissionLifecycle } from "../src/gen/FixtureExamples";
import type { SessionEntry, Transport } from "../src/transport";
import { classifyStoredLine } from "../src/transport/fixture";
import { Transcript } from "../src/views/Transcript";
import { emptyFold, foldEntry, groupTurns, keptEntries } from "../src/views/turns";

const open = (id: string, requestSeq = 1) => ({ ...permissionLifecycle[0], request_id: id, request_seq: requestSeq });
const native = (id: string): CanUseTool => ({ type: "control_request", request_id: id, request: { ...permissionLifecycle[0].request } });
const closed = (id: string) => ({ ...permissionLifecycle[2], request_id: id, decision: "allow_once" as const });
const disposers: (() => void)[] = [];
afterEach(() => {
  for (const dispose of disposers.splice(0)) dispose();
  document.body.innerHTML = "";
});

function transcript() {
  let observer: Parameters<Transport["subscribe"]>[2] | undefined;
  const transport: Transport = { subscribe: (_stream, _after, given) => ((observer = given), { close: () => undefined }) };
  const root = document.createElement("div");
  document.body.append(root);
  disposers.push(render(() => <Transcript transport={transport} stream="agent/fixture" source="replay" />, root));
  if (observer === undefined) throw new Error("transcript did not subscribe");
  const subscribed = observer;
  let seq = 0;
  return {
    root,
    deliver: (entry: SessionEntry) => subscribed.entry(entry),
    send: (event: object) => subscribed.entry(classifyStoredLine(++seq, JSON.stringify(event))),
    synced: () => subscribed.synced(),
    reset: () => { seq = 0; subscribed.reset?.(); },
  };
}

for (const prompts of [1, 2]) {
  test(`one transcript entry per prompt, including request ${prompts} after Allow once`, () => {
    const { root, send, synced } = transcript();
    send(open("typed-request"));
    send(native("typed-request"));
    if (prompts === 2) {
      send(closed("typed-request"));
      send({ type: "user", message: { role: "user", content: "another request" } });
      send(open("typed-request-2", 5));
      send(native("typed-request-2"));
    }
    synced();
    expect([...root.querySelectorAll("h2")].filter((h) => h.textContent === "Permission request · Bash")).toHaveLength(prompts);
    expect(root.querySelectorAll(".event--flk_permission_request")).toHaveLength(prompts === 1 ? 1 : 3);
    expect(root.querySelectorAll(".event--control_request")).toHaveLength(0);
    if (prompts === 2) expect(root.textContent).toContain('"decision": "allow_once"');
    // Raw facts still advance the stream, even when their presentation is folded.
    expect(root.querySelector('[data-testid="transcript-synced"]')?.textContent).toBe(`End of recording · ${prompts === 1 ? 2 : 6} events`);
  });
}

test("uncorrelated native requests render, including beside an unrelated or unknown canonical fact", () => {
  const { root, send } = transcript();
  send(open("other-request"));
  send({ ...open("native-only"), state: "future-state" });
  send(native("native-only"));
  expect(root.querySelectorAll(".event--control_request")).toHaveLength(1);
  expect(root.querySelectorAll('[data-testid="unknown-event"]')).toHaveLength(1);
  expect(root.querySelectorAll("h2")).toHaveLength(2);
});

for (const id of ["late-request", "__proto__", "constructor"]) {
  test(`a later canonical twin replaces the native presentation by exact id (${id})`, () => {
    const { root, send } = transcript();
    send(native(id));
    expect(root.querySelectorAll(".event--control_request")).toHaveLength(1);
    send({ type: "user", message: { role: "user", content: "intervening turn" } });
    send(open(id));
    expect(root.querySelectorAll(".event--control_request")).toHaveLength(0);
    expect(root.querySelectorAll(".event--flk_permission_request")).toHaveLength(1);
    expect(root.querySelectorAll("h2")).toHaveLength(2);
  });
}

test("reset discards canonical correlation before replaying a native-only request", () => {
  const { root, send, reset } = transcript();
  send(open("replayed"));
  send(native("replayed"));
  expect(root.querySelectorAll(".event--control_request")).toHaveLength(0);
  reset();
  send(native("replayed"));
  expect(root.querySelectorAll(".event--control_request")).toHaveLength(1);
  expect(root.querySelectorAll(".event--flk_permission_request")).toHaveLength(0);
});

test("canonical correlation is scoped to one session transcript", () => {
  const first = transcript();
  const second = transcript();
  first.send(open("shared-id"));
  second.send(native("shared-id"));
  expect(first.root.querySelectorAll(".event--flk_permission_request")).toHaveLength(1);
  expect(second.root.querySelectorAll(".event--control_request")).toHaveLength(1);
});

test("a canonical close without an open still presents its native twin only once", () => {
  const { root, send } = transcript();
  send(native("closed-only"));
  send(closed("closed-only"));
  expect(root.querySelectorAll(".event--control_request")).toHaveLength(0);
  expect(root.querySelectorAll(".event--flk_permission_request")).toHaveLength(1);
  expect(root.textContent).toContain("Permission request closed");
});

test("native-first permission replay visits entries a bounded number of times", () => {
  const { root, deliver, synced } = transcript();
  const prompts = 1000;
  let visits = 0;
  for (let n = 0; n < prompts; n++) {
    for (const [offset, event] of [native(`request-${n}`), open(`request-${n}`, n * 2 + 2)].entries()) {
      const entry = classifyStoredLine(n * 2 + offset + 1, JSON.stringify(event));
      const kind = entry.kind;
      Object.defineProperty(entry, "kind", { get: () => { visits++; return kind; }, enumerable: true });
      deliver(entry);
    }
  }
  synced();
  expect(root.querySelectorAll(".event--control_request")).toHaveLength(0);
  expect(root.querySelectorAll(".event--flk_permission_request")).toHaveLength(prompts);
  expect(visits / (prompts * 2)).toBeLessThan(20);
});

for (const shape of ["same-turn gap", "turn boundary", "repeated native"] as const) {
  test(`${shape} permission pairing visits entries a bounded number of times`, () => {
    const { root, deliver, synced } = transcript();
    const prompts = 200;
    let visits = 0;
    let entries = 0;
    const send = (event: object) => {
      const entry = classifyStoredLine(++entries, JSON.stringify(event));
      const kind = entry.kind;
      Object.defineProperty(entry, "kind", { get: () => { visits++; return kind; }, enumerable: true });
      deliver(entry);
    };
    for (let n = 0; n < prompts; n++) {
      send({ type: "user", message: { role: "user", content: `turn ${n}` } });
      send(native(`request-${n}`));
      if (shape === "repeated native") {
        send(native(`request-${n}`));
        expect(root.querySelectorAll(".event--control_request")).toHaveLength(2);
      } else {
        send({ type: shape === "turn boundary" ? "user" : "assistant", message: { role: shape === "turn boundary" ? "user" : "assistant", content: "between twins" } });
      }
      send(open(`request-${n}`, entries + 1));
    }
    synced();
    root.querySelector<HTMLButtonElement>('[data-testid="earlier-turns"]')?.click();
    expect(root.querySelectorAll(".event--control_request")).toHaveLength(0);
    expect(root.querySelectorAll(".event--flk_permission_request")).toHaveLength(prompts);
    expect(root.querySelector('[data-testid="transcript-synced"]')?.textContent).toBe(`End of recording · ${entries} events`);
    expect(visits / entries).toBeLessThan(20);
  });
}

/** Every ordering, including twins which precede the first user turn. */
function* orderings<T>(items: readonly T[]): Generator<T[]> {
  if (items.length === 0) { yield []; return; }
  for (const [index, item] of items.entries()) {
    for (const rest of orderings(items.filter((_, i) => i !== index))) yield [item, ...rest];
  }
}

test("indexed permission slots match replay through opening-turn removal and compaction", () => {
  const events = [native("__proto__"), native("__proto__"), open("__proto__"), native("constructor"), open("constructor"), { type: "user", message: { role: "user", content: "next turn" } }];
  for (const ordering of orderings(events)) {
    const fold = emptyFold();
    const applied: SessionEntry[] = [];
    for (const event of ordering) {
      const entry = classifyStoredLine(applied.length + 1, JSON.stringify(event));
      applied.push(entry);
      expect(foldEntry(fold, entry)).toBe(true);
      const expected = groupTurns(applied).turns.map((turn) => ({ key: turn.key, entries: turn.entries.map((entry) => entry.seq) }));
      expect(fold.turns.map((turn) => ({ key: turn.key, entries: keptEntries(turn).map((entry) => entry.seq) }))).toEqual(expected);
    }
  }
});

test("batch retirement and compaction have bounded slot-array visits", () => {
  const fold = emptyFold();
  let entries = 0;
  const send = (event: object) => expect(foldEntry(fold, classifyStoredLine(++entries, JSON.stringify(event)))).toBe(true);
  send({ type: "user", message: { role: "user", content: "one long turn" } });
  const prompts = 400;
  for (let n = 0; n < prompts; n++) for (let copy = 0; copy < 3; copy++) send(native(`request-${n}`));
  const turn = fold.turns[0];
  if (turn === undefined) throw new Error("missing fixture turn");
  let visits = 0;
  let arrays = 0;
  const counted = new WeakSet<object>();
  for (let n = 0; n < prompts; n++) {
    if (!counted.has(turn.entries)) {
      const slots = new Proxy(turn.entries, {
        get(target, key, receiver) {
          if (typeof key === "string" && /^\d+$/.test(key)) visits++;
          return Reflect.get(target, key, receiver);
        },
      });
      counted.add(slots);
      turn.entries = slots;
      arrays++;
    }
    send(open(`request-${n}`, entries + 1));
  }
  expect(arrays).toBeGreaterThan(1);
  expect(visits / entries).toBeLessThan(20);
  expect(keptEntries(turn)).toHaveLength(prompts + 1);
});


test("a late tool-result regroup keeps pending permission locations attached to the rendered slots", () => {
  const { root, send } = transcript();
  send({ type: "user", message: { role: "user", content: "start" } });
  const result = (content: string) => ({ type: "user", message: { role: "user", content: [{ type: "tool_result", tool_use_id: "late-tool", content }] } });
  send(result("first"));
  send(native("pending"));
  send({ type: "assistant", message: { role: "assistant", content: [{ type: "tool_use", id: "late-tool", name: "Bash", input: { command: "pwd" } }] } });
  send(result("second"));
  expect(root.querySelectorAll(".event--control_request")).toHaveLength(1);
  send(open("pending"));
  expect(root.querySelectorAll(".event--control_request")).toHaveLength(0);
  expect(root.querySelectorAll(".event--flk_permission_request")).toHaveLength(1);
  expect(root.querySelectorAll('[data-testid="tool-run"]')).toHaveLength(1);
  expect(root.textContent).toContain("pwd");
  expect(root.textContent).toContain("second");
});


test("only the opening permission-only turn disappears; later user anchors survive", () => {
  const fold = emptyFold();
  let seq = 0;
  const send = (event: object) => foldEntry(fold, classifyStoredLine(++seq, JSON.stringify(event)));
  send(native("opening"));
  send({ type: "user", message: { role: "user", content: "first user" } });
  send(native("later"));
  send({ type: "user", message: { role: "user", content: "second user" } });
  send(open("opening"));
  expect(fold.turns.map((turn) => turn.key)).toEqual([2, 4]);
  expect(fold.permissionLocations[3]).toEqual({ turn: 0, slot: 1 });
  send(open("later"));
  expect(fold.turns.map((turn) => keptEntries(turn).map((entry) => entry.seq))).toEqual([[2], [4, 5, 6]]);
});

test("an empty non-opening permission turn is rejected as a broken invariant", () => {
  const fold = emptyFold();
  let seq = 0;
  const send = (event: object) => foldEntry(fold, classifyStoredLine(++seq, JSON.stringify(event)));
  send({ type: "user", message: { role: "user", content: "first user" } });
  send({ type: "user", message: { role: "user", content: "second user" } });
  send(native("broken"));
  const turn = fold.turns[1];
  const location = fold.permissionLocations[3];
  if (turn === undefined || location === undefined) throw new Error("missing indexed fixture turn");
  // Deliberate corruption: remove the user anchor, which place always supplies
  // to every non-opening turn. The guard must reject this impossible state.
  turn.entries.shift();
  location.slot = 0;
  expect(() => send(open("broken"))).toThrow("empty non-opening permission turn");
});

test("the opening turn's cursor crosses each retired leading slot only once", () => {
  const fold = emptyFold();
  let seq = 0;
  const send = (event: object) => foldEntry(fold, classifyStoredLine(++seq, JSON.stringify(event)));
  const prompts = 400;
  for (let n = 0; n < prompts; n++) send(native(`leading-${n}`));
  const turn = fold.turns[0];
  if (turn === undefined) throw new Error("missing opening fixture turn");
  let visits = 0;
  turn.entries = new Proxy(turn.entries, {
    get(target, key, receiver) {
      if (typeof key === "string" && /^\d+$/.test(key)) visits++;
      return Reflect.get(target, key, receiver);
    },
  });
  for (let n = 0; n < prompts; n++) send(open(`leading-${n}`, seq + 1));
  expect(keptEntries(turn)).toHaveLength(prompts);
  expect(visits / seq).toBeLessThan(20);
});
