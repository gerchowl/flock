/**
 * Decoding server text frames into the generated {@link ServerFrame}
 * (ADR-0036 §5, §6).
 *
 * The shape table below is typed against the generated union: a tag the
 * union lacks, or a field a variant does not have, fails `tsc`. Decoding
 * checks the tag and the JSON kind of every listed field. Additive fields
 * pass (ADR-0036 §6); a missing required field, a wrong kind or an unknown
 * `t` is a protocol error, which the connection closes on. Nested payloads
 * (`ev`, `error`, `result`) are left to their consumers.
 *
 * @module
 */
import type { ServerFrame } from "../gen/ServerFrame";

type Kind = "string" | "number" | "boolean" | "array" | "object" | "json";
type Field = { readonly kind: Kind; readonly optional?: true };
type Shapes = {
  readonly [T in ServerFrame["t"]]: { readonly [K in Exclude<keyof Extract<ServerFrame, { t: T }>, "t">]-?: Field };
};

const req = (kind: Kind): Field => ({ kind });
const opt = (kind: Kind): Field => ({ kind, optional: true });

const SHAPES: Shapes = {
  welcome: {
    protocol: req("number"),
    server_version: req("string"),
    capabilities: req("array"),
    principal: req("string"),
    server_epoch: req("string"),
    idempotency_window_ms: req("number"),
  },
  ping: { nonce: req("string") },
  ev: { sub_id: req("string"), stream: req("string"), seq: opt("number"), ts_ms: req("number"), ev: req("json") },
  synced: { sub_id: req("string"), seq: req("number") },
  subfail: { sub_id: req("string"), code: req("string"), message: req("string") },
  gap: { sub_id: req("string"), from: req("number"), to: req("number"), snapshot_to: opt("number") },
  lagged: { sub_id: req("string"), skipped_to: req("number") },
  ack: { cmd_id: req("string"), ok: req("boolean"), result: opt("json"), error: opt("json") },
};

function kindOf(value: unknown): Kind {
  if (Array.isArray(value)) return "array";
  if (value === null) return "json";
  const type = typeof value;
  return type === "string" || type === "number" || type === "boolean" ? type : "object";
}

/** The outcome of {@link decodeServerFrame}. */
export type Decoded = { readonly ok: true; readonly frame: ServerFrame } | { readonly ok: false; readonly reason: string };

/** Decodes one text frame; never throws. */
export function decodeServerFrame(data: unknown): Decoded {
  if (typeof data !== "string") return { ok: false, reason: "a binary frame" };
  let value: unknown;
  try {
    value = JSON.parse(data);
  } catch {
    return { ok: false, reason: "a frame that is not JSON" };
  }
  if (kindOf(value) !== "object") return { ok: false, reason: "a frame that is not an object" };
  const record = value as Record<string, unknown>;
  const tag = record["t"];
  if (typeof tag !== "string" || !Object.hasOwn(SHAPES, tag)) {
    return { ok: false, reason: `an unknown frame type ${JSON.stringify(tag)}` };
  }
  const shape: { readonly [field: string]: Field } = SHAPES[tag as ServerFrame["t"]];
  for (const [field, spec] of Object.entries(shape)) {
    const present = record[field] !== undefined;
    if (!present && spec.optional) continue;
    if (!present) return { ok: false, reason: `${tag} without ${field}` };
    if (spec.kind !== "json" && kindOf(record[field]) !== spec.kind) {
      return { ok: false, reason: `${tag}.${field} is not a ${spec.kind}` };
    }
  }
  return { ok: true, frame: record as ServerFrame };
}
