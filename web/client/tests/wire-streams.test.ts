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

/** The id of the latest `sub` sent: a same-socket re-sub moves a stream to a fresh one. */
const lastSub = (h: Harness): string => (sentFrames(h).filter((frame) => (frame as { t: string }).t === "sub").at(-1) as { sub_id: string }).sub_id;

/** The ids of every `sub` sent, in order. */
const subIds = (h: Harness): string[] => sentFrames(h).flatMap((frame) => ((frame as { t: string }).t === "sub" ? [(frame as { sub_id: string }).sub_id] : []));

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
    expect(sentFrames(h).at(-1)).toEqual({ t: "sub", sub_id: "s2", stream: "fleet" });
    expect(h.connection.streams().get("s2")).toEqual({ stream: "fleet", state: "Replaying", cursor: null });
    h.step(step({ step: "send", frame: { t: "gap", sub_id: "s2", from: 1, to: 8, snapshot_to: 9 } }));
    h.step(step({ step: "send", frame: { ...fleetLine(9, "b"), sub_id: "s2" } as ServerFrame }));
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

  test("repeated identical snapshot gaps do not reset the progress deadline", () => {
    const h = new Harness();
    live(h);
    h.step(step({ step: "client", action: { kind: "subscribe", sub_id: "s1", stream: "fleet" } }));
    const gap: ServerFrame = { t: "gap", sub_id: "s1", from: 1, to: 2, snapshot_to: 4 };
    h.step(step({ step: "send", frame: gap }));
    for (let i = 0; i < 3; i++) {
      h.clock.advanceTo(h.clock.now() + 10_000);
      h.step(step({ step: "send", frame: gap }));
    }
    expect(sentFrames(h).slice(-2)).toEqual([
      { t: "unsub", sub_id: "s1" },
      { t: "sub", sub_id: "s2", stream: "fleet" },
    ]);
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
      { t: "sub", sub_id: "s2", stream: "fleet" },
    ]);
  });

  test("duplicates are dropped and the cursor advances only after the handler applied the event", () => {
    const h = new Harness();
    live(h);
    const cursors: (number | null)[] = [];
    h.connection.subscribe("agent/a", {
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
    expect(h.connection.streams().get("s1")).toBeUndefined();
    expect(h.connection.streams().get("s2")?.state).toBe("Replaying");
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
    const resub = lastSub(h);
    expect(resub).not.toBe(sub);
    send({ t: "synced", sub_id: resub, seq: 4 });
    send({ t: "subfail", sub_id: resub, code: "unknown_stream", message: "agent removed" });
    send({ t: "ev", sub_id: resub, stream: "agent/a", seq: 5, ts_ms: 0, ev: { type: "user", message: {} } });
    expect(seen).toEqual([
      { seq: 2, kind: "gap", from: 1, to: 2 },
      { seq: 3, kind: "known", event: { type: "user", message: {} } as never },
      { seq: 4, kind: "unknown", unknown: { raw: "x y", shape: "claude/2:x" } },
      "synced",
      "failed: agent removed",
    ]);
    expect(sentFrames(h).filter((frame) => (frame as { t: string }).t === "unsub")).toEqual([]);
    expect(h.connection.state).toBe("Live");
  });

  test("a stale gap after a lagged re-sub is dropped, so seqs only increase and no marker repeats", () => {
    const h = new Harness();
    live(h);
    const seen: SessionEntry[] = [];
    createWireTransport(h.connection).subscribe("agent/a", null, { entry: (entry) => seen.push(entry), synced: () => undefined, failed: () => undefined });
    const sub = (sentFrames(h).at(-1) as { sub_id: string }).sub_id;
    const send = (frame: ServerFrame): void => h.step(step({ step: "send", frame }));
    send({ t: "gap", sub_id: sub, from: 1, to: 4 });
    send({ t: "ev", sub_id: sub, stream: "agent/a", seq: 5, ts_ms: 0, ev: { type: "user", message: {} } });
    send({ t: "lagged", sub_id: sub, skipped_to: 9 });
    send({ t: "gap", sub_id: lastSub(h), from: 1, to: 4 });
    send({ t: "gap", sub_id: lastSub(h), from: 3, to: 7 });
    expect(seen.map((entry) => [entry.kind, entry.seq])).toEqual([
      ["gap", 4],
      ["known", 5],
      ["gap", 7],
    ]);
    expect(seen[2]).toEqual({ seq: 7, kind: "gap", from: 6, to: 7 });
  });

  test("subscribes from the caller's cursor, and survives a reconnect from the applied one", () => {
    const h = new Harness();
    live(h);
    createWireTransport(h.connection).subscribe("agent/a", 7, { entry: () => undefined, synced: () => undefined, failed: () => undefined });
    expect(sentFrames(h).at(-1)).toEqual({ t: "sub", sub_id: "s1", stream: "agent/a", after: 7 });
    h.step(step({ step: "send", frame: { t: "ev", sub_id: "s1", stream: "agent/a", seq: 8, ts_ms: 0, ev: { type: "user", message: {} } } }));
    h.step(step({ step: "drop" }));
    h.step(step({ step: "expect_connect", within_ms: 500, not_before_ms: 250 }));
    h.step(step({ step: "open" }));
    h.step(step({ step: "send", frame: WELCOME }));
    expect(sentFrames(h).at(-1)).toEqual({ t: "sub", sub_id: "s1", stream: "agent/a", after: 8 });
  });
});

