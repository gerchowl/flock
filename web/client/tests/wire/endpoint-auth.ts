/** Complete real-endpoint auth trace; unimplemented scripts are explicitly accounted. */
import { strict as assert } from "node:assert";
import type { FixtureLine } from "../../src/gen/FixtureLine";
import { FakeNetwork, FakeSocket, Harness } from "./harness";
import { assertInventory, loadS7MinScripts, runTrace, type CheckedStep, type StepHandlers, type TraceScript, type TraceFact } from "./endpoint-runner";

const [url] = process.argv.slice(2);
assert(url);
class NativeSocket extends FakeSocket {
  readonly native: WebSocket;
  readonly ready: Promise<void>;
  readonly ended: Promise<number>;
  constructor(network: FakeNetwork) {
    super(network);
    const Socket = WebSocket as unknown as { new(url: string, options: { headers: Record<string, string> }): WebSocket };
    this.native = new Socket(url!, { headers: { Origin: new URL(url!.replace("ws:", "http:")).origin } });
    let open!: () => void, failOpen!: (error: Error) => void;
    let close!: (code: number) => void, failClose!: (error: Error) => void;
    this.ready = new Promise((resolve, reject) => { open = resolve; failOpen = reject; });
    this.ended = new Promise((resolve, reject) => { close = resolve; failClose = reject; });
    this.native.onopen = open;
    this.native.onclose = (event) => { close(event.code); failOpen(new Error("closed before upgrade")); };
    this.native.onerror = () => { const error = new Error("endpoint transport failed"); failOpen(error); failClose(error); };
    this.native.onmessage = () => assert.fail("unasserted server frame on refused auth");
  }
  override send(text: string): void {
    super.send(text);
    const frame = JSON.parse(text);
    // Resolve the scenario credential label to a deliberately unregistered bearer.
    if (frame.t === "hello") { assert.equal(frame.token, "fixture-token"); frame.token = "0".repeat(64); }
    this.native.send(JSON.stringify(frame));
  }
  override close(code: number, reason: string): void { super.close(code, reason); this.native.close(code, reason); }
}
async function tokenRefused(script: TraceScript): Promise<readonly TraceFact[]> {
  let socket: NativeSocket | undefined;
  class Network extends FakeNetwork {
    protected override makeSocket(): FakeSocket { socket = new NativeSocket(this); return socket; }
  }
  const harness = new Harness(() => 0, (clock) => new Network(clock));
  const checked = (source: CheckedStep["source"]): CheckedStep => ({ checked: true, source });
  const step = (source: CheckedStep["source"]) => async (line: Readonly<FixtureLine>) => { harness.step(line); return checked(source); };
  const handlers: StepHandlers = {
    given: step("setup"), client: step("client"), expect_connect: step("observation"), expect: step("observation"),
    observe: step("observation"), quiet: step("clock"),
    open: async (line) => { assert(socket); await socket.ready; harness.step(line); return checked("client"); },
    token_refused: async (line) => {
      assert(socket && line.step === "token_refused");
      assert.equal(await socket.ended, line.code, "actual endpoint refusal");
      harness.step(line); return checked("observation");
    },
  };
  try { return await runTrace(script, handlers); }
  finally { harness.conn?.disconnect(); }
}
const scripts = await loadS7MinScripts(`${import.meta.dir}/../../test/fixtures/wire`);
const providers = { "token-refused": tokenRefused };
assertInventory(scripts, Object.keys(providers));
const results: Record<string, readonly TraceFact[]> = {};
for (const script of scripts) {
  if (Object.hasOwn(providers, script.name)) results[script.name] = await providers[script.name as keyof typeof providers](script);
}
console.log(JSON.stringify(results));
