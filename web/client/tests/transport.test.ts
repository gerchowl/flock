import { describe, expect, test } from "bun:test";

import { permissionLifecycle } from "../src/gen/FixtureExamples";
import type { KnownSessionEvent } from "../src/gen/KnownSessionEvent";
import type { SessionEntry } from "../src/transport";
import { FIXTURE_STREAM, classifyStoredLine, createFixtureTransport } from "../src/transport/fixture";
import { readFixture, readVariants, replay } from "./fixture-replay";

/** The variant name `variants.txt` uses, by narrowing the generated union. */
function variant(event: KnownSessionEvent): string {
  switch (event.type) {
    case "flk_permission_request":
      return "PermissionRequest";
    case "assistant":
      return "Assistant";
    case "user":
      return "User";
    case "command_lifecycle":
      return "CommandLifecycle";
    case "rate_limit_event":
      return "RateLimit";
    case "control_request":
      return "CanUseTool";
    case "control_cancel_request":
      return "ControlCancel";
    case "control_response":
      return event.response.subtype === "success" ? "ControlSuccess" : "ControlError";
    case "result":
      return event.subtype === "success" ? "Success" : "ExecutionError";
    case "system":
      switch (event.subtype) {
        case "init":
          return "Init";
        case "status":
          return "Status";
        case "thinking_tokens":
          return "ThinkingTokens";
        case "compact_boundary":
          return "CompactBoundary";
        case "permission_denied":
          return "PermissionDenied";
        case "hook_started":
          return "HookStarted";
        case "hook_response":
          return "HookResponse";
        case "peer_message_hold":
          return "PeerMessageHold";
      }
    // A complete inner switch: no fallthrough into stream_event.
    case "stream_event":
      switch (event.event.type) {
        case "message_start":
          return "MessageStart";
        case "content_block_start":
          return "ContentBlockStart";
        case "content_block_delta":
          return "ContentBlockDelta";
        case "content_block_stop":
          return "ContentBlockStop";
        case "message_delta":
          return "MessageDelta";
        case "message_stop":
          return "MessageStop";
      }
  }
}

function describeEntry(entry: SessionEntry): string {
  return entry.kind === "known" ? variant(entry.event) : "Unknown";
}

describe("fixture transport", () => {
  test("replays the whole recording: every line, in variant order, with seqs 1..n", async () => {
    const [jsonl, variants] = await Promise.all([readFixture(), readVariants()]);
    const entries = await replay(createFixtureTransport(jsonl), FIXTURE_STREAM);
    expect(entries.length).toBe(variants.length);
    expect(entries.map(describeEntry)).toEqual(variants);
    expect(entries.map((entry) => entry.seq)).toEqual(variants.map((_, index) => index + 1));
  });

  test("delivers nothing synchronously, and nothing after close", async () => {
    const jsonl = await readFixture();
    const seen: number[] = [];
    const subscription = createFixtureTransport(jsonl).subscribe(FIXTURE_STREAM, null, {
      entry: (entry) => {
        seen.push(entry.seq);
        subscription.close();
      },
      synced: () => seen.push(-1),
      failed: () => seen.push(-2),
    });
    expect(seen).toEqual([]);
    await Promise.resolve();
    expect(seen).toEqual([1]);
  });

  test("resumes after a cursor and fails a cursor past the end", async () => {
    const [jsonl, variants] = await Promise.all([readFixture(), readVariants()]);
    const end = variants.length;
    const transport = createFixtureTransport(jsonl);
    const tail = await new Promise<number[]>((resolve, reject) => {
      const seqs: number[] = [];
      transport.subscribe(FIXTURE_STREAM, end - 2, {
        entry: (entry) => seqs.push(entry.seq),
        synced: () => resolve(seqs),
        failed: (failure) => reject(new Error(failure.message)),
      });
    });
    expect(tail).toEqual([end - 1, end]);
    await expect(
      new Promise((resolve, reject) =>
        transport.subscribe(FIXTURE_STREAM, end + 1, { entry: resolve, synced: () => resolve(null), failed: (f) => reject(new Error(f.message)) }),
      ),
    ).rejects.toThrow("past the recording's last seq");
  });

  test("refuses any stream but the fixture's", async () => {
    await expect(replay(createFixtureTransport(""), "agent/other")).rejects.toThrow("serves only agent/fixture");
  });

  test("an unknown or non-JSON line is an unknown entry, never an error", () => {
    expect(classifyStoredLine(1, '{"type":"future_event","x":1}')).toEqual({
      seq: 1,
      kind: "unknown",
      unknown: { raw: { type: "future_event", x: 1 }, shape: null },
    });
    expect(classifyStoredLine(2, '{"type":"system","subtype":"future"}').kind).toBe("unknown");
    expect(classifyStoredLine(3, '{"type":"stream_event","event":{"type":"ping"}}').kind).toBe("unknown");
    expect(classifyStoredLine(4, '{"type":"assistant","subtype":"x","message":{}}').kind).toBe("unknown");
    expect(classifyStoredLine(5, "not json")).toEqual({ seq: 5, kind: "unknown", unknown: { raw: "not json", shape: null } });
    expect(classifyStoredLine(6, "").kind).toBe("unknown");
  });
});

test("permission audit fixtures classify as known open and standalone closed records", () => {
  for (const payload of permissionLifecycle) {
    if (!("state" in payload)) continue;
    const entry = classifyStoredLine(1, JSON.stringify(payload));
    expect(entry.kind).toBe("known");
    if (entry.kind !== "known") throw new Error("permission record classified as unknown");
    expect(variant(entry.event)).toBe("PermissionRequest");
  }
  expect(classifyStoredLine(1, JSON.stringify({ type: "flk_permission_request", state: "decided" })).kind).toBe("unknown");
});
