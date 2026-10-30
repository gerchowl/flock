/** Complete permission trace: literal frames, real persistence and observed wall windows. */
import { strict as assert } from "node:assert";
import type { ServerFrame } from "../../src/gen/ServerFrame";
import { matchFrame } from "./timestamp-matcher";
import type { FixtureLine } from "../../src/gen/FixtureLine";
import { FakeNetwork, FakeSocket, Harness } from "./harness";
import { loadS7MinScripts, runTrace, type CheckedStep, type StepHandlers } from "./endpoint-runner";
const [url, history, start, ready, control, buildVersion] = process.argv.slice(2);
assert(url && history && start && ready);
assert(["", "persisted", "window", "delivery_window", "outbox", "cursor"].includes(control ?? ""));
let before = Number(start), after = Number(ready);
let socket: Port;
class Port extends FakeSocket {
  readonly native: WebSocket;
  readonly ready: Promise<void>;
  readonly frames: { frame: ServerFrame; delivered_at: number }[] = [];
  wake: (() => void) | undefined;
  error: Error | undefined;
  constructor(net: FakeNetwork) {
    super(net);
    const Socket = WebSocket as unknown as { new(url: string, options: { headers: Record<string, string> }): WebSocket };
    this.native = new Socket(url!, { headers: { Origin: new URL(url!.replace("ws:", "http:")).origin } });
    this.ready = new Promise((resolve, reject) => {
      this.native.onopen = () => resolve();
      this.native.onerror = () => { this.error = new Error("transport failed"); reject(this.error); this.wake?.(); };
      this.native.onclose = () => { this.error = new Error("endpoint closed before trace finished"); reject(this.error); this.wake?.(); };
    });
    this.native.onmessage = (event) => { this.frames.push({ delivered_at: Date.now(), frame: JSON.parse(String(event.data)) }); this.wake?.(); };
  }
  override send(text: string): void {
    super.send(text);
    const frame = JSON.parse(text);
    if (frame.t === "hello") { assert.equal(frame.token, "fixture-token"); frame.token = "a".repeat(64); }
    this.native.send(JSON.stringify(frame));
  }
  override close(code: number, reason: string): void { super.close(code, reason); this.native.close(code, reason); }
  async next(): Promise<{ frame: ServerFrame; delivered_at: number }> {
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
