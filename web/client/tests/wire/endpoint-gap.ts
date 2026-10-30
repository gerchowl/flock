/** Retained history is read by the real endpoint; every expected frame stays literal. */
import { strict as assert } from "node:assert";
import type { FixtureLine } from "../../src/gen/FixtureLine";
import { FakeNetwork, FakeSocket, Harness } from "./harness";
import { NativePort } from "./native-port";
import { loadS7MinScripts, runTrace, type CheckedStep, type StepHandlers } from "./endpoint-runner";
import { matchFrame } from "./timestamp-matcher";
const [url, buildVersion, control = ""] = process.argv.slice(2);
assert(url && ["", "gap", "stamp", "cursor"].includes(control));
let socket: NativePort;
class Network extends FakeNetwork {
  protected override makeSocket(): FakeSocket { socket = new NativePort(this, url!); return socket; }
}
const harness = new Harness(() => 0, (clock) => new Network(clock));
const checked = (source: CheckedStep["source"]): CheckedStep => ({ checked: true, source });
const step = (source: CheckedStep["source"]) => async (line: Readonly<FixtureLine>) => { harness.step(line); return checked(source); };
const script = (await loadS7MinScripts(`${import.meta.dir}/../../test/fixtures/wire`)).find((s) => s.name === "agent-gap"); assert(script);
const handlers: StepHandlers = {
  given: step("setup"), client: step("client"), expect_connect: step("observation"), expect: step("observation"), observe: step("observation"),
  open: async (line) => { await socket.ready; return step("client")(line); },
  send: async (line) => {
    assert(line.step === "send");
    const { frame } = await socket.next();
    await matchFrame(frame, line.frame, undefined, buildVersion);
    if (!(control === "cursor" && frame.t === "ev")) harness.acceptServerFrame(frame);
    return checked("server");
  },
  quiet: async (line) => { assert.equal(socket.frames.length, 0, "unasserted server frame"); return step("clock")(line); },
};
try { console.log(JSON.stringify(await runTrace(script, handlers))); }
finally { harness.conn?.disconnect(); }
