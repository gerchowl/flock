import { expect, test } from "bun:test";
import { strict as assert } from "node:assert";

import type { FixtureLine } from "../src/gen/FixtureLine";
import { ExecutionLedger, WIRED_SCRIPTS, assertInventory, PENDING_PART_2B, coverage, loadS7MinScripts, runTrace, type CheckedStep, type StepHandlers, type TraceScript } from "./wire/endpoint-runner";
import { VirtualClock, parseScript } from "./wire/harness";

const ROOT = `${import.meta.dir}/../test/fixtures/wire`;
const script = (...middle: object[]): TraceScript => ({ name: "runner", lines: parseScript([
  { v: 1, step: "given", covers: ["s7-min"], cursors: {}, outbox: [] },
  ...middle,
  { v: 1, step: "quiet", ceiling_ms: 1000 },
].map((value) => JSON.stringify(value)).join("\n")) });
const checked = (source: CheckedStep["source"]): CheckedStep => ({ checked: true, source });
const base: StepHandlers = {
  given: async () => checked("setup"),
  quiet: async () => checked("clock"),
};

test("selection uses covers containment, including multi-label headers", async () => {
  const selected = await loadS7MinScripts(ROOT);
  assertInventory(selected, WIRED_SCRIPTS);
  expect(Object.keys(PENDING_PART_2B).sort()).toEqual(["agent-gap", "agent-send", "clean", "heartbeat-missed", "ping-pong", "token-revoked"]);
  expect(() => assertInventory(selected, [])).toThrow();
  expect(() => assertInventory(selected, [...WIRED_SCRIPTS, "clean"])).toThrow();
  expect(() => assertInventory([...selected, { ...selected[0]!, name: "new-unclassified" }], WIRED_SCRIPTS)).toThrow();
  const clientOnly = { name: "client-fleet", lines: parseScript(await Bun.file(`${ROOT}/fleet-lite.jsonl`).text()) };
  expect(coverage(clientOnly)).toEqual(["client-only"]);
  expect(selected.map((value) => value.name)).not.toContain("fleet-lite");
});

test("invalid, repeated or contradictory coverage labels cannot disappear from selection", () => {
  for (const covers of [[], ["s7-min", "s7-min"], ["future"], ["s7-min", "client-only"]]) {
    const value = script();
    const header = value.lines[0]!;
    assert(header.step === "given");
    Object.assign(header, { covers });
    expect(() => coverage(value)).toThrow();
  }
});

test("completion is ordered and waits for the action's observable assertion", async () => {
  let complete!: () => void;
  const gate = new Promise<void>((resolve) => { complete = resolve; });
  let enter!: () => void;
  const entered = new Promise<void>((resolve) => { enter = resolve; });
  const started: string[] = [];
  const running = runTrace(script({ v: 1, step: "client", action: { kind: "connect" } }), {
    ...base,
    client: async () => { started.push("action"); enter(); await gate; started.push("asserted"); return checked("client"); },
    quiet: async () => { started.push("quiet"); return checked("clock"); },
  });
  try {
    await Promise.race([entered, running]);
    expect(started).toEqual(["action"]);
  } finally { complete(); }
  expect((await running).map((fact) => [fact.index, fact.step])).toEqual([[0, "given"], [1, "client"], [2, "quiet"]]);
  expect(started).toEqual(["action", "asserted", "quiet"]);
});

test("coverage rejects a declared-but-unhandled step", async () => {
  const value = script({ v: 1, step: "observe", connection: "Live" });
  await expect(runTrace(value, { ...base, observe: async () => undefined }))
    .rejects.toThrow("runner: step 2 (observe): unhandled step observe: no checked receipt");
});

test("missing and unknown handlers fail at the exact authored index", async () => {
  for (const step of ["observe", "future", "toString"]) {
    await expect(runTrace(script({ v: 1, step }), base)).rejects.toThrow(`step 2 (${step}): unhandled step ${step}`);
  }
});

test("an assertion failure aborts before quiet and preserves its cause", async () => {
  let quiet = false;
  await expect(runTrace(script({ v: 1, step: "observe" }), {
    ...base,
    observe: async () => { throw new Error("actual capability mismatch"); },
    quiet: async () => { quiet = true; return checked("clock"); },
  })).rejects.toThrow("step 2 (observe): actual capability mismatch");
  expect(quiet).toBe(false);
});

test("expected nested values are immutable rather than normalized by a port", async () => {
  const value = script({ v: 1, step: "send", frame: { t: "ping", nonce: "fixture-ping" } });
  await expect(runTrace(value, { ...base, send: async (line) => {
    assert(line.step === "send" && line.frame.t === "ping");
    line.frame.nonce = "rewritten";
    return checked("server");
  } })).rejects.toThrow();
  const line = value.lines[1]!;
  assert(line.step === "send" && line.frame.t === "ping");
  expect(line.frame.nonce).toBe("fixture-ping");
});

test("server output and deliberately injected faults have distinct accounting", async () => {
  const value = script(
    { v: 1, step: "send", frame: { t: "ping", nonce: "fixture-ping" } },
    { v: 1, step: "send", frame: { t: "synced", sub_id: "never-opened", seq: 99 } },
  );
  let ordinal = 0;
  const facts = await runTrace(value, { ...base, send: async () => checked(ordinal++ === 0 ? "server" : "client_fault") });
  expect(facts.map((fact) => fact.source)).toEqual(["setup", "server", "client_fault", "clock"]);
  await expect(runTrace(value, { ...base, send: async () => checked("observation") })).rejects.toThrow("no server/fault provenance");
});

test("a non-send cannot claim server or fault provenance", async () => {
  await expect(runTrace(script(), { ...base, given: async () => checked("server") })).rejects.toThrow("non-send");
});

test("ledger refuses missing, duplicate and out-of-order completions", () => {
  const ledger = new ExecutionLedger(script());
  expect(() => ledger.finish()).toThrow("unhandled step 1 of 2");
  expect(() => ledger.complete(1, checked("clock"))).toThrow("out-of-order");
  ledger.complete(0, checked("setup"));
  expect(() => ledger.complete(0, checked("setup"))).toThrow("out-of-order");
  expect(() => ledger.finish()).toThrow("unhandled step 2 of 2");
  ledger.complete(1, checked("clock"));
  expect(ledger.finish()).toHaveLength(2);
});

test("a silent asynchronous port fails at a separate guard-clock ceiling", async () => {
  const guard = new VirtualClock();
  const running = runTrace(script(), { ...base, given: () => new Promise(() => {}) }, guard);
  const failure = running.then(() => { throw new Error("silent port incorrectly completed"); }, (error: Error) => error);
  guard.advanceTo(5000);
  expect((await failure).message).toContain("step 1 (given): asynchronous trace port stayed silent");
});

test("invalid versions, repeated given and missing final quiet fail visibly", async () => {
  const repeated = script({ v: 1, step: "given", covers: ["s7-min"], cursors: {}, outbox: [] });
  await expect(runTrace(repeated, base)).rejects.toThrow("repeated given");
  await expect(runTrace({ name: "empty", lines: [] }, base)).rejects.toThrow("expected v1 given");
  await expect(runTrace({ name: "no-quiet", lines: script().lines.slice(0, 1) }, base)).rejects.toThrow("missing final quiet");
  await expect(runTrace(script({ v: 2, step: "quiet", ceiling_ms: 1 }), base)).rejects.toThrow("invalid version");
});