describe("sub ids (ADR-0038 §3, sub ids and closed subs)", () => {
  const quiet = { entry: () => undefined, synced: () => undefined, failed: () => undefined };

  test("transports on one connection never reuse a sub id, even across agent switches", () => {
    const h = new Harness();
    live(h);
    createWireTransport(h.connection).subscribe("agent/a", null, quiet).close();
    createWireTransport(h.connection).subscribe("agent/b", null, quiet).close();
    createWireTransport(h.connection).subscribe("agent/c", null, quiet);
    const ids = subIds(h);
    expect(ids).toHaveLength(3);
    expect(new Set(ids).size).toBe(3);
  });

  test("the connection allocates every id: one opened after an automatic re-sub never collides with it (#1816)", () => {
    const h = new Harness();
    live(h);
    const handler = { event: () => undefined, gap: () => undefined, synced: () => undefined, snapshot: () => undefined, reset: () => undefined, refused: () => undefined };
    const a = h.connection.subscribe("agent/a", handler);
    expect(a.id).toBe("s1");
    h.step(step({ step: "send", frame: { t: "lagged", sub_id: "s1", skipped_to: 9 } }));
    // The re-sub took s2, the very id a caller picking "the next one" by hand would have chosen.
    expect(a.id).toBe("s2");
    const b = h.connection.subscribe("agent/b", handler);
    expect(b.id).toBe("s3");
    expect([...h.connection.streams()].map(([id, status]) => [id, status.stream])).toEqual([
      ["s2", "agent/a"],
      ["s3", "agent/b"],
    ]);
    a.close();
    b.close();
    expect(subIds(h)).toEqual(["s1", "s2", "s3"]);
  });

  test("late frames for a closed sub are dropped: the connection stays Live and the next agent is untouched", () => {
    const h = new Harness();
    live(h);
    const send = (frame: ServerFrame): void => h.step(step({ step: "send", frame }));
    const a: string[] = [];
    const first = createWireTransport(h.connection).subscribe("agent/a", null, { entry: (e) => a.push(e.kind), synced: () => a.push("synced"), failed: () => a.push("failed") });
    const old = subIds(h)[0] ?? "";
    send({ t: "synced", sub_id: old, seq: 0 });
    first.close();
    const b: string[] = [];
    createWireTransport(h.connection).subscribe("agent/b", null, { entry: (e) => b.push(`${e.kind} ${e.seq}`), synced: () => b.push("synced"), failed: () => b.push("failed") });
    const next = subIds(h)[1] ?? "";
    send({ t: "ev", sub_id: next, stream: "agent/b", seq: 1, ts_ms: 0, ev: { type: "user", message: {} } });
    send({ t: "synced", sub_id: next, seq: 1 });
    const before = h.connection.streams().get(next);
    send({ t: "ev", sub_id: old, stream: "agent/a", seq: 2, ts_ms: 0, ev: { type: "user", message: {} } });
    send({ t: "gap", sub_id: old, from: 3, to: 4 });
    send({ t: "synced", sub_id: old, seq: 4 });
    send({ t: "lagged", sub_id: old, skipped_to: 9 });
    send({ t: "subfail", sub_id: old, code: "unknown_stream", message: "gone" });
    expect(h.connection.state).toBe("Live");
    expect(h.connection.error).toBeNull();
    expect(h.connection.streams().get(next)).toEqual(before ?? fail());
    expect(b).toEqual(["known 1", "synced"]);
    expect(a).toEqual(["synced"]);
  });

  test("a frame for a sub id never opened on this connection is a protocol error", () => {
    const h = new Harness();
    live(h);
    h.step(step({ step: "send", frame: { t: "synced", sub_id: "t9", seq: 0 } }));
    expect(h.connection.state).toBe("Reconnecting");
    expect(h.connection.error).toContain("unknown subscription t9");
  });
});

function fail(): never {
  throw new Error("missing stream");
}

