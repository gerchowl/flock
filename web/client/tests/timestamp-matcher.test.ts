import { expect, test } from "bun:test";
import type { FixtureServerFrame } from "../src/gen/FixtureServerFrame";
import type { ServerFrame } from "../src/gen/ServerFrame";
import { matchFrame } from "./wire/timestamp-matcher";
const actual: ServerFrame = { t: "ev", sub_id: "s1", stream: "agent/fixture", seq: 3, ts_ms: 5, ev: { type: "assistant", message: {} } };
const expected: FixtureServerFrame = { ...actual, ts_ms: { $dynamic: "ts_ms", source: "persisted", bound: "after_ack" } };
const proof = () => ({ record: { seq: 3, received_at_ms: 5, raw: JSON.stringify(actual.ev) }, before_action: 4, after_ack: 6, record_delivery: 6 });
test("wall time matches the durable record and the observed action window", async () => {
  await expect(matchFrame(actual, { ...expected, ts_ms: { $dynamic: "ts_ms", source: "persisted" } } as unknown as FixtureServerFrame, async () => proof())).rejects.toThrow("bound");
  await matchFrame(actual, expected, async () => proof());
  const delivery = { ...expected, ts_ms: { $dynamic: "ts_ms", source: "persisted", bound: "record_delivery" } } satisfies FixtureServerFrame;
  await matchFrame(actual, delivery, async () => ({ ...proof(), after_ack: 4 }));
  await expect(matchFrame(actual, expected, async () => ({ ...proof(), after_ack: 4 }))).rejects.toThrow("observed");
  await matchFrame(actual, delivery, async () => ({ ...proof(), before_action: 5, record_delivery: 5 }));
  await expect(matchFrame(actual, delivery, async () => ({ ...proof(), record_delivery: 4 }))).rejects.toThrow("observed");
  await expect(matchFrame(actual, delivery, async () => ({ record: proof().record, before_action: 4, after_ack: 6 }))).rejects.toThrow("observed");
  await expect(matchFrame(actual, expected)).rejects.toThrow("missing independent");
  for (const field of ["seq", "received_at_ms", "raw"] as const) {
    const value = proof();
    if (field === "raw") value.record.raw = "{}";
    else value.record[field]++;
    await expect(matchFrame(actual, expected, async () => value)).rejects.toThrow(`persisted ${field === "raw" ? "payload" : field === "seq" ? "sequence" : "timestamp"}`);
  }
  for (const value of [{ ...proof(), before_action: 6 }, { record: proof().record, before_action: 4, record_delivery: 6 }]) {
    await expect(matchFrame(actual, expected, async () => value)).rejects.toThrow("observed");
  }
});
test("literal fields remain exact and a marker outside ts_ms is refused", async () => {
  await expect(matchFrame({ ...actual, sub_id: "wrong" }, expected, async () => proof())).rejects.toThrow("frame.sub_id");
  const misplaced = { ...expected, ev: { type: "assistant", message: { $dynamic: "ts_ms", source: "persisted" } } };
  await expect(matchFrame(actual, misplaced, async () => proof())).rejects.toThrow("frame.ts_ms");
});

test("literal objects reject extra fields at every nesting level", async () => {
  for (const frame of [
    { ...actual, extra: true },
    { ...actual, ev: { ...actual.ev as object, extra: true } },
    { ...actual, ev: { type: "assistant", message: { extra: true } } },
  ]) await expect(matchFrame(frame, expected, async () => proof())).rejects.toThrow();
  const ack = { t: "ack", cmd_id: "c1", ok: true, result: {} } as const;
  await expect(matchFrame({ ...ack, result: { extra: true } }, ack)).rejects.toThrow();
  await expect(matchFrame({ ...ack, extra: true } as ServerFrame, ack)).rejects.toThrow();
  await expect(matchFrame({ t: "synced", sub_id: "s1", seq: 1, extra: true } as ServerFrame, { t: "synced", sub_id: "s1", seq: 1 })).rejects.toThrow();
});

test("welcome version matches the supplied build, not a pinned release", async () => {
  const frame = { t: "welcome", protocol: 1, server_version: "9.8.7-test", capabilities: ["read"], principal: "0".repeat(32), server_epoch: "fixture", idempotency_window_ms: 1 } as const;
  const expected = { ...frame, server_version: { $dynamic: "server_version", source: "build" } } as unknown as FixtureServerFrame;
  const actual = { ...frame, capabilities: ["read"] } satisfies ServerFrame;
  await matchFrame(actual, expected, undefined, "9.8.7-test");
  await expect(matchFrame(actual, expected, undefined, "9.8.8-test")).rejects.toThrow("build version");
  await expect(matchFrame(actual, expected)).rejects.toThrow("build version");
});
