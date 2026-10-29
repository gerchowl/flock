// The init script's openBridge (src/open-bridge.js) against a fake Tauri IPC.
import { expect, test } from "bun:test";

type Callback = (payload: unknown) => void;
type Call = { command: string; args: Record<string, unknown> };

const source = await Bun.file(new URL("../src/open-bridge.js", import.meta.url)).text();

/** A fake `__TAURI_INTERNALS__`: records calls, answers `bridge_open` with `opened`. */
function fakeIpc(opened: () => Promise<unknown>) {
  const calls: Call[] = [];
  /** Each send or close stays pending until the test settles it. */
  const unsettled: (() => void)[] = [];
  const callbacks = new Map<number, Callback>();
  let nextCallback = 1;
  const ipc = {
    transformCallback(callback: Callback) {
      callbacks.set(nextCallback, callback);
      return nextCallback++;
    },
    unregisterCallback(id: number) {
      callbacks.delete(id);
    },
    invoke(command: string, args: Record<string, unknown>) {
      calls.push({ command, args });
      if (command === "bridge_open") return opened();
      return new Promise((resolve) => unsettled.push(() => resolve(null)));
    },
  };
  (globalThis as unknown as { window: unknown }).window = { __TAURI_INTERNALS__: ipc };
  const openBridge = (0, eval)(source) as (request: { renew: boolean }) => Promise<Record<string, unknown>>;
  /** The channel's message `index`, as Tauri delivers it. */
  const deliver = (index: number, message: unknown) => {
    const [callback] = [...callbacks.values()];
    callback?.({ message, index });
  };
  /** The native side dropped the channel after `count` messages. */
  const endChannel = (count: number) => {
    const [callback] = [...callbacks.values()];
    callback?.({ end: true, index: count });
  };
  return { calls, callbacks, openBridge, deliver, endChannel, unsettled };
}

type Socket = {
  send(text: string): void;
  close(code?: number, reason?: string): void;
  onmessage: ((text: string) => void) | null;
  onclose: ((code: number, reason: string) => void) | null;
};

const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

test("events wait for the handlers and arrive in channel order", async () => {
  const fake = fakeIpc(() => Promise.resolve({ id: 7, expiresInMs: 1000, fresh: true }));
  const pending = fake.openBridge({ renew: true });
  expect(fake.calls[0]).toEqual({ command: "bridge_open", args: { renew: true, events: "__CHANNEL__:1" } });
  // Out of order, and before the session resolves.
  fake.deliver(1, { kind: "close", code: 4403, reason: "revoked" });
  fake.deliver(0, { kind: "message", data: '{"t":"welcome"}' });
  const session = await pending;
  expect(Object.keys(session).sort()).toEqual(["expiresInMs", "fresh", "socket"]);
  expect([session.expiresInMs, session.fresh]).toEqual([1000, true]);
  const seen: unknown[] = [];
  const socket = session.socket as Socket;
  socket.onmessage = (text) => seen.push(["message", text]);
  socket.onclose = (code, reason) => seen.push(["close", code, reason]);
  await settle();
  expect(seen).toEqual([
    ["message", '{"t":"welcome"}'],
    ["close", 4403, "revoked"],
  ]);
  expect(fake.callbacks.size).toBe(0);
});

test("sends and the close go out one at a time, in order, with the session id", async () => {
  const fake = fakeIpc(() => Promise.resolve({ id: 3 }));
  const session = await fake.openBridge({ renew: false });
  expect(Object.keys(session)).toEqual(["socket"]);
  const socket = session.socket as Socket;
  socket.send('{"t":"hello"}');
  socket.send('{"t":"cmd"}');
  socket.close();
  // The next command waits for the one before it to finish.
  for (let issued = 2; issued <= 4; issued++) {
    await settle();
    expect(fake.calls.length).toBe(issued);
    fake.unsettled.shift()?.();
  }
  expect(fake.calls.slice(1)).toEqual([
    { command: "bridge_send", args: { id: 3, text: '{"t":"hello"}' } },
    { command: "bridge_send", args: { id: 3, text: '{"t":"cmd"}' } },
    { command: "bridge_close", args: { id: 3, code: null, reason: null } },
  ]);
});

test("a refusal rejects with the shell's message and frees the channel", async () => {
  const fake = fakeIpc(() => Promise.reject("The flk server refused this app's connection: pane."));
  await expect(fake.openBridge({ renew: false })).rejects.toBe("The flk server refused this app's connection: pane.");
  expect(fake.callbacks.size).toBe(0);
});

test("a channel that ends without a close closes once with 1006", async () => {
  const fake = fakeIpc(() => Promise.resolve({ id: 5 }));
  const session = await fake.openBridge({ renew: false });
  const seen: unknown[] = [];
  const socket = session.socket as Socket;
  socket.onmessage = (text) => seen.push(["message", text]);
  socket.onclose = (code, reason) => seen.push(["close", code, reason]);
  // The end arrives before the one message sent ahead of it, and again.
  fake.endChannel(1);
  fake.deliver(0, { kind: "message", data: "{}" });
  fake.endChannel(1);
  await settle();
  expect(seen).toEqual([
    ["message", "{}"],
    ["close", 1006, ""],
  ]);
  expect(fake.callbacks.size).toBe(0);
});

test("nothing after a close reaches the socket", async () => {
  const fake = fakeIpc(() => Promise.resolve({ id: 6 }));
  const session = await fake.openBridge({ renew: false });
  const seen: unknown[] = [];
  const socket = session.socket as Socket;
  socket.onmessage = () => seen.push("message");
  socket.onclose = (code) => seen.push(code);
  fake.deliver(0, { kind: "close", code: 1000, reason: "bye" });
  fake.deliver(1, { kind: "message", data: "late" });
  fake.endChannel(2);
  await settle();
  expect(seen).toEqual([1000]);
});
