/**
 * The wire fleet source over S7-min's `fleet` snapshot stream (#1746).
 * All time is virtual (ADR-0037).
 */
import { describe, expect, test } from "bun:test";

import type { FleetAgentRow } from "../src/gen/FleetAgentRow";
import type { AgentState } from "../src/gen/AgentState";
import type { Phase } from "../src/gen/Phase";
import type { ServerFrame } from "../src/gen/ServerFrame";
import type { AgentRow } from "../src/fleet";
import { fixtureExamples } from "../src/gen/FixtureExamples";
import { createWireFleet } from "../src/wire/fleet";
import { Harness, live, sentFrames, step } from "./wire/harness";

type Seen = { readonly rows: readonly string[] } | { readonly failed: string };

const agentState = (phase: Phase): AgentState => ({ turn: "idle", background: "none", attention: "none", waiting: "none", lifecycle: "live", phase, source: {} });
const row = (id: string, extra: object = {}): object => ({ id, label: id.toUpperCase(), state: agentState("working"), waiting_on_human: [], ...extra });
const upsert = (seq: number, value: object): ServerFrame => ({ t: "ev", sub_id: "s1", stream: "fleet", seq, ts_ms: 0, ev: { type: "fleet.upsert", row: value } as never });
const remove = (seq: number, id: string): ServerFrame => ({ t: "ev", sub_id: "s1", stream: "fleet", seq, ts_ms: 0, ev: { type: "fleet.remove", id } });

function subscribed(): { h: Harness; seen: Seen[]; last: () => readonly AgentRow[]; close: () => void } {
  const h = new Harness();
  live(h);
  const seen: Seen[] = [];
  let rows: readonly AgentRow[] = [];
  const close = createWireFleet(h.connection).subscribe({
    rows: (next) => {
      rows = next;
      seen.push({ rows: next.map((r) => r.id) });
    },
    failed: (message) => seen.push({ failed: message }),
  });
  expect(sentFrames(h).at(-1)).toEqual({ t: "sub", sub_id: "s1", stream: "fleet" });
  return { h, seen, last: () => rows, close };
}

const send = (h: Harness, frame: ServerFrame): void => h.step(step({ step: "send", frame }));

