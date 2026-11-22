/** Fresh replay through the real endpoint, with independently read durable records. */
import { strict as assert } from "node:assert";
import type { FixtureLine } from "../../src/gen/FixtureLine";
import { FakeNetwork, FakeSocket, Harness } from "./harness";
import { NativePort } from "./native-port";
import { loadS7MinScripts, runTrace, type CheckedStep, type StepHandlers } from "./endpoint-runner";
import { matchFrame } from "./timestamp-matcher";
const [url, history, starts, buildVersion, control = ""] = process.argv.slice(2);
assert(url && history && starts && ["", "persisted", "window", "fresh", "synced"].includes(control));
const before: Record<string, number> = JSON.parse(starts);
let socket: NativePort;
class Network extends FakeNetwork {
  protected override makeSocket(): FakeSocket { socket = new NativePort(this, url!); return socket; }
}
const harness = new Harness(() => 0, (clock) => new Network(clock));
const checked = (source: CheckedStep["source"]): CheckedStep => ({ checked: true, source });
const step = (source: CheckedStep["source"]) => async (line: Readonly<FixtureLine>) => { harness.step(line); return checked(source); };
const script = (await loadS7MinScripts(`${import.meta.dir}/../../test/fixtures/wire`)).find((s) => s.name === "clean"); assert(script);
const handlers: StepHandlers = {
  given: async (line) => {
    assert(line.step === "given");
    harness.step(control === "fresh" ? { ...line, cursors: { "agent/agent_worker_246705": 0 } } : line);
    return checked("setup");
  },
  client: step("client"), expect_connect: step("observation"), expect: step("observation"), observe: step("observation"),
  open: async (line) => { await socket.ready; return step("client")(line); },
  send: async (line) => {
    assert(line.step === "send");
    const { frame, delivered_at } = await socket.next();
    await matchFrame(frame, line.frame, async (event) => {
      const records = (await Bun.file(history!).text()).trim().split("\n").map((raw) => JSON.parse(raw));
      const record = records.find((r) => r.seq === event.seq); assert(record, "missing persisted record");
      return { record: { ...record, received_at_ms: record.received_at_ms + (control === "persisted" ? 1 : 0) }, before_action: before[String(event.seq)]!, record_delivery: control === "window" ? before[String(event.seq)]! - 1 : delivered_at };
    }, buildVersion);
    if (!(control === "synced" && frame.t === "synced")) harness.acceptServerFrame(frame);
    return checked("server");
  },
  quiet: async (line) => { assert.equal(socket.frames.length, 0, "unasserted server frame"); return step("clock")(line); },
};
try { console.log(JSON.stringify(await runTrace(script, handlers))); }
finally { harness.conn?.disconnect(); }
