// The contract's `openBridge` (web/client/src/config.ts, ADR-0038 §6): one
// bridged session over the shell's three commands (src/main.rs) and one Tauri
// channel. The channel delivers in send order by `index`; the events wait for
// the client's handlers, which it sets once the session resolves. A channel
// that ends (its native side dropped) without a close closes with 1006.
({ renew }) => {
  const ipc = window.__TAURI_INTERNALS__;
  const early = new Map();
  const ready = [];
  let next = 0;
  let end = null;
  let closing = false;
  let id = null;
  let onclose = null;
  let scheduled = false;
  // One command at a time, so frames reach the native side in order.
  let tail = Promise.resolve();
  const call = (command, args) => {
    tail = tail.then(() => ipc.invoke(command, { id, ...args })).catch(() => undefined);
  };
  const deliver = () => {
    scheduled = false;
    while (onclose !== null && ready.length > 0) {
      const event = ready.shift();
      if (event.kind === "message") {
        socket.onmessage?.(event.data);
      } else {
        ipc.unregisterCallback(channel);
        onclose(event.code, event.reason);
      }
    }
  };
  const schedule = () => {
    if (!scheduled) {
      scheduled = true;
      queueMicrotask(deliver);
    }
  };
  const socket = {
    onmessage: null,
    onerror: null,
    get onclose() {
      return onclose;
    },
    set onclose(handler) {
      onclose = handler;
      schedule();
    },
    send(text) {
      call("bridge_send", { text });
    },
    close(code, reason) {
      call("bridge_close", { code: code ?? null, reason: reason ?? null });
    },
  };
  const channel = ipc.transformCallback((payload) => {
    if (payload.end) end = payload.index;
    else early.set(payload.index, payload.message);
    while (early.has(next)) {
      const event = early.get(next);
      early.delete(next++);
      if (closing) continue;
      closing = event.kind === "close";
      ready.push(event);
    }
    // `end` carries the count of messages sent, so all of them are in.
    if (end === next && !closing) {
      closing = true;
      ready.push({ kind: "close", code: 1006, reason: "" });
    }
    schedule();
  });
  return ipc.invoke("bridge_open", { renew, events: `__CHANNEL__:${channel}` }).then(
    ({ id: opened, ...session }) => {
      id = opened;
      return { socket, ...session };
    },
    (error) => {
      ipc.unregisterCallback(channel);
      throw error;
    },
  );
}
