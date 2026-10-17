/**
 * The wire fleet source over S7-min's `fleet` snapshot stream (#1746).
 * All time is virtual (ADR-0037).
 */
import { describe, expect, test } from "bun:test";

import type { ServerFrame } from "../src/gen/ServerFrame";
import type { AgentRow } from "../src/fleet";
import { createWireFleet } from "../src/wire/fleet";
import { Harness, live, sentFrames, step } from "./wire/harness";

type Seen = { readonly rows: readonly string[] } | { readonly failed: string };

const row = (id: string, extra: object = {}): object => ({ id, label: id.toUpperCase(), state: "working", waiting_on_human: [], ...extra });
const upsert = (seq: number, value: object): ServerFrame => ({ t: "ev", sub_id: "f1", stream: "fleet", seq, ts_ms: 0, ev: { type: "fleet.upsert", row: value } as never });
const remove = (seq: number, id: string): ServerFrame => ({ t: "ev", sub_id: "f1", stream: "fleet", seq, ts_ms: 0, ev: { type: "fleet.remove", id } });

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
  expect(sentFrames(h).at(-1)).toEqual({ t: "sub", sub_id: "f1", stream: "fleet" });
  return { h, seen, last: () => rows, close };
}

const send = (h: Harness, frame: ServerFrame): void => h.step(step({ step: "send", frame }));

describe("wire fleet", () => {
  test("a snapshot is shown whole at its end, then deltas apply one row each", () => {
    const { h, seen, last } = subscribed();
    send(h, { t: "gap", sub_id: "f1", from: 1, to: 0, snapshot_to: 2 });
    send(h, upsert(1, row("atlas", { activity: "editing", waiting_on_human: ["r1"] })));
    expect(seen).toEqual([]);
    send(h, upsert(2, row("kite")));
    send(h, { t: "synced", sub_id: "f1", seq: 2 });
    expect(last()).toEqual([
      { id: "atlas", label: "ATLAS", state: "working", activity: "editing", waitingOnHuman: ["r1"] },
      { id: "kite", label: "KITE", state: "working", activity: "", waitingOnHuman: [] },
    ]);
    send(h, upsert(3, row("kite", { state: "needs_human" })));
    send(h, remove(4, "atlas"));
    expect(seen).toEqual([{ rows: ["atlas", "kite"] }, { rows: ["atlas", "kite"] }, { rows: ["kite"] }]);
    expect(last()[0]?.state).toBe("needs_human");
  });

  test("a later snapshot replaces every row, so a missing agent is gone", () => {
    const { h, seen } = subscribed();
    send(h, { t: "gap", sub_id: "f1", from: 1, to: 0, snapshot_to: 2 });
    send(h, upsert(1, row("atlas")));
    send(h, upsert(2, row("kite")));
    send(h, { t: "synced", sub_id: "f1", seq: 2 });
    send(h, { t: "gap", sub_id: "f1", from: 3, to: 8, snapshot_to: 9 });
    send(h, upsert(9, row("wren")));
    expect(seen.at(-1)).toEqual({ rows: ["wren"] });
  });

  test("an empty fleet is an empty list, not a wait", () => {
    const { h, seen } = subscribed();
    send(h, { t: "gap", sub_id: "f1", from: 1, to: 0, snapshot_to: 0 });
    expect(seen).toEqual([{ rows: [] }]);
  });

  test("a payload that is not a fleet delta ends the list visibly", () => {
    for (const bad of [{ type: "fleet.upsert", row: row("a", { state: "dancing" }) }, { type: "fleet.upsert", row: { id: "a" } }, { type: "fleet.rename", id: "a" }, "atlas"]) {
      const { h, seen } = subscribed();
      send(h, { t: "gap", sub_id: "f1", from: 1, to: 0, snapshot_to: 0 });
      send(h, { t: "ev", sub_id: "f1", stream: "fleet", seq: 1, ts_ms: 0, ev: bad as never });
      expect(seen.at(-1)).toEqual({ failed: expect.stringContaining("not a fleet delta") });
      expect(sentFrames(h).at(-1)).toEqual({ t: "unsub", sub_id: "f1" });
      expect(h.connection.streams().has("f1")).toBe(false);
    }
  });

  test("a bad snapshot line shows no partial list, and leaves no deadline behind", () => {
    const { h, seen } = subscribed();
    send(h, { t: "gap", sub_id: "f1", from: 1, to: 0, snapshot_to: 2 });
    send(h, upsert(1, row("atlas")));
    send(h, { t: "ev", sub_id: "f1", stream: "fleet", seq: 2, ts_ms: 0, ev: { id: "x" } });
    expect(seen).toEqual([{ failed: expect.stringContaining("not a fleet delta") }]);
    h.clock.advanceTo(h.clock.now() + 120_000);
    expect(sentFrames(h).filter((frame) => (frame as { t: string }).t === "sub")).toHaveLength(1);
  });

  test("a refusal is a visible failure; closing unsubscribes", () => {
    const refused = subscribed();
    send(refused.h, { t: "subfail", sub_id: "f1", code: "scope", message: "This token cannot read the fleet." });
    expect(refused.seen).toEqual([{ failed: "This token cannot read the fleet." }]);

    const closed = subscribed();
    closed.close();
    expect(sentFrames(closed.h).at(-1)).toEqual({ t: "unsub", sub_id: "f1" });
    send(closed.h, { t: "gap", sub_id: "f1", from: 1, to: 0, snapshot_to: 0 });
    expect(closed.seen).toEqual([]);
  });
});
