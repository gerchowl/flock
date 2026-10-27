import { expect, test } from "bun:test";
import type { FixtureServerFrame } from "../src/gen/FixtureServerFrame";
import type { ServerFrame } from "../src/gen/ServerFrame";
import { matchFrame } from "./wire/timestamp-matcher";
const actual: ServerFrame = { t: "ev", sub_id: "s1", stream: "agent/fixture", seq: 3, ts_ms: 5, ev: { type: "assistant", message: {} } };
const expected: FixtureServerFrame = { ...actual, ts_ms: { $dynamic: "ts_ms", source: "persisted" } };
const proof = () => ({ record: { seq: 3, received_at_ms: 5, raw: JSON.stringify(actual.ev) }, before_action: 4, after_ack: 6 });
test("wall time matches the durable record and the observed action window", async () => {
  await matchFrame(actual, expected, async () => proof());
  await expect(matchFrame(actual, expected)).rejects.toThrow("missing independent");
  for (const field of ["seq", "received_at_ms", "raw"] as const) {
    const value = proof();
    if (field === "raw") value.record.raw = "{}";
    else value.record[field]++;
    await expect(matchFrame(actual, expected, async () => value)).rejects.toThrow(`persisted ${field === "raw" ? "payload" : field === "seq" ? "sequence" : "timestamp"}`);
  }
  for (const value of [{ ...proof(), before_action: 6 }, { ...proof(), after_ack: 4 }]) {
    await expect(matchFrame(actual, expected, async () => value)).rejects.toThrow("observed");
  }
});
test("literal fields remain exact and a marker outside ts_ms is refused", async () => {
  await expect(matchFrame({ ...actual, sub_id: "wrong" }, expected, async () => proof())).rejects.toThrow("frame.sub_id");
  const misplaced = { ...expected, ev: { type: "assistant", message: { $dynamic: "ts_ms", source: "persisted" } } };
  await expect(matchFrame(actual, misplaced, async () => proof())).rejects.toThrow("frame.ts_ms");
});