describe("a same-socket re-sub takes a fresh id (ADR-0038 §3, ADR-0036 §5a)", () => {
  const triggers = {
    "the progress deadline": (h: Harness) => h.clock.advanceTo(h.clock.now() + 30_000),
    lagged: (h: Harness) => {
      h.step(step({ step: "send", frame: { t: "synced", sub_id: "s1", seq: 0 } }));
      h.step(step({ step: "send", frame: { t: "lagged", sub_id: "s1", skipped_to: 9 } }));
    },
    cursor_ahead: (h: Harness) => h.step(step({ step: "send", frame: { t: "subfail", sub_id: "s1", code: "cursor_ahead", message: "ahead" } })),
  } as const;

  for (const [name, trigger] of Object.entries(triggers)) {
    test(`after ${name}, late synced, gap and subfail for the old id leave the new sub alone and the connection Live`, () => {
      const h = new Harness();
      live(h);
      const calls: string[] = [];
      const record = (kind: string) => () => void calls.push(kind);
      h.connection.subscribe("agent/a", { event: record("event"), gap: record("gap"), synced: record("synced"), snapshot: record("snapshot"), reset: record("reset"), refused: record("refused") });
      trigger(h);
      const fresh = lastSub(h);
      expect(fresh).not.toBe("s1");
      const sent = sentFrames(h);
      expect(sent.filter((frame) => (frame as { t: string }).t === "unsub")).toEqual(name === "the progress deadline" ? [{ t: "unsub", sub_id: "s1" }] : []);
      h.step(step({ step: "send", frame: { t: "synced", sub_id: fresh, seq: 0 } }));
      expect(h.connection.state).toBe("Live");
      const state = h.connection.streams().get(fresh);
      expect(state).toBeDefined();
      const before = calls.length;
      h.step(step({ step: "send", frame: { t: "synced", sub_id: "s1", seq: 7 } }));
      h.step(step({ step: "send", frame: { t: "gap", sub_id: "s1", from: 1, to: 6 } }));
      h.step(step({ step: "send", frame: { t: "subfail", sub_id: "s1", code: "scope", message: "late" } }));
      expect(h.connection.state).toBe("Live");
      expect(h.connection.error).toBeNull();
      expect(h.connection.streams().get(fresh)).toEqual(state);
      expect(h.connection.streams().has("s1")).toBe(false);
      expect(calls.length).toBe(before);
      expect(sentFrames(h).length).toBe(sent.length);
    });
  }
});

describe("sub_id_reused (ADR-0036 §5a)", () => {
  function refusedStream(): { h: Harness; seen: string[]; sub: { readonly id: string; readonly close: () => void } } {
    const h = new Harness();
    live(h);
    const seen: string[] = [];
    const sub = h.connection.subscribe("agent/a", { event: () => seen.push("event"), gap: () => seen.push("gap"), synced: () => seen.push("synced"), snapshot: () => seen.push("snapshot"), reset: () => seen.push("reset"), refused: (refusal) => seen.push(`refused ${refusal.code}: ${refusal.message}`) });
    h.step(step({ step: "send", frame: { t: "subfail", sub_id: sub.id, code: "sub_id_reused", message: "s1 was used" } }));
    return { h, seen, sub };
  }

  test("marks the stream Refused with a visible reason, and closes the id", () => {
    const { h, seen } = refusedStream();
    expect(h.connection.streams().get("s1")).toEqual({ stream: "agent/a", state: "Refused", cursor: null });
    expect(seen).toEqual(["refused sub_id_reused: The server refused sub id s1 as already used on this connection, so this stream is not retried. Server: s1 was used"]);
    const sent = sentFrames(h).length;
    h.step(step({ step: "send", frame: { t: "ev", sub_id: "s1", stream: "agent/a", seq: 1, ts_ms: 0, ev: { type: "user", message: {} } } }));
    h.step(step({ step: "send", frame: { t: "synced", sub_id: "s1", seq: 1 } }));
    expect(seen).toHaveLength(1);
    expect(h.connection.state).toBe("Live");
    expect(sentFrames(h).length).toBe(sent);
  });

  test("is never re-subscribed: not on the same socket, nor after a reconnect", () => {
    const { h } = refusedStream();
    // Past the 30 s progress deadline, inside the 45 s liveness window.
    h.clock.advanceTo(h.clock.now() + 31_000);
    expect(subIds(h)).toEqual(["s1"]);
    h.step(step({ step: "drop" }));
    h.step(step({ step: "expect_connect", within_ms: 500, not_before_ms: 250 }));
    h.step(step({ step: "open" }));
    h.step(step({ step: "send", frame: WELCOME }));
    expect(h.connection.state).toBe("Live");
    expect(subIds(h)).toEqual(["s1"]);
    expect(h.connection.streams().get("s1")?.state).toBe("Refused");
  });
});
