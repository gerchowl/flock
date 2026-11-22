/** Actual credential revocation, ordered native ack/close and retained outbox assertions. */
import { strict as assert } from "node:assert";
import type { FixtureLine } from "../../src/gen/FixtureLine";
import { FakeNetwork, FakeSocket, Harness } from "./harness";
import { NativePort } from "./native-port";
import { loadS7MinScripts, runTrace, type CheckedStep, type StepHandlers } from "./endpoint-runner";
import { matchFrame } from "./timestamp-matcher";
const [url, buildVersion, control = ""] = process.argv.slice(2);
assert(url && ["", "valid", "close", "outbox"].includes(control));
const replies = Bun.stdin.stream().getReader();
let socket: NativePort;
class Network extends FakeNetwork {
  protected override makeSocket(): FakeSocket { socket = new NativePort(this, url!); return socket; }
}
const harness = new Harness(() => 0, (clock) => new Network(clock));
const checked = (source: CheckedStep["source"]): CheckedStep => ({ checked: true, source });
const step = (source: CheckedStep["source"]) => async (line: Readonly<FixtureLine>) => { harness.step(line); return checked(source); };
const script = (await loadS7MinScripts(`${import.meta.dir}/../../test/fixtures/wire`)).find((s) => s.name === "token-revoked"); assert(script);
const handlers: StepHandlers = {
  given: step("setup"), expect_connect: step("observation"), expect: step("observation"), observe: step("observation"),
  client: async (line) => {
    assert(line.step === "client");
    if (line.action.kind === "command") {
      console.log(JSON.stringify({ op: "revoke" }));
      const reply = await replies.read(); assert(!reply.done, "revocation driver closed");
      assert.equal(new TextDecoder().decode(reply.value), "ok\n", "credential mutation completed");
    }
    return step("client")(line);
  },
  open: async (line) => { await socket.ready; return step("client")(line); },
  send: async (line) => {
    assert(line.step === "send");
    const { frame } = await socket.next(line.frame.t === "ack");
    await matchFrame(frame, line.frame, undefined, buildVersion);
    if (!(control === "outbox" && frame.t === "ack")) harness.acceptServerFrame(frame);
    return checked("server");
  },
  token_refused: async (line) => {
    assert(line.step === "token_refused");
    assert.equal(await socket.ended, control === "close" ? 4401 : line.code, "actual endpoint close");
    assert.deepEqual(socket.order, ["frame", "frame", "close"], "ack must precede close");
    return step("observation")(line);
  },
  quiet: async (line) => { assert.equal(socket.frames.length, 0, "unasserted server frame"); return step("clock")(line); },
};
try { console.log(JSON.stringify(await runTrace(script, handlers))); }
finally { harness.conn?.disconnect(); await replies.cancel(); }
