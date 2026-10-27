/** Recorded-agent endpoint port for #2467; frames come only from the real socket. */
import { strict as assert } from "node:assert";
import { createInterface } from "node:readline";
import type { ServerFrame } from "../../src/gen/ServerFrame";
import type { FixtureLine } from "../../src/gen/FixtureLine";
import { FakeNetwork, FakeSocket, Harness, VirtualClock, parseScript } from "./harness";
import { runTrace, type CheckedStep, type StepHandlers } from "./endpoint-runner";

class EndpointClosed extends Error { constructor(readonly expected: boolean) { super("endpoint closed"); } }
class EndpointSocket extends FakeSocket {
  readonly socket: WebSocket;
  readonly ready: Promise<void>;
  readonly ended: Promise<void>;
  readonly pending: (ServerFrame | Error)[] = [];
  #wake = () => {};
  #closing = false;
  #failure: Error | undefined;
  constructor(readonly network: EndpointNetwork) {
    super(network);
    const NativeSocket = WebSocket as unknown as { new(url: string, options: { headers: Record<string, string> }): WebSocket };
    this.socket = new NativeSocket(network.url, { headers: { Origin: new URL(network.url.replace("ws:", "http:")).origin } });
    let rejectReady!: (error: Error) => void;
    this.ready = new Promise((resolve, reject) => {
      rejectReady = reject;
      this.socket.onopen = () => resolve();
      this.socket.onerror = () => { const error = new Error("endpoint transport failed"); reject(error); this.#failure = error; this.#wake(); };
    });
    this.ended = new Promise((resolve) => {
      this.socket.onclose = () => { const end = new EndpointClosed(this.#closing); rejectReady(end); this.pending.push(end); this.#wake(); resolve(); };
    });
    this.socket.onmessage = ({ data }) => {
      if (this.#failure) return;
      if (this.pending.length >= network.pendingLimit) {
        this.#failure = new Error("endpoint exceeded the authored frame budget");
        this.socket.close(1002); this.#wake(); return;
      }
      this.pending.push(JSON.parse(String(data)) as ServerFrame); this.#wake();
    };
  }
  override send(text: string): void {
    super.send(text);
    const frame = JSON.parse(text);
    // The authored credential names its provider; only the endpoint sees the bearer.
    if (frame.t === "hello" && frame.token === "fixture-token") frame.token = this.network.token;
    this.socket.send(JSON.stringify(frame));
  }
  override close(code: number, reason: string): void {
    super.close(code, reason);
    if (this.socket.readyState === WebSocket.OPEN || this.socket.readyState === WebSocket.CONNECTING) this.#closing = true;
    this.socket.close(code, reason);
  }
  async next(): Promise<ServerFrame> {
    if (this.#failure) throw this.#failure;
    if (!this.pending.length) await new Promise<void>((resolve) => { this.#wake = resolve; });
    if (this.#failure) throw this.#failure;
    const frame = this.pending.shift();
    if (!frame || frame instanceof Error) throw frame ?? new Error("missing endpoint frame");
    return frame;
  }
  assertDrained(): void {
    if (this.#failure) throw this.#failure;
    assert(this.pending.every((value) => value instanceof EndpointClosed && value.expected), "unasserted server frame or unexpected close after the trace");
  }
}
class EndpointNetwork extends FakeNetwork {
  constructor(clock: VirtualClock, readonly url: string, readonly token: string, readonly pendingLimit: number) { super(clock); }
  protected override makeSocket(): EndpointSocket { return new EndpointSocket(this); }
  override current(): EndpointSocket { return super.current() as EndpointSocket; }
}

/** Additive server fields are allowed; every authored semantic value stays exact. */
function subset(actual: unknown, expected: unknown): void {
  if (expected !== null && typeof expected === "object" && !Array.isArray(expected)) {
    assert(actual !== null && typeof actual === "object" && !Array.isArray(actual));
    for (const [key, value] of Object.entries(expected)) {
      assert(Object.hasOwn(actual, key), `missing server field ${key}`);
      subset((actual as Record<string, unknown>)[key], value);
    }
  } else assert.deepEqual(actual, expected);
}
function compare(actual: ServerFrame, expected: ServerFrame): void {
  if (actual.t === "welcome" && expected.t === "welcome") {
    // These three are semantics; version/epoch/principal are server identities.
    for (const key of ["protocol", "capabilities", "idempotency_window_ms"] as const) assert.deepEqual(actual[key], expected[key]);
  } else subset(actual, expected);
}

const [url, file, token, dropped] = process.argv.slice(2);
assert(url && file && token);
const script = { name: file.split("/").at(-1)!, lines: parseScript(await Bun.file(file).text()) };
const harness = new Harness(() => 0, (clock) => new EndpointNetwork(clock, url, token, script.lines.filter((line) => line.step === "send").length + 1));
const input = createInterface({ input: process.stdin });
const replies = input[Symbol.asyncIterator]();
const checked = (source: CheckedStep["source"]): CheckedStep => ({ checked: true, source });
const net = (): EndpointNetwork => { assert(harness.net instanceof EndpointNetwork); return harness.net; };
const synchronous = (source: CheckedStep["source"]) => async (line: Readonly<FixtureLine>): Promise<CheckedStep> => {
  harness.step(line); return checked(source);
};
const handlers: StepHandlers = {
  given: synchronous("setup"), client: synchronous("client"), expect_connect: synchronous("observation"),
  expect: synchronous("observation"), observe: synchronous("observation"),
  open: async (line) => { await net().current().ready; harness.step(line); return checked("client"); },
  send: async (line) => {
    assert(line.step === "send");
    const actual = await net().current().next();
    compare(actual, line.frame);
    harness.acceptServerFrame(actual, line.socket ?? "current");
    return checked("server");
  },
  quiet: async (line) => {
    assert(line.step === "quiet");
    console.log(JSON.stringify({ advance: line.ceiling_ms }));
    const reply = await replies.next();
    assert.equal(reply.done, false);
    assert.deepEqual(JSON.parse(reply.value), { advanced: line.ceiling_ms });
    harness.step(line);
    return checked("clock");
  },
};
if (dropped) handlers[dropped as FixtureLine["step"]] = async () => undefined;
try {
  const facts = await runTrace(script, handlers);
  harness.connection.disconnect();
  let closeTimer: ReturnType<typeof setTimeout> | undefined;
  try {
    await Promise.race([net().current().ended, new Promise((_, reject) => { closeTimer = setTimeout(() => reject(new Error("endpoint close stayed silent")), 5000); })]);
  } finally { if (closeTimer !== undefined) clearTimeout(closeTimer); }
  net().current().assertDrained();
  console.log(JSON.stringify({ complete: true, facts }));
} finally {
  harness.conn?.disconnect();
  input.close(); process.stdin.pause();
}
