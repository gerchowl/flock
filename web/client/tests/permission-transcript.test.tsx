/** Permission facts reach the real transcript through its transport; no timers or server login. */
import { afterEach, expect, test } from "bun:test";
import { render } from "solid-js/web";

import type { CanUseTool } from "../src/gen/CanUseTool";
import { permissionLifecycle } from "../src/gen/FixtureExamples";
import type { SessionEntry, Transport } from "../src/transport";
import { classifyStoredLine } from "../src/transport/fixture";
import { Transcript } from "../src/views/Transcript";

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