describe("wire fleet", () => {
  test("optional mesh metadata is compatible with local-only fleet presentation", () => {
    const routed: FleetAgentRow = {
      id: "fixture-agent", label: "Example", state: agentState("working"), waiting_on_human: [],
      route: { node: "5f23ba66a62a6aeffca866a461c0686ae7f869c685e1ea1f69b2b84e84de5bb0", name: "origin.example", next_hop: "5f23ba66a62a6aeffca866a461c0686ae7f869c685e1ea1f69b2b84e84de5bb0", hops: 1 },
    };
    const { h, seen, last } = subscribed();
    send(h, { t: "gap", sub_id: "s1", from: 1, to: 0, snapshot_to: 1 });
    send(h, upsert(1, routed));
    send(h, { t: "synced", sub_id: "s1", seq: 1 });
    expect(seen).toEqual([{ rows: ["fixture-agent"] }]);
    expect(last()[0]?.label).toBe("Example");
  });

  test("a snapshot is shown whole at its end, then deltas apply one row each", () => {
    const { h, seen, last } = subscribed();
    send(h, { t: "gap", sub_id: "s1", from: 1, to: 0, snapshot_to: 2 });
    send(h, upsert(1, row("atlas", { activity: "editing", waiting_on_human: ["r1"] })));
    expect(seen).toEqual([]);
    send(h, upsert(2, row("kite")));
    send(h, { t: "synced", sub_id: "s1", seq: 2 });
    expect(last()).toEqual([
      { id: "atlas", label: "ATLAS", state: "working", activity: "editing", waitingOnHuman: ["r1"] },
      { id: "kite", label: "KITE", state: "working", activity: "", waitingOnHuman: [] },
    ]);
    send(h, upsert(3, row("kite", { state: agentState("needs_human") })));
    send(h, remove(4, "atlas"));
    expect(seen).toEqual([{ rows: ["atlas", "kite"] }, { rows: ["atlas", "kite"] }, { rows: ["kite"] }]);
    expect(last()[0]?.state).toBe("needs_human");
  });

  test("the real fleet-lite frames, with their AgentState object, become rows", () => {
    const { h, seen, last } = subscribed();
    const frames = fixtureExamples["fleet-lite"].flatMap((line) => (line.step === "send" && line.frame?.t === "ev" && line.frame.sub_id === "s1" ? [line.frame] : []));
    expect(frames.map((frame) => (frame.ev as { type: string }).type)).toEqual(["fleet.upsert", "fleet.upsert", "fleet.upsert", "fleet.remove"]);
    send(h, { t: "gap", sub_id: "s1", from: 1, to: 0, snapshot_to: 2 });
    send(h, frames[0] as ServerFrame);
    send(h, frames[1] as ServerFrame);
    send(h, { t: "synced", sub_id: "s1", seq: 2 });
    expect(last()).toEqual([
      { id: "fixture-agent", label: "Example", state: "working", activity: "Reading a file", waitingOnHuman: [] },
      { id: "second-fixture-agent", label: "Second", state: "needs_human", activity: "", waitingOnHuman: ["request-one"] },
    ]);
    send(h, frames[2] as ServerFrame);
    expect(last()[0]?.activity).toBe("Writing a file");
    send(h, frames[3] as ServerFrame);
    expect(last().map((r) => r.id)).toEqual(["fixture-agent"]);
    expect(seen.every((entry) => "rows" in entry)).toBe(true);
  });

  test("a flat-string state, the old hand-written shape, is not a fleet delta", () => {
    const { h, seen } = subscribed();
    send(h, { t: "gap", sub_id: "s1", from: 1, to: 0, snapshot_to: 1 });
    send(h, upsert(1, row("atlas", { state: "working" })));
    expect(seen).toEqual([{ failed: expect.stringContaining("not a fleet delta") }]);
  });

  test("a later snapshot replaces every row, so a missing agent is gone", () => {
    const { h, seen } = subscribed();
    send(h, { t: "gap", sub_id: "s1", from: 1, to: 0, snapshot_to: 2 });
    send(h, upsert(1, row("atlas")));
    send(h, upsert(2, row("kite")));
    send(h, { t: "synced", sub_id: "s1", seq: 2 });
    send(h, { t: "gap", sub_id: "s1", from: 3, to: 8, snapshot_to: 9 });
    send(h, upsert(9, row("wren")));
    expect(seen.at(-1)).toEqual({ rows: ["wren"] });
  });

  test("an empty fleet is an empty list, not a wait", () => {
    const { h, seen } = subscribed();
    send(h, { t: "gap", sub_id: "s1", from: 1, to: 0, snapshot_to: 0 });
    expect(seen).toEqual([{ rows: [] }]);
  });

  test("a payload that is not a fleet delta ends the list visibly", () => {
    for (const bad of [{ type: "fleet.upsert", row: row("a", { state: "working" }) }, { type: "fleet.upsert", row: { id: "a" } }, { type: "fleet.rename", id: "a" }, "atlas"]) {
      const { h, seen } = subscribed();
      send(h, { t: "gap", sub_id: "s1", from: 1, to: 0, snapshot_to: 0 });
      send(h, { t: "ev", sub_id: "s1", stream: "fleet", seq: 1, ts_ms: 0, ev: bad as never });
      expect(seen.at(-1)).toEqual({ failed: expect.stringContaining("not a fleet delta") });
      expect(sentFrames(h).at(-1)).toEqual({ t: "unsub", sub_id: "s1" });
      expect(h.connection.streams().has("s1")).toBe(false);
    }
  });

  test("a bad snapshot line shows no partial list, and leaves no deadline behind", () => {
    const { h, seen } = subscribed();
    send(h, { t: "gap", sub_id: "s1", from: 1, to: 0, snapshot_to: 2 });
    send(h, upsert(1, row("atlas")));
    send(h, { t: "ev", sub_id: "s1", stream: "fleet", seq: 2, ts_ms: 0, ev: { id: "x" } });
    expect(seen).toEqual([{ failed: expect.stringContaining("not a fleet delta") }]);
    h.clock.advanceTo(h.clock.now() + 120_000);
    expect(sentFrames(h).filter((frame) => (frame as { t: string }).t === "sub")).toHaveLength(1);
  });

  test("a refusal is a visible failure; closing unsubscribes", () => {
    const refused = subscribed();
    send(refused.h, { t: "subfail", sub_id: "s1", code: "scope", message: "This token cannot read the fleet." });
    expect(refused.seen).toEqual([{ failed: "This token cannot read the fleet." }]);

    const closed = subscribed();
    closed.close();
    expect(sentFrames(closed.h).at(-1)).toEqual({ t: "unsub", sub_id: "s1" });
    send(closed.h, { t: "gap", sub_id: "s1", from: 1, to: 0, snapshot_to: 0 });
    expect(closed.seen).toEqual([]);
  });
});

test("fleet sources on one connection never reuse a sub id, and a closed one's late frames are dropped", () => {
  const h = new Harness();
  live(h);
  const quiet = { rows: () => undefined, failed: () => undefined };
  createWireFleet(h.connection).subscribe(quiet)();
  const seen: string[][] = [];
  createWireFleet(h.connection).subscribe({ rows: (rows) => seen.push(rows.map((r) => r.id)), failed: (message) => seen.push([message]) });
  const subs = sentFrames(h).flatMap((frame) => ((frame as { t: string }).t === "sub" ? [(frame as { sub_id: string }).sub_id] : []));
  expect(new Set(subs).size).toBe(2);
  const current = subs[1] ?? "";
  send(h, { t: "gap", sub_id: current, from: 1, to: 0, snapshot_to: 1 });
  send(h, { ...upsert(1, row("kite")), sub_id: current } as ServerFrame);
  send(h, { t: "synced", sub_id: current, seq: 1 });
  expect(h.connection.state).toBe("Live");
  send(h, { t: "gap", sub_id: "s1", from: 1, to: 0, snapshot_to: 1 });
  send(h, upsert(1, row("stale")));
  expect(h.connection.state).toBe("Live");
  expect(seen).toEqual([["kite"]]);
});

test("generated S4 fleet state reaches presentation without discarding the row", () => {
  const { h, last } = subscribed();
  send(h, { t: "gap", sub_id: "s1", from: 1, to: 0, snapshot_to: 1 });
  send(h, upsert(1, row("fixture-agent", { state: { turn: "working", background: "none", attention: "none", waiting: "none", lifecycle: "live", phase: "working", source: {} } })));
  expect(last().map((value) => [value.id, value.state])).toEqual([["fixture-agent", "working"]]);
});

test("a newer phase keeps the fleet row and renders unknown", () => {
  const { h, last } = subscribed();
  send(h, { t: "gap", sub_id: "s1", from: 1, to: 0, snapshot_to: 1 });
  send(h, upsert(1, row("fixture-agent", { state: agentState("dancing" as Phase) })));
  expect(last().map((value) => [value.id, value.state])).toEqual([["fixture-agent", "unknown"]]);
});
