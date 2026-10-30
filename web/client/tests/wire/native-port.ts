/** Owned native transport with callback-time delivery evidence; shared by trace providers. */
import { strict as assert } from "node:assert";
import type { ServerFrame } from "../../src/gen/ServerFrame";
import { FakeNetwork, FakeSocket } from "./harness";
export class NativePort extends FakeSocket {
  readonly native: WebSocket;
  readonly ready: Promise<void>;
  readonly frames: { frame: ServerFrame; delivered_at: number }[] = [];
  wake: (() => void) | undefined;
  error: Error | undefined;
  constructor(net: FakeNetwork, url: string) {
    super(net);
    const Socket = WebSocket as unknown as { new(url: string, options: { headers: Record<string, string> }): WebSocket };
    this.native = new Socket(url, { headers: { Origin: new URL(url.replace("ws:", "http:")).origin } });
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
