import { expect, test } from "bun:test";
import type { FixtureServerFrame } from "../src/gen/FixtureServerFrame";
import type { ServerFrame } from "../src/gen/ServerFrame";
import { TimestampMatcher, type TimestampOracle } from "./wire/timestamp-matcher";
const actual: ServerFrame = { t: "ev", sub_id: "s", stream: "agent/fixture", seq: 1, ts_ms: 15, ev: { type: "assistant", message: {} } };
const expected = { ...actual, ts_ms: { $dynamic: "ts_ms", source: "persisted" } } as FixtureServerFrame;
const evidence = { record: { seq: 1, received_at_ms: 15, raw: JSON.stringify(actual.ev) }, before_action: 10, after_ack: 20 };
const oracle: TimestampOracle = async () => structuredClone(evidence);
test("a typed timestamp needs persistence AND its action/ack window", async () => {
  const matcher = new TimestampMatcher();
  await matcher.check(actual, expected, oracle);
  await expect(matcher.check(actual, expected)).rejects.toThrow("no independent oracle");
  for (const patch of [{ record: { ...evidence.record, received_at_ms: 16 } }, { before_action: 16 }, { after_ack: 14 }, { record: { ...evidence.record, seq: 2 } }, { record: { ...evidence.record, raw: "{}" } }]) {
    await expect(matcher.check(actual, expected, async () => ({ ...evidence, ...patch }))).rejects.toThrow();
  }
});
test("literal fields, marker locations and oracle kinds stay exact", async () => {
  const matcher = new TimestampMatcher();
  await expect(matcher.check(actual, { ...actual, ts_ms: 1 })).rejects.toThrow("frame.ts_ms");
  for (const frame of [{ ...expected, seq: { $dynamic: "ts_ms", source: "persisted" } }, { ...expected, ts_ms: { $dynamic: "ts_ms", source: "socket" } }]) {
    await expect(matcher.check(actual, frame as unknown as FixtureServerFrame, oracle)).rejects.toThrow();
  }
});
