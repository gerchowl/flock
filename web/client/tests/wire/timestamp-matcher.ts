import { strict as assert } from "node:assert";
import type { FixtureServerFrame } from "../../src/gen/FixtureServerFrame";
import type { ServerFrame } from "../../src/gen/ServerFrame";
type Event = Extract<ServerFrame, { t: "ev" }>;
type Oracle = (event: Event) => Promise<{
  record: { seq: number; received_at_ms: number; raw: string };
  before_action: number; after_ack?: number; record_delivery?: number;
}>;
/** Match literals exactly; dynamic wall times need persisted equality and a declared zero-slack bound. */
export async function matchFrame(actual: ServerFrame, expected: FixtureServerFrame, oracle?: Oracle, buildVersion?: string): Promise<void> {
  const visit = async (got: unknown, want: unknown, path: string): Promise<void> => {
    if (want !== null && typeof want === "object" && !Array.isArray(want)) {
      if (Object.hasOwn(want, "$dynamic")) {
        if ("$dynamic" in want && want.$dynamic === "server_version") {
          assert.equal(path, "frame.server_version");
          assert.deepEqual(want, { $dynamic: "server_version", source: "build" });
          assert(actual.t === "welcome" && typeof buildVersion === "string" && buildVersion.length > 0, "missing build version oracle");
          assert.equal(got, buildVersion, "build version");
          return;
        }
        assert.equal(path, "frame.ts_ms");
        assert("bound" in want && (want.bound === "after_ack" || want.bound === "record_delivery"), "missing or unknown timestamp bound");
        assert.deepEqual(want, { $dynamic: "ts_ms", source: "persisted", bound: want.bound });
        assert(actual.t === "ev" && oracle, "missing independent timestamp oracle");
        const { record, before_action, after_ack, record_delivery } = await oracle(actual);
        const upper = want.bound === "after_ack" ? after_ack : record_delivery;
        assert.equal(record.seq, actual.seq, "persisted sequence");
        assert.deepEqual(JSON.parse(record.raw), actual.ev, "persisted payload");
        assert.equal(got, record.received_at_ms, "persisted timestamp");
        assert(Number.isSafeInteger(before_action) && typeof upper === "number" && Number.isSafeInteger(upper) && before_action <= upper, "invalid observed window");
        assert(Number.isSafeInteger(got) && (got as number) >= before_action && (got as number) <= upper, `observed ${want.bound} window: got=${String(got)}, before=${before_action}, upper=${upper}`);
        return;
      }
      assert(got !== null && typeof got === "object" && !Array.isArray(got));
      assert.deepEqual(Object.keys(got).sort(), Object.keys(want).sort(), `exact keys at ${path}`);
      for (const [key, value] of Object.entries(want)) {
        assert(Object.hasOwn(got, key), `missing ${path}.${key}`);
        await visit((got as Record<string, unknown>)[key], value, `${path}.${key}`);
      }
    } else assert.deepEqual(got, want, path);
  };
  // ADR-0036 promises sequence order, not monotonic wall-clock receipt times.
  await visit(actual, expected, "frame");
}
