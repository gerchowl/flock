/** #1833: actual endpoint, public admission and browser wire, with an owned fake harness. */
import { strict as assert } from "node:assert";
import { assertSeedSucceeded } from "./seed-process";
import type { FleetDelta } from "../../src/gen/FleetDelta";
import type { JsonValue } from "../../src/gen/serde_json/JsonValue";
import type { ServerFrame } from "../../src/gen/ServerFrame";
import type { WireEvent } from "../../src/gen/WireEvent";
import { WireConnection } from "../../src/wire/connection";
import { browserClock, memoryStore, type SocketLike } from "../../src/wire/env";

const [url, token, readerToken, agent, binary, disabled, journal] = process.argv.slice(2);
assert(url && token && readerToken && agent && binary);
const events: WireEvent[] = [];
const rows = new Map<string, { waiting_on_human: string[]; activity?: string }>();
const frames: ServerFrame[] = [];
const sentFrames: unknown[] = [];
let changed = () => {};
let failure: Error | undefined;
let stage = "startup";
const sockets = (endpoint: string): SocketLike => {
    // Bun supplies no browser Origin automatically; send the real same-origin header.
    const NativeSocket = WebSocket as unknown as { new(url: string, options: { headers: Record<string, string> }): WebSocket };
    const socket = new NativeSocket(endpoint, { headers: { Origin: new URL(endpoint.replace("ws:", "http:")).origin } });
    const proxy: SocketLike = {
      onopen: null, onmessage: null, onclose: null, onerror: null,
      send(text) {
        const frame = JSON.parse(text) as { t: string };
        sentFrames.push(frame.t === "hello" ? { t: "hello" } : frame);
        socket.send(text);
      },
      close(code, reason) { socket.close(code, reason); },
    };
    socket.onopen = () => proxy.onopen?.();
    socket.onclose = (event) => proxy.onclose?.({ code: event.code, reason: event.reason });
    socket.onerror = () => proxy.onerror?.();
    socket.onmessage = (event) => {
      const frame = JSON.parse(String(event.data)) as ServerFrame;
      frames.push(frame);
      // Explicit causal controls withhold delivery, never manufacture server output.
      const withheld = frame.t === "ev" && ((disabled === "stream" && frame.stream.startsWith("agent/")) || (disabled === "fleet" && frame.stream === "fleet"));
      if (!withheld) proxy.onmessage?.({ data: event.data });
      changed();
    };
    return proxy;
  };
