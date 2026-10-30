/** Real handshake and protocol close; impossible server output is a labelled client fault. */
import { strict as assert } from "node:assert";
import type { FixtureLine } from "../../src/gen/FixtureLine";
import type { ServerFrame } from "../../src/gen/ServerFrame";
import { FakeNetwork, FakeSocket, Harness } from "./harness";
import { loadS7MinScripts, runTrace, type CheckedStep, type StepHandlers } from "./endpoint-runner";
import { matchFrame } from "./timestamp-matcher";

const [url, control = "", buildVersion] = process.argv.slice(2);
assert(url && ["", "welcome", "fault", "close"].includes(control));
let socket: Port;
class Port extends FakeSocket {
  readonly native: WebSocket;
  readonly ready: Promise<void>;
  readonly ended: Promise<number>;
  readonly frames: ServerFrame[] = [];
  wake: (() => void) | undefined;
  error: Error | undefined;
  constructor(net: FakeNetwork) {
    super(net);
    const Socket = WebSocket as unknown as { new(url: string, options: { headers: Record<string, string> }): WebSocket };
    this.native = new Socket(url!, { headers: { Origin: new URL(url!.replace("ws:", "http:")).origin } });
    let resolveOpen!: () => void, rejectOpen!: (error: Error) => void;
    let resolveClose!: (code: number) => void, rejectClose!: (error: Error) => void;
    this.ready = new Promise((resolve, reject) => { resolveOpen = resolve; rejectOpen = reject; });
    this.ended = new Promise((resolve, reject) => { resolveClose = resolve; rejectClose = reject; });
    // Observe cancellation of the final unopened reconnect; original awaits still reject.
    void this.ready.catch((error: Error) => { this.error = error; this.wake?.(); });
    void this.ended.catch((error: Error) => { this.error = error; this.wake?.(); });
    this.native.onopen = resolveOpen;
    this.native.onmessage = (event) => { this.frames.push(JSON.parse(String(event.data))); this.wake?.(); };
    this.native.onerror = () => {
      this.error = new Error("endpoint transport failed");
      rejectOpen(this.error); rejectClose(this.error); this.wake?.();
    };
    this.native.onclose = (event) => {
      this.error = new Error("endpoint closed");
      rejectOpen(this.error); resolveClose(event.code); this.wake?.();
    };
  }
  override send(text: string): void {
    super.send(text);
    const frame = JSON.parse(text);
    if (frame.t === "hello") { assert.equal(frame.token, "fixture-token"); frame.token = "a".repeat(64); }
    this.native.send(JSON.stringify(frame));
  }
  override close(code: number, reason: string): void { super.close(code, reason); this.native.close(code, reason); }
  async next(): Promise<ServerFrame> {
    if (!this.frames.length && !this.error) await new Promise<void>((resolve) => { this.wake = resolve; });
    this.wake = undefined;
    if (this.error) throw this.error;
    const frame = this.frames.shift(); assert(frame, "missing server frame"); return frame;
  }
}
class Network extends FakeNetwork {
  protected override makeSocket(): FakeSocket { socket = new Port(this); return socket; }
}
const harness = new Harness(() => 0, (clock) => new Network(clock));
const checked = (source: CheckedStep["source"]): CheckedStep => ({ checked: true, source });
const step = (source: CheckedStep["source"]) => async (line: Readonly<FixtureLine>) => { harness.step(line); return checked(source); };
const script = (await loadS7MinScripts(`${import.meta.dir}/../../test/fixtures/wire`)).find((s) => s.name === "unknown-subscription"); assert(script);
const handlers: StepHandlers = {
  given: step("setup"), client: step("client"), expect: step("observation"), observe: step("observation"),
  expect_connect: step("observation"),
  open: async (line) => { await socket.ready; return step("client")(line); },
  send: async (line) => {
    assert(line.step === "send");
    if (line.frame.t === "welcome") {
      const frame = await socket.next();
      await matchFrame(control === "welcome" ? { ...frame, protocol: 99 } as ServerFrame : frame, line.frame, undefined, buildVersion);
      harness.acceptServerFrame(frame); return checked("server");
    }
    assert(line.frame.t === "synced" && line.frame.sub_id === "never-opened");
    if (control !== "fault") harness.step(line);
    return checked("client_fault");
  },
  expect_close: async (line) => {
    assert(line.step === "expect_close");
    harness.step(line);
    assert.equal(await socket.ended, control === "close" ? 1000 : line.code, "actual endpoint close");
    assert.equal(socket.frames.length, 0, "unasserted server frame");
    return checked("observation");
  },
  quiet: async (line) => { assert.equal(socket.frames.length, 0, "unasserted server frame"); return step("clock")(line); },
};
try { console.log(JSON.stringify(await runTrace(script, handlers))); }
finally { harness.conn?.disconnect(); }
