// The contract's `openBridge` (web/client/src/config.ts, ADR-0038 §6): one
// bridged session over the shell's three commands (src/main.rs) and one Tauri
// channel. The channel delivers in send order by `index`; the events wait for
// the client's handlers, which it sets once the session resolves.
({ renew }) => {
  const ipc = window.__TAURI_INTERNALS__;
  const early = new Map();
  const ready = [];
  let next = 0;
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
  const channel = ipc.transformCallback(({ message, index, end }) => {
    if (end) return;
    early.set(index, message);
    while (early.has(next)) {
      ready.push(early.get(next));
      early.delete(next++);
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
