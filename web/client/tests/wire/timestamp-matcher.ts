/** Fixture-only runtime timestamps: persisted provenance AND the action's wall window. */
import { strict as assert } from "node:assert";
import type { FixtureServerFrame } from "../../src/gen/FixtureServerFrame";
import type { ServerFrame } from "../../src/gen/ServerFrame";

type Event = Extract<ServerFrame, { t: "ev" }>;
export type TimestampOracle = (event: Event) => Promise<{
  record: { seq: number; received_at_ms: number; raw: string };
  before_action: number; after_ack: number;
}>;

/** Literal fields stay exact; a dynamic marker without both independent oracles fails. */
export class TimestampMatcher {
  async check(actual: ServerFrame, expected: FixtureServerFrame, oracle?: TimestampOracle): Promise<void> {
    const visit = async (got: unknown, want: unknown, path: string): Promise<void> => {
      if (want !== null && typeof want === "object" && !Array.isArray(want)) {
        if (Object.hasOwn(want, "$dynamic")) {
          assert.equal(path, "frame.ts_ms", "dynamic marker outside a wall-clock field");
          assert.deepEqual(want, { $dynamic: "ts_ms", source: "persisted" });
          assert(actual.t === "ev" && oracle, "dynamic timestamp has no independent oracle");
          const { record, before_action, after_ack } = await oracle(actual);
          assert(Number.isSafeInteger(before_action) && Number.isSafeInteger(after_ack) && before_action <= after_ack, "invalid observed wall window");
          assert.equal(record.seq, actual.seq, "persisted record sequence");
          assert.deepEqual(JSON.parse(record.raw), actual.ev, "persisted record payload");
          assert.equal(got, record.received_at_ms, "persisted timestamp");
          assert(Number.isSafeInteger(got) && (got as number) >= before_action && (got as number) <= after_ack, "timestamp outside action/ack window");
          // Receipt times are wall-clock samples: ADR-0036 promises sequence order,
          // not monotonic wall time. Do not impose a new clock monotonicity rule.
          return;
        }
        assert(got !== null && typeof got === "object" && !Array.isArray(got));
        for (const [key, value] of Object.entries(want)) {
          assert(Object.hasOwn(got, key), `missing ${path}.${key}`);
          await visit((got as Record<string, unknown>)[key], value, `${path}.${key}`);
        }
      } else assert.deepEqual(got, want, path);
    };
    if (actual.t === "welcome" && expected.t === "welcome") {
      for (const key of ["protocol", "capabilities", "idempotency_window_ms"] as const) assert.deepEqual(actual[key], expected[key]);
    } else await visit(actual, expected, "frame");
  }
}