const options = { url, token, client: { name: "fixture", version: "1" }, clock: browserClock, store: memoryStore(), random: () => 0.5, sockets };
const connection = new WireConnection(options);
const reader = new WireConnection({ ...options, token: readerToken, store: memoryStore() });
const invalid = new WireConnection({ ...options, token: "0".repeat(64), store: memoryStore() });
reader.onChange(() => changed());
invalid.onChange(() => changed());
connection.onChange(() => {
  if (connection.state === "Reconnecting") failure = new Error(`unexpected reconnect during ${stage}: ${connection.error}`);
  changed();
});
function until(condition: () => boolean, label: string): Promise<void> {
  stage = label;
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(() => { changed = () => {}; reject(new Error(`server/client did not produce ${label}: ${JSON.stringify({ state: connection.state, error: connection.error, frames, sentFrames, streams: [...connection.streams()] })}`)); }, 5_000);
    const check = () => {
      if (failure) { clearTimeout(timeout); reject(failure); }
      else if (condition()) { clearTimeout(timeout); changed = () => {}; resolve(); }
    };
    changed = check;
    check();
  });
}
const handler = { event(_seq: number, event: WireEvent) { events.push(event); changed(); }, gap() {}, synced() { changed(); }, snapshot() {}, reset() {}, refused(value: unknown) { failure = new Error(JSON.stringify(value)); changed(); } };
connection.subscribe(`agent/${agent}`, handler);
const fleetHandler = { ...handler,
  event(_seq: number, ev: WireEvent) { apply(ev); changed(); },
  snapshot(values: readonly WireEvent[]) { rows.clear(); for (const value of values) apply(value); changed(); },
};
function apply(value: WireEvent): void {
  const event = value as FleetDelta;
  if (event.type === "fleet.upsert") rows.set(event.row.id, event.row);
  if (event.type === "fleet.remove") rows.delete(event.id);
}
const records = () => events as unknown as Record<string, unknown>[];
let commandNumber = 0;
async function command(verb: string, args: JsonValue, code?: string): Promise<Extract<ServerFrame, { t: "ack" }>> {
  const id = `dogfood-${++commandNumber}`;
  connection.command(verb, args, id);
  await until(() => frames.some((f) => f.t === "ack" && f.cmd_id === id), `${verb} ack`);
  const frame = frames.find((f) => f.t === "ack" && f.cmd_id === id) as Extract<ServerFrame, { t: "ack" }>;
  assert.equal(frame.ok, code === undefined, JSON.stringify(frame));
  if (code) assert.equal(frame.error?.code, code, JSON.stringify(frame));
  return frame;
}
try {
  connection.connect();
  await until(() => connection.state === "Live" && [...connection.streams()].every(([, s]) => s.state === "Current"), "agent bootstrap");
  connection.subscribe("fleet", fleetHandler);
  await until(() => connection.state === "Live" && [...connection.streams()].every(([, s]) => s.state === "Current") && rows.has(agent), "agent and fleet bootstrap");
  assert.equal(rows.get(agent)?.activity, undefined, "no activity producer until #2243");
  invalid.connect();
  await until(() => invalid.state === "NeedsToken", "invalid-token refusal");
  reader.connect();
  await until(() => reader.state === "Live", "read-only principal");
  reader.command("agent.send", { target: agent, text: "must not arrive", submit: true }, "scope-denied");
  await until(() => frames.some((f) => f.t === "ack" && f.cmd_id === "scope-denied"), "scope refusal");
  assert(frames.some((f) => f.t === "ack" && f.cmd_id === "scope-denied" && !f.ok && f.error?.code === "scope"));
  if (disabled === "without-operate") {
    // #2585 tracks whether send alone should suffice; pin the current S6 policy.
    assert(journal, "scope proof receives its owned queue journal");
    const before = await Bun.file(journal).bytes();
    await command("agent.send", { target: agent, text: "must not arrive", submit: true }, "not_authorized");
    assert.deepEqual(await Bun.file(journal).bytes(), before, "send without operate journaled nothing");
    // Seed the permission only after the byte-for-byte refusal check, so its
    // legitimate queue writes cannot race the negative assertion.
    const seed = Bun.spawn([binary, "--json", "agent", "send", agent, "fixture message"], { stdout: "pipe", stderr: "pipe" });
    const seeded = JSON.parse(await assertSeedSucceeded(seed)) as { result?: unknown };
    assert(seeded.result, "operator seed acknowledges durable queue admission");
    await until(() => records().some((e) => e.type === "flk_permission_request" && e.state === "open"), "operator-created permission request");
    const prompt = records().find((e) => e.type === "flk_permission_request" && e.state === "open")!;
    await until(() => rows.get(agent)?.waiting_on_human.includes(String(prompt.request_id)) === true, "operator-created permission fleet handle");
    await command("agent.decide", { agent, request_id: String(prompt.request_id), request_seq: Number(prompt.request_seq), decision: "deny" }, "not_authorized");
    assert(rows.get(agent)?.waiting_on_human.includes(String(prompt.request_id)), "refused decision kept its real permission handle");
    console.log(JSON.stringify({ status: "passed", without_operate: { send: "not_authorized", journal_unchanged: true, decide: "not_authorized" } }));
  } else {
    const sent = await command("agent.send", { target: agent, text: "fixture message", submit: true });
    assert(sent.result, "ack identifies durable queue admission");
    await until(() => records().some((e) => e.type === "flk_permission_request" && e.state === "open") && rows.get(agent)?.waiting_on_human.includes("typed-request") === true, "real permission stream and fleet handle");
    assert(records().some((e) => e.type === "command_lifecycle" && e.state === "started"), "harness lifecycle reached the agent stream");
    const prompt = records().find((e) => e.type === "flk_permission_request" && e.state === "open")!;
    assert.equal(prompt.request_id, "typed-request");
    const args = { agent, request_id: String(prompt.request_id), request_seq: Number(prompt.request_seq), decision: "deny", message: "operator reason" };
    await command("agent.decide", { ...args, request_seq: Number(prompt.request_seq) + 1 }, "permission_request_unknown");
    await command("agent.decide", { ...args, agent: "missing-fixture-agent" }, "agent_not_found");
    await command("agent.send", { target: agent, text: "must not arrive", submit: true, raw: true }, "invalid_params");
    await command("agent.decide", args);
    await until(() => records().some((e) => e.type === "flk_permission_request" && e.state === "closed") && records().some((e) => e.type === "result" && e.result === "wire-complete") && rows.get(agent)?.waiting_on_human.length === 0, "decision close, harness result and fleet clear");
    await command("agent.decide", args, "permission_request_closed");
    // Revocation is performed by the isolated local operator; the browser never owns that authority.
    if (disabled !== "revocation") {
    const revoke = Bun.spawn([binary, "--json", "client", "token", "rm", "dogfood-wire"], { stdout: "pipe", stderr: "pipe" });
    assert.equal(await revoke.exited, 0, await new Response(revoke.stderr).text());
    }
    connection.command("agent.send", { target: agent, text: "must not arrive", submit: true }, "revoked");
    await until(() => connection.state === "NeedsToken", "revocation close");
    assert(frames.some((f) => f.t === "ack" && f.cmd_id === "revoked" && !f.ok && f.error?.code === "auth_revoked"));
    console.log(JSON.stringify({ status: "passed", start_stream_send_permission_decide_result_fleet: true, exact_handle: true, revoked: true }));
  }
} finally { connection.disconnect(); reader.disconnect(); invalid.disconnect(); }
