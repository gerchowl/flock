/**
 * Streams on the connection and the {@link Transport} adapter (#1747,
 * second slice). All time is virtual (ADR-0037).
 */
import { describe, expect, test } from "bun:test";

import type { ServerFrame } from "../src/gen/ServerFrame";
import type { SessionEntry } from "../src/transport";
import { createWireTransport } from "../src/wire/transport";
import { Harness, WELCOME, expectNeedsToken, live, sentFrames, step } from "./wire/harness";

test("auth refusal → NeedsToken from Syncing too", () => {
  for (const code of [4401, 4403]) {
    const h = new Harness();
    live(h);
    h.step(step({ step: "client", action: { kind: "subscribe", sub_id: "s1", stream: "agent/a" } }));
    expectNeedsToken(h, "Syncing", code);
  }
});

/** A fleet snapshot line or delta; the connection does not look inside. */
const fleetLine = (seq: number, id: string): ServerFrame => ({ t: "ev", sub_id: "s1", stream: "fleet", seq, ts_ms: 0, ev: { id } });

describe("streams", () => {
  test("a history gap leaves a marker and continues after it", () => {
    const h = new Harness();
    live(h);
    h.step(step({ step: "client", action: { kind: "subscribe", sub_id: "s1", stream: "agent/a" } }));
    h.step(step({ step: "send", frame: { t: "gap", sub_id: "s1", from: 1, to: 4 } }));
    h.step(step({ step: "send", frame: { t: "ev", sub_id: "s1", stream: "agent/a", seq: 5, ts_ms: 0, ev: { type: "assistant", message: {} } } }));
    h.step(step({ step: "send", frame: { t: "synced", sub_id: "s1", seq: 5 } }));
    expect(h.seen.get("s1")?.map((seen) => seen.kind)).toEqual(["gap", "event", "synced"]);
    expect(h.connection.streams().get("s1")).toEqual({ stream: "agent/a", state: "Current", cursor: 5 });
  });

  test("a snapshot gap on a history stream is a protocol error", () => {
    const h = new Harness();
    live(h);
    h.step(step({ step: "client", action: { kind: "subscribe", sub_id: "s1", stream: "agent/a" } }));
    h.step(step({ step: "send", frame: { t: "gap", sub_id: "s1", from: 1, to: 0, snapshot_to: 0 } }));
    expect(h.connection.state).toBe("Reconnecting");
  });

  test("a snapshot is Resyncing until its end, then applied whole", () => {
    const h = new Harness();
    live(h);
    h.step(step({ step: "client", action: { kind: "subscribe", sub_id: "s1", stream: "fleet" } }));
    h.step(step({ step: "send", frame: { t: "gap", sub_id: "s1", from: 1, to: 2, snapshot_to: 4 } }));
    h.step(step({ step: "send", frame: fleetLine(3, "a") }));
    h.step(step({ step: "send", frame: fleetLine(3, "a") }));
    expect(h.connection.streams().get("s1")).toEqual({ stream: "fleet", state: "Resyncing", cursor: null });
    expect(h.seen.get("s1")).toEqual([]);
    h.step(step({ step: "send", frame: fleetLine(4, "b") }));
    expect(h.seen.get("s1")).toEqual([{ kind: "snapshot", lines: [{ id: "a" }, { id: "b" }] }]);
    expect(h.connection.streams().get("s1")).toEqual({ stream: "fleet", state: "Replaying", cursor: 4 });
    h.step(step({ step: "send", frame: { t: "synced", sub_id: "s1", seq: 4 } }));
    h.step(step({ step: "send", frame: fleetLine(5, "c") }));
    expect(h.seen.get("s1")?.slice(1)).toEqual([{ kind: "synced" }, { kind: "event", seq: 5, ev: { id: "c" } }]);
  });

  test("a newer snapshot supersedes an unfinished one", () => {
    const h = new Harness();
    live(h);
    h.step(step({ step: "client", action: { kind: "subscribe", sub_id: "s1", stream: "fleet" } }));
    h.step(step({ step: "send", frame: { t: "gap", sub_id: "s1", from: 1, to: 2, snapshot_to: 4 } }));
    h.step(step({ step: "send", frame: fleetLine(3, "old") }));
    h.step(step({ step: "send", frame: { t: "gap", sub_id: "s1", from: 1, to: 5, snapshot_to: 6 } }));
    h.step(step({ step: "send", frame: fleetLine(6, "new") }));
    expect(h.seen.get("s1")).toEqual([{ kind: "snapshot", lines: [{ id: "new" }] }]);
  });

  test("a re-sub during Resyncing drops the unfinished snapshot and keeps the old cursor", () => {
    const h = new Harness();
    live(h);
    h.step(step({ step: "client", action: { kind: "subscribe", sub_id: "s1", stream: "fleet" } }));
    h.step(step({ step: "send", frame: { t: "gap", sub_id: "s1", from: 1, to: 2, snapshot_to: 4 } }));
    h.step(step({ step: "send", frame: fleetLine(3, "a") }));
    h.step(step({ step: "send", frame: { t: "lagged", sub_id: "s1", skipped_to: 9 } }));
    expect(sentFrames(h).at(-1)).toEqual({ t: "sub", sub_id: "s1", stream: "fleet" });
    expect(h.connection.streams().get("s1")).toEqual({ stream: "fleet", state: "Replaying", cursor: null });
    h.step(step({ step: "send", frame: { t: "gap", sub_id: "s1", from: 1, to: 8, snapshot_to: 9 } }));
    h.step(step({ step: "send", frame: fleetLine(9, "b") }));
    expect(h.seen.get("s1")).toEqual([{ kind: "snapshot", lines: [{ id: "b" }] }]);
  });

  test("synced before the snapshot's end, or a skipped line, is a protocol error", () => {
    for (const early of [{ t: "synced", sub_id: "s1", seq: 4 }, fleetLine(4, "b")] satisfies ServerFrame[]) {
      const h = new Harness();
      live(h);
      h.step(step({ step: "client", action: { kind: "subscribe", sub_id: "s1", stream: "fleet" } }));
      h.step(step({ step: "send", frame: { t: "gap", sub_id: "s1", from: 1, to: 2, snapshot_to: 4 } }));
      h.step(step({ step: "send", frame: early }));
      expect(h.connection.state).toBe("Reconnecting");
      expect(h.seen.get("s1")).toEqual([]);
    }
  });

  test("a stalled snapshot re-subscribes at the progress deadline", () => {
    const h = new Harness();
    live(h);
    h.step(step({ step: "client", action: { kind: "subscribe", sub_id: "s1", stream: "fleet" } }));
    h.step(step({ step: "send", frame: { t: "gap", sub_id: "s1", from: 1, to: 2, snapshot_to: 4 } }));
    h.clock.advanceTo(h.clock.now() + 29_999);
    h.step(step({ step: "send", frame: fleetLine(3, "a") }));
    h.clock.advanceTo(h.clock.now() + 29_999);
    expect(h.connection.streams().get("s1")?.state).toBe("Resyncing");
    h.clock.advanceTo(h.clock.now() + 1);
    expect(sentFrames(h).slice(-2)).toEqual([
      { t: "unsub", sub_id: "s1" },
      { t: "sub", sub_id: "s1", stream: "fleet" },
    ]);
  });

  test("duplicates are dropped and the cursor advances only after the handler applied the event", () => {
    const h = new Harness();
    live(h);
    const cursors: (number | null)[] = [];
    h.connection.subscribe("s1", "agent/a", {
      event: () => cursors.push(h.connection.streams().get("s1")?.cursor ?? null),
      gap: () => undefined,
      synced: () => undefined,
      snapshot: () => undefined,
      reset: () => undefined,
      refused: () => undefined,
    });
    const ev = (seq: number): ServerFrame => ({ t: "ev", sub_id: "s1", stream: "agent/a", seq, ts_ms: 0, ev: { type: "user", message: {} } });
    h.step(step({ step: "send", frame: ev(1) }));
    h.step(step({ step: "send", frame: ev(1) }));
    h.step(step({ step: "send", frame: ev(2) }));
    expect(cursors).toEqual([null, 1]);
  });

  test("a new sub in Live goes back to Syncing; a lagged re-sub keeps Live", () => {
    const h = new Harness();
    live(h);
    h.step(step({ step: "client", action: { kind: "subscribe", sub_id: "s1", stream: "agent/a" } }));
    expect(h.connection.state).toBe("Syncing");
    h.step(step({ step: "send", frame: { t: "synced", sub_id: "s1", seq: 0 } }));
    h.step(step({ step: "send", frame: { t: "lagged", sub_id: "s1", skipped_to: 9 } }));
    expect(h.connection.state).toBe("Live");
    expect(h.connection.streams().get("s1")?.state).toBe("Replaying");
  });
});

