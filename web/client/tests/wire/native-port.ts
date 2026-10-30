/** Owned native transport with callback-time delivery evidence; shared by trace providers. */
import { strict as assert } from "node:assert";
import type { ServerFrame } from "../../src/gen/ServerFrame";
import { FakeNetwork, FakeSocket } from "./harness";
export class NativePort extends FakeSocket {
  readonly native: WebSocket;
  readonly ready: Promise<void>;
  /** Native close receipt; callers still assert its code and ordering. */
  readonly ended: Promise<number>;
  /** Callback order, independent of when the interpreter drains frames. */
  readonly order: ("frame" | "close")[] = [];
  readonly frames: { frame: ServerFrame; delivered_at: number }[] = [];
  wake: (() => void) | undefined;
  error: Error | undefined;
  constructor(net: FakeNetwork, url: string) {
    super(net);
    const Socket = WebSocket as unknown as { new(url: string, options: { headers: Record<string, string> }): WebSocket };
    this.native = new Socket(url, { headers: { Origin: new URL(url.replace("ws:", "http:")).origin } });
    let closed!: (code: number) => void;
    this.ended = new Promise((resolve) => { closed = resolve; });
    this.ready = new Promise((resolve, reject) => {
      this.native.onopen = () => resolve();
      this.native.onerror = () => { this.error = new Error("transport failed"); reject(this.error); this.wake?.(); };
      this.native.onclose = (event) => { this.order.push("close"); closed(event.code); this.error = new Error("endpoint closed before trace finished"); reject(this.error); this.wake?.(); };
    });
    this.native.onmessage = (event) => { this.order.push("frame"); this.frames.push({ delivered_at: Date.now(), frame: JSON.parse(String(event.data)) }); this.wake?.(); };
  }
  override send(text: string): void {
    super.send(text);
    const frame = JSON.parse(text);
    if (frame.t === "hello") { assert.equal(frame.token, "fixture-token"); frame.token = "a".repeat(64); }
    this.native.send(JSON.stringify(frame));
  }
  override close(code: number, reason: string): void { super.close(code, reason); this.native.close(code, reason); }
  /** Opt in only when a later script step checks the native close. */
  async next(allowClosed = false): Promise<{ frame: ServerFrame; delivered_at: number }> {
    if (!this.frames.length && !this.error) await new Promise<void>((resolve) => { this.wake = resolve; });
    this.wake = undefined;
    if (this.error && !(allowClosed && this.frames.length && this.order.at(-1) === "close")) throw this.error;
    const frame = this.frames.shift(); assert(frame, "missing server frame"); return frame;
  }
}
