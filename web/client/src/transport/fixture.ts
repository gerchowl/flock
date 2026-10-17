/**
 * The fixture transport: replays a recorded session (G1, ADR-0038 §10).
 *
 * The recording is the Claude adapter's stored form (ADR-0036 §4), one
 * event per JSON line, as in `tests/fixtures/claude-session/events.jsonl`.
 * Line n is delivered with seq n. A line that is not JSON is delivered as an
 * unknown entry carrying the line itself, as the Rust parser keeps it.
 *
 * **Classification, and its limit.** On the wire the server classifies each
 * event and wraps unknowns (ADR-0036 §8); a recording has no such wrapper,
 * so this adapter classifies by the catalogue's discriminators: `type`, and
 * the nested `subtype`, `event.type`, `request.subtype` or
 * `response.subtype`. The discriminator tables below are typed against the
 * generated {@link KnownSessionEvent}, so `tsc` fails if they miss or invent
 * a value. Fields beyond the discriminators are not validated here: a line
 * with a known discriminator but incompatible fields, which the Rust parser
 * would call unknown, is delivered as known. The recorded fixtures are
 * proven field-by-field at compile time (`src/gen/known-session-fixtures.ts`),
 * and views fall back to unknown rendering if a known event's content is
 * not what they expect. G3's wire adapter uses the server's classification
 * instead and has no such limit.
 *
 * @module
 */
import type { JsonValue } from "../gen/serde_json/JsonValue";
import type { KnownSessionEvent } from "../gen/KnownSessionEvent";
import type { Cursor, SessionEntry, SessionObserver, SessionStream, Subscription, Transport } from "../transport";

/** The one stream a fixture transport serves. */
export const FIXTURE_STREAM: SessionStream = "agent/fixture";

type Discriminators<K extends string> = { readonly [key in K]: true };
type Of<T extends KnownSessionEvent["type"]> = Extract<KnownSessionEvent, { type: T }>;

function set<K extends string>(table: Discriminators<K>): ReadonlySet<string> {
  return new Set(Object.keys(table));
}

const TYPES = set<KnownSessionEvent["type"]>({
  assistant: true,
  user: true,
  command_lifecycle: true,
  system: true,
  rate_limit_event: true,
  control_request: true,
  control_response: true,
  stream_event: true,
  result: true,
});
const SYSTEM = set<Of<"system">["subtype"]>({
  init: true,
  status: true,
  thinking_tokens: true,
  compact_boundary: true,
  permission_denied: true,
  hook_started: true,
  hook_response: true,
  peer_message_hold: true,
});
const RESULT = set<Of<"result">["subtype"]>({ success: true, error_during_execution: true });
const STREAM = set<Of<"stream_event">["event"]["type"]>({
  message_start: true,
  content_block_start: true,
  content_block_delta: true,
  content_block_stop: true,
  message_delta: true,
  message_stop: true,
});
const CONTROL_REQUEST = set<Of<"control_request">["request"]["subtype"]>({ can_use_tool: true });
const CONTROL_RESPONSE = set<Of<"control_response">["response"]["subtype"]>({ success: true, error: true });

type JsonObject = { readonly [key: string]: JsonValue };

function isObject(value: JsonValue | undefined): value is JsonObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function nestedIn(value: JsonValue | undefined, key: string, allowed: ReadonlySet<string>): boolean {
  if (!isObject(value)) return false;
  const tag = value[key];
  return typeof tag === "string" && allowed.has(tag);
}

/** True if `value` carries a known discriminator path (see the module doc). */
function hasKnownDiscriminators(value: JsonValue): boolean {
  if (!isObject(value)) return false;
  const type = value["type"];
  if (typeof type !== "string" || !TYPES.has(type)) return false;
  switch (type) {
    case "system":
      return nestedIn(value, "subtype", SYSTEM);
    case "result":
      return nestedIn(value, "subtype", RESULT);
    case "stream_event":
      return nestedIn(value["event"], "type", STREAM);
    case "control_request":
      return nestedIn(value["request"], "subtype", CONTROL_REQUEST);
    case "control_response":
      return nestedIn(value["response"], "subtype", CONTROL_RESPONSE);
    default:
      // assistant and user reject any subtype; the other kinds have none.
      return !("subtype" in value) || (type !== "assistant" && type !== "user");
  }
}

/** Classifies one stored line as a {@link SessionEntry} with the given seq. */
export function classifyStoredLine(seq: number, line: string): SessionEntry {
  let value: JsonValue;
  try {
    value = JSON.parse(line) as JsonValue;
  } catch {
    return { seq, kind: "unknown", unknown: { raw: line, shape: null } };
  }
  if (hasKnownDiscriminators(value)) {
    // Discriminator-level check only; the module doc states the limit.
    return { seq, kind: "known", event: value as KnownSessionEvent };
  }
  return { seq, kind: "unknown", unknown: { raw: value, shape: null } };
}

/** Splits a JSON Lines recording; only the final line terminator is dropped. */
export function storedLines(jsonl: string): string[] {
  const lines = jsonl.split("\n");
  if (lines.at(-1) === "") lines.pop();
  return lines;
}

/**
 * Creates a transport that replays `jsonl` on {@link FIXTURE_STREAM}.
 * Subscribing to any other stream, or from a cursor past the recording's
 * end, fails the subscription rather than delivering nothing.
 */
export function createFixtureTransport(jsonl: string): Transport {
  const entries = storedLines(jsonl).map((line, index) => classifyStoredLine(index + 1, line));
  return {
    subscribe(stream: SessionStream, after: Cursor, observer: SessionObserver): Subscription {
      let open = true;
      queueMicrotask(() => {
        if (!open) return;
        if (stream !== FIXTURE_STREAM) {
          open = false;
          observer.failed({ message: `The fixture replay serves only ${FIXTURE_STREAM}, not ${stream}.` });
          return;
        }
        const from = after ?? 0;
        if (from > entries.length) {
          open = false;
          observer.failed({ message: `Cursor ${from} is past the recording's last seq ${entries.length}.` });
          return;
        }
        for (const entry of entries.slice(from)) {
          if (!open) return;
          observer.entry(entry);
        }
        if (open) observer.synced();
      });
      return {
        close: () => {
          open = false;
        },
      };
    },
  };
}