describe("wire transport", () => {
  test("maps events, the unknown wrapper, gaps, one synced and refusals", () => {
    const h = new Harness();
    live(h);
    const transport = createWireTransport(h.connection);
    const seen: (SessionEntry | string)[] = [];
    transport.subscribe("agent/a", null, {
      entry: (entry) => seen.push(entry),
      synced: () => seen.push("synced"),
      failed: (failure) => seen.push(`failed: ${failure.message}`),
    });
    const sub = (sentFrames(h).at(-1) as { sub_id: string }).sub_id;
    const send = (frame: ServerFrame): void => h.step(step({ step: "send", frame }));
    send({ t: "gap", sub_id: sub, from: 1, to: 2 });
    send({ t: "ev", sub_id: sub, stream: "agent/a", seq: 3, ts_ms: 0, ev: { type: "user", message: {} } });
    send({ t: "ev", sub_id: sub, stream: "agent/a", seq: 4, ts_ms: 0, ev: { type: "unknown", raw: "x y", shape: "claude/2:x" } });
    send({ t: "synced", sub_id: sub, seq: 4 });
    send({ t: "lagged", sub_id: sub, skipped_to: 9 });
    send({ t: "synced", sub_id: sub, seq: 4 });
    send({ t: "subfail", sub_id: sub, code: "unknown_stream", message: "agent removed" });
    send({ t: "ev", sub_id: sub, stream: "agent/a", seq: 5, ts_ms: 0, ev: { type: "user", message: {} } });
    expect(seen).toEqual([
      { seq: 2, kind: "gap", from: 1, to: 2 },
      { seq: 3, kind: "known", event: { type: "user", message: {} } as never },
      { seq: 4, kind: "unknown", unknown: { raw: "x y", shape: "claude/2:x" } },
      "synced",
      "failed: agent removed",
    ]);
  });

  test("subscribes from the caller's cursor, and survives a reconnect from the applied one", () => {
    const h = new Harness();
    live(h);
    createWireTransport(h.connection).subscribe("agent/a", 7, { entry: () => undefined, synced: () => undefined, failed: () => undefined });
    expect(sentFrames(h).at(-1)).toEqual({ t: "sub", sub_id: "t1", stream: "agent/a", after: 7 });
    h.step(step({ step: "send", frame: { t: "ev", sub_id: "t1", stream: "agent/a", seq: 8, ts_ms: 0, ev: { type: "user", message: {} } } }));
    h.step(step({ step: "drop" }));
    h.step(step({ step: "expect_connect", within_ms: 500, not_before_ms: 250 }));
    h.step(step({ step: "open" }));
    h.step(step({ step: "send", frame: WELCOME }));
    expect(sentFrames(h).at(-1)).toEqual({ t: "sub", sub_id: "t1", stream: "agent/a", after: 8 });
  });
});
