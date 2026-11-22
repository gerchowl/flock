/** Real socket fault proxy: corrupt transport identity, never server history. */
import { strict as assert } from "node:assert";
import { WireConnection, type StreamHandler } from "../../src/wire/connection";
import { browserClock, browserSockets, memoryStore } from "../../src/wire/env";

const [upstreamUrl, mode] = process.argv.slice(2);
assert(upstreamUrl && (mode === "late" || mode === "never"));
const upstreams = new Set<WebSocket>();
let old = "";
let second = "";
let corrupted = false;
const proxy = Bun.serve<{ upstream: WebSocket; queue: string[] }>({
  hostname: "127.0.0.1", port: 0,
  fetch(request, server) {
    const upstream = new WebSocket(upstreamUrl);
    const data = { upstream, queue: [] as string[] };
    upstreams.add(upstream);
    upstream.onopen = () => { for (const text of data.queue.splice(0)) upstream.send(text); };
    return server.upgrade(request, { data }) ? undefined : new Response("upgrade required", { status: 400 });
  },
  websocket: {
    open(socket) {
      socket.data.upstream.onmessage = ({ data }) => {
        assert.equal(typeof data, "string");
        const frame = JSON.parse(data as string);
        if (frame.t === "synced" && mode === "never" && !corrupted) {
          corrupted = true;
          socket.send(JSON.stringify({ t: "synced", sub_id: "never-opened", seq: 0 }));
        }
        if (frame.t === "synced" && frame.sub_id === second && mode === "late") {
          corrupted = true;
          for (const late of [
            { t: "ev", sub_id: old, stream: "agent/fixture-agent", seq: 99, ts_ms: 0, ev: { type: "assistant", message: {} } },
            { t: "gap", sub_id: old, from: 2, to: 99 },
            { t: "synced", sub_id: old, seq: 99 },
            { t: "lagged", sub_id: old, skipped_to: 99 },
            { t: "subfail", sub_id: old, code: "unknown_stream", message: "injected" },
          ]) socket.send(JSON.stringify(late));
        }
        socket.send(data as string);
      };
      socket.data.upstream.onclose = ({ code, reason }) => socket.close(code === 1005 || code === 1006 ? 1001 : code, reason);
      socket.data.upstream.onerror = () => socket.close(1001, "upstream failed");
    },
    message(socket, message) {
      assert.equal(typeof message, "string");
      if (socket.data.upstream.readyState === WebSocket.OPEN) socket.data.upstream.send(message);
      else socket.data.queue.push(message as string);
    },
    close(socket) { socket.data.upstream.close(); },
  },
});
const connection = new WireConnection({
  url: `ws://127.0.0.1:${proxy.port}/v1/ws`, token: "a".repeat(64),
  client: { name: "fixture", version: "1" }, clock: browserClock,
  sockets: browserSockets, random: () => 0.5, store: memoryStore(),
});
let release!: () => void;
let reject!: (error: Error) => void;
const completion = new Promise<void>((resolve, fail) => { release = resolve; reject = fail; });
const ceiling = setTimeout(() => reject(new Error("proxy/client producer stayed silent")), 5_000);
let stage = 0;
const handler: StreamHandler = {
  event(seq) { assert.equal(seq, 1); }, gap() { throw new Error("unexpected gap"); },
  snapshot() { throw new Error("unexpected snapshot"); }, reset() { throw new Error("unexpected reset"); },
  refused() { throw new Error("unexpected refusal"); },
  synced() {
    if (mode === "late" && stage === 0) {
      stage = 1;
      first.close();
      second = connection.subscribe("agent/fixture-agent", { ...handler, synced() { stage = 2; } }, 1).id;
    }
  },
};
const first = connection.subscribe("agent/fixture-agent", handler);
old = first.id;
connection.onChange(() => {
  if (mode === "never" && connection.state === "Reconnecting") {
    assert(corrupted);
    assert.match(connection.error ?? "", /unknown subscription never-opened/);
    release();
  }
  if (mode === "late" && stage === 2 && connection.state === "Live") {
    assert(corrupted);
    assert.equal(connection.error, null);
    assert.deepEqual([...connection.streams()], [[second, { stream: "agent/fixture-agent", state: "Current", cursor: 1 }]]);
    release();
  }
});
try { connection.connect(); await completion; }
finally {
  clearTimeout(ceiling);
  connection.disconnect();
  for (const upstream of upstreams) upstream.close();
  proxy.stop(true);
}
console.log(`verified ${mode} identity fault against real client and endpoint`);
