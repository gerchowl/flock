import type { Assistant } from "../src/gen/Assistant";

import type { KnownSessionEvent } from "../src/gen/KnownSessionEvent";
import type { MessageStart } from "../src/gen/MessageStart";
import type { MessageStop } from "../src/gen/MessageStop";
import type { ContentBlockStart } from "../src/gen/ContentBlockStart";
import type { ContentBlockDelta } from "../src/gen/ContentBlockDelta";
import type { ContentBlockStop } from "../src/gen/ContentBlockStop";
import type { MessageDelta } from "../src/gen/MessageDelta";
import type { ControlSuccess } from "../src/gen/ControlSuccess";
import type { ControlError } from "../src/gen/ControlError";

// @ts-expect-error The flattened extras cannot widen the type discriminator.
const wrongType: KnownSessionEvent = { type: "future", message: {} };
// @ts-expect-error The parser rejects an assistant carrying any subtype.
const wrongSubtype: KnownSessionEvent = { type: "assistant", subtype: "future", message: {} };

declare const start: MessageStart;
// @ts-expect-error The complete stream envelopes must also remain distinct.
const stop: MessageStop = start;
declare const success: ControlSuccess;
// @ts-expect-error The complete control envelopes must also remain distinct.
const error: ControlError = success;

function assertNever(value: never): never {
  throw new Error(`Unexpected discriminator: ${value}`);
}

// Nested switches narrow the payload; TypeScript does not correlate that
// narrowing back to the containing envelope.
function narrow(event: KnownSessionEvent): void {
  switch (event.type) {
    case "flk_permission_request":
      if (event.state === "open") {
        event.request.tool_name satisfies string;
      } else {
        event.state satisfies "closed";
        // @ts-expect-error Closed records never carry a tool request.
        event.request;
      }
      break;
    case "assistant":
      event satisfies Assistant;
      // Flattened extras must not widen the assistant's absent-only subtype (§6).
      event.subtype satisfies undefined;
      break;
    case "user": event satisfies import("../src/gen/User").User; break;
    case "command_lifecycle": event satisfies import("../src/gen/CommandLifecycle").CommandLifecycle; break;
    case "rate_limit_event": event satisfies import("../src/gen/RateLimit").RateLimit; break;
    case "control_request": event.request.subtype satisfies "can_use_tool"; break;
    case "control_cancel_request": event satisfies import("../src/gen/ControlCancel").ControlCancel; break;
    case "system":
      switch (event.subtype) {
        case "init": event satisfies import("../src/gen/Init").Init; break;
        case "status": event satisfies import("../src/gen/Status").Status; break;
        case "thinking_tokens": event satisfies import("../src/gen/ThinkingTokens").ThinkingTokens; break;
        case "compact_boundary": event satisfies import("../src/gen/CompactBoundary").CompactBoundary; break;
        case "permission_denied": event satisfies import("../src/gen/PermissionDenied").PermissionDenied; break;
        case "hook_started": event satisfies import("../src/gen/HookStarted").HookStarted; break;
        case "hook_response": event satisfies import("../src/gen/HookResponse").HookResponse; break;
        case "peer_message_hold": event satisfies import("../src/gen/PeerMessageHold").PeerMessageHold; break;
        default: assertNever(event);
      }
      break;
    case "result":
      switch (event.subtype) {
        case "success": event satisfies import("../src/gen/Success").Success; break;
        case "error_during_execution": event satisfies import("../src/gen/ExecutionError").ExecutionError; break;
        default: assertNever(event);
      }
      break;
    case "stream_event":
      switch (event.event.type) {
        case "message_start":
          event.event satisfies MessageStart["event"];
          // @ts-expect-error Start and stop payloads have different literal tags.
          event.event satisfies MessageStop["event"];
          break;
        case "message_stop":
          event.event satisfies MessageStop["event"];
          // @ts-expect-error Stop cannot be assigned to start.
          event.event satisfies MessageStart["event"];
          break;
        case "content_block_start":
          event.event satisfies ContentBlockStart["event"];
          // @ts-expect-error A different stream branch cannot acquire this tag.
          event.event.type satisfies "message_stop";
          break;
        case "content_block_delta":
          event.event satisfies ContentBlockDelta["event"];
          // @ts-expect-error A different stream branch cannot acquire this tag.
          event.event.type satisfies "message_stop";
          break;
        case "content_block_stop":
          event.event satisfies ContentBlockStop["event"];
          // @ts-expect-error A different stream branch cannot acquire this tag.
          event.event.type satisfies "message_stop";
          break;
        case "message_delta":
          event.event satisfies MessageDelta["event"];
          // @ts-expect-error A different stream branch cannot acquire this tag.
          event.event.type satisfies "message_stop";
          break;
        default: assertNever(event.event);
      }
      break;
    case "control_response":
      switch (event.response.subtype) {
        case "success":
          event.response satisfies ControlSuccess["response"];
          // @ts-expect-error A successful response cannot be an error response.
          event.response satisfies ControlError["response"];
          break;
        case "error":
          event.response satisfies ControlError["response"];
          // @ts-expect-error An error response cannot be a successful response.
          event.response satisfies ControlSuccess["response"];
          break;
        default: assertNever(event.response);
      }
      break;
    default: assertNever(event);
  }
}

// Every parser-absent subtype rejects strings and explicit null in generated TS.
type AbsentSubtype = Extract<KnownSessionEvent, {
  type: "assistant" | "user" | "command_lifecycle" | "rate_limit_event"
}>;
declare const absent: AbsentSubtype;
absent.subtype satisfies undefined;
declare const baseAssistant: import("../src/gen/Assistant").Assistant;
// @ts-expect-error The parser requires omission, including for this variant.
const subtypeAssistant: typeof baseAssistant = { ...baseAssistant, subtype: "future" };
// @ts-expect-error Explicit null is not absence.
const nullSubtypeAssistant: typeof baseAssistant = { ...baseAssistant, subtype: null };
declare const baseUser: import("../src/gen/User").User;
// @ts-expect-error The parser requires omission, including for this variant.
const subtypeUser: typeof baseUser = { ...baseUser, subtype: "future" };
// @ts-expect-error Explicit null is not absence.
const nullSubtypeUser: typeof baseUser = { ...baseUser, subtype: null };
declare const baseCommandLifecycle: import("../src/gen/CommandLifecycle").CommandLifecycle;
// @ts-expect-error The parser requires omission, including for this variant.
const subtypeCommandLifecycle: typeof baseCommandLifecycle = { ...baseCommandLifecycle, subtype: "future" };
// @ts-expect-error Explicit null is not absence.
const nullSubtypeCommandLifecycle: typeof baseCommandLifecycle = { ...baseCommandLifecycle, subtype: null };
declare const baseRateLimit: import("../src/gen/RateLimit").RateLimit;
// @ts-expect-error The parser requires omission, including for this variant.
const subtypeRateLimit: typeof baseRateLimit = { ...baseRateLimit, subtype: "future" };
// @ts-expect-error Explicit null is not absence.
const nullSubtypeRateLimit: typeof baseRateLimit = { ...baseRateLimit, subtype: null };
