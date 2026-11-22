/**
 * Unknown events: ADR-0036 §8's rendering model.
 *
 * An event the client cannot classify renders as a collapsed block whose
 * title is the shape key and whose body is the raw JSON. Building that model
 * never throws, whatever the input: it is the fallback for everything else.
 *
 * @module
 */
import type { JsonValue } from "./gen/serde_json/JsonValue";

/**
 * What an unknown block renders. G1's fixture transport passes a stored
 * line as `raw` with no shape; G3 passes the wire's `UnknownEvent.raw` and
 * `UnknownEvent.shape` unchanged.
 */
export type UnknownInput = {
  /** The event as received: parsed JSON, or the line itself if not JSON. */
  readonly raw: JsonValue;
  /** The server's shape key (ADR-0036 §8), when the server sent one. */
  readonly shape: string | null;
};

/** The rendered form of an {@link UnknownInput}. */
export type UnknownView = {
  readonly title: string;
  readonly body: string;
};

/** Builds the collapsed block's title and body. Never throws. */
export function describeUnknown(input: UnknownInput): UnknownView {
  const { raw, shape } = input;
  let hint = "";
  if (typeof raw === "object" && raw !== null && !Array.isArray(raw) && typeof raw["type"] === "string") {
    hint = `: ${raw["type"]}`;
  }
  const title = shape ?? `Unrecognised event${hint}`;
  // JsonValue holds no cycles or bigints, so stringify cannot throw; a value
  // outside JSON (undefined from a faulty caller) still renders as text.
  const body = typeof raw === "string" ? raw : (JSON.stringify(raw, null, 2) ?? String(raw));
  return { title, body };
}
