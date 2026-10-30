/** Complete permission trace: literal frames, real persistence and observed wall windows. */
import { strict as assert } from "node:assert";
import { NativePort } from "./native-port";
import { matchFrame } from "./timestamp-matcher";
import type { FixtureLine } from "../../src/gen/FixtureLine";
import { FakeNetwork, FakeSocket, Harness } from "./harness";
import { loadS7MinScripts, runTrace, type CheckedStep, type StepHandlers } from "./endpoint-runner";
const [url, history, start, ready, control, buildVersion] = process.argv.slice(2);
assert(url && history && start && ready);
assert(["", "persisted", "window", "delivery_window", "outbox", "cursor"].includes(control ?? ""));
let before = Number(start), after = Number(ready);
let socket: NativePort;
class Network extends FakeNetwork {
  protected override makeSocket(): FakeSocket { socket = new NativePort(this, url!); return socket; }
}
const harness = new Harness(() => 0, (clock) => new Network(clock));
const checked = (source: CheckedStep["source"]): CheckedStep => ({ checked: true, source });
const step = (source: CheckedStep["source"]) => async (line: Readonly<FixtureLine>) => { harness.step(line); return checked(source); };
const script = (await loadS7MinScripts(`${import.meta.dir}/../../test/fixtures/wire`)).find((s) => s.name === "permission-decision"); assert(script);
const handlers: StepHandlers = {
  given: step("setup"), expect_connect: step("observation"), expect: step("observation"), observe: step("observation"),
  client: async (line) => { assert(line.step === "client"); if (line.action.kind === "command") before = Date.now(); return step("client")(line); },
  open: async (line) => { await socket.ready; return step("client")(line); },
  send: async (line) => {
    assert(line.step === "send"); const { frame: actual, delivered_at } = await socket.next();
    if (actual.t === "ack") after = delivered_at;
    await matchFrame(actual, line.frame, async (event) => {
      const records = (await Bun.file(history!).text()).trim().split("\n").map((raw) => JSON.parse(raw));
      const record = records.find((r) => r.seq === event.seq); assert(record, "missing persisted record");
      return { record: { ...record, received_at_ms: record.received_at_ms + (control === "persisted" ? 1 : 0) }, before_action: control === "window" || control === "delivery_window" ? before - 2 : before, after_ack: control === "window" ? before - 1 : after, record_delivery: control === "delivery_window" ? before - 1 : delivered_at };
    }, buildVersion);
    if (!(control === "outbox" && actual.t === "ack") && !(control === "cursor" && actual.t === "ev" && actual.seq === 5)) harness.acceptServerFrame(actual);
    return checked("server");
  },
  quiet: async (line) => { assert.equal(socket.frames.length, 0, "unasserted server frame"); return step("clock")(line); },
};
try { console.log(JSON.stringify(await runTrace(script, handlers))); }
finally { harness.conn?.disconnect(); }
