/** Resume events exercise the mounted app with virtual time and a silent open socket. */
import { afterEach, expect, spyOn, test } from "bun:test";
import { render } from "solid-js/web";
import type { ClientFrame } from "../src/gen/ClientFrame";
import type { ServerFrame } from "../src/gen/ServerFrame";
import { App } from "../src/App";
import { RESUME_FALLBACK_MS, RESUME_PROBE_MS } from "../src/wire/connection";
import { Harness, WELCOME, sentFrames, step } from "./wire/harness";

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  dispose = undefined;
  document.body.innerHTML = "";
  localStorage.clear();
  Reflect.deleteProperty(document, "visibilityState");
});

const frames = (h: Harness): ClientFrame[] => sentFrames(h) as ClientFrame[];

function mount(capable = true) {
  const h = new Harness();
  h.step(step({ step: "given", cursors: {}, outbox: [] }));
  const root = document.createElement("div");
  document.body.append(root);
  dispose = render(() => <App config={{ ok: true, config: { transport: "wire", serverUrl: "https://example.invalid", token: "fixture-token" } }} env={{ clock: h.clock, sockets: h.net.factory, random: () => 0, store: h.store, session: () => undefined }} />, root);
  h.net.current().open();
  h.net.current().deliver({ ...(WELCOME as Extract<ServerFrame, { t: "welcome" }>), capabilities: capable ? ["read", "resume_probe"] : ["read"] });
  for (const sub of frames(h).filter((frame) => frame.t === "sub")) {
    if (sub.t === "sub") h.net.current().deliver({ t: "synced", sub_id: sub.sub_id, seq: 0 });
  }
  return { h, root };
}

function restore() {
  const event = new Event("pageshow");
  Object.defineProperty(event, "persisted", { value: true });
  window.dispatchEvent(event);
}

test("BFCache restore probes a silently dead open socket and shows reconnecting", () => {
  const { h, root } = mount();
  restore();
  expect(frames(h).at(-1)?.t).toBe("ping");
  h.clock.advanceTo(RESUME_PROBE_MS);
  expect(root.textContent).toContain("Reconnecting");
  expect(h.net.closes).toHaveLength(1);
  h.clock.advanceTo(RESUME_PROBE_MS + 500);
  expect(h.net.attempts).toHaveLength(2);
});

function visibility(state: DocumentVisibilityState) {
  Object.defineProperty(document, "visibilityState", { value: state, configurable: true });
  document.dispatchEvent(new Event("visibilitychange"));
}

test("healthy probe answers leave the rendered state and socket unchanged", () => {
  const { h, root } = mount();
  const before = root.textContent;
  restore();
  const ping = frames(h).at(-1) as { t: string; nonce: string };
  expect(ping.t).toBe("ping");
  h.net.current().deliver(JSON.stringify({ t: "pong", nonce: ping.nonce }));
  h.clock.advanceTo(RESUME_PROBE_MS + 500);
  expect(root.textContent).toBe(before);
  expect(h.net.attempts).toHaveLength(1);
  expect(h.net.closes).toHaveLength(0);
});

test("hidden to visible probes once while rapid toggles and restore coalesce", () => {
  const { h } = mount();
  visibility("visible");
  expect(frames(h).filter((frame) => frame.t === "ping")).toHaveLength(0);
  visibility("hidden");
  visibility("visible");
  visibility("hidden");
  visibility("visible");
  restore();
  expect(frames(h).filter((frame) => frame.t === "ping")).toHaveLength(1);
  h.clock.advanceTo(RESUME_PROBE_MS + 500);
  expect(h.net.attempts).toHaveLength(2);
});

test("a non-persisted pageshow does not probe", () => {
  const { h } = mount();
  window.dispatchEvent(new Event("pageshow"));
  expect(frames(h).filter((frame) => frame.t === "ping")).toHaveLength(0);
});


test("older servers get no unsupported frame and reconnect after heartbeat plus grace", () => {
  const { h, root } = mount(false);
  restore();
  expect(frames(h).filter((frame) => frame.t === "ping")).toHaveLength(0);
  h.clock.advanceTo(RESUME_FALLBACK_MS - 1);
  expect(h.net.closes).toHaveLength(0);
  h.clock.advanceTo(RESUME_FALLBACK_MS);
  expect(root.textContent).toContain("Reconnecting");
  expect(h.net.closes).toHaveLength(1);
  h.clock.advanceTo(RESUME_FALLBACK_MS + 500);
  expect(h.net.attempts).toHaveLength(2);
});

test("an older server's regular heartbeat proves it is alive without changing the UI", () => {
  const { h, root } = mount(false);
  const before = root.textContent;
  restore();
  h.clock.advanceTo(15_000);
  h.net.current().deliver({ t: "ping", nonce: "heartbeat" });
  h.clock.advanceTo(RESUME_FALLBACK_MS + 500);
  expect(root.textContent).toBe(before);
  expect(h.net.closes).toHaveLength(0);
});

test("unmount removes both listeners and cancels an in-flight probe", () => {
  const addPage = spyOn(window, "addEventListener");
  const removePage = spyOn(window, "removeEventListener");
  const addDoc = spyOn(document, "addEventListener");
  const removeDoc = spyOn(document, "removeEventListener");
  try {
    const { h } = mount();
    restore();
    dispose?.();
    dispose = undefined;
    const pageListener = addPage.mock.calls.find(([type]) => type === "pageshow")?.[1];
    const docListener = addDoc.mock.calls.find(([type]) => type === "visibilitychange")?.[1];
    expect(pageListener).toBeDefined();
    expect(docListener).toBeDefined();
    expect(removePage.mock.calls.some(([type, listener]) => type === "pageshow" && listener === pageListener)).toBe(true);
    expect(removeDoc.mock.calls.some(([type, listener]) => type === "visibilitychange" && listener === docListener)).toBe(true);
    restore();
    visibility("hidden");
    visibility("visible");
    h.clock.advanceTo(60_000);
    expect(h.net.attempts).toHaveLength(1);
    expect(h.clock.pendingTimers).toBe(0);
  } finally {
    addPage.mockRestore();
    removePage.mockRestore();
    addDoc.mockRestore();
    removeDoc.mockRestore();
  }
});

test("wrong and replayed pongs cannot satisfy a fresh probe", () => {
  const { h } = mount();
  restore();
  const first = frames(h).at(-1) as { nonce: string };
  h.net.current().deliver(JSON.stringify({ t: "pong", nonce: first.nonce }));
  restore();
  h.net.current().deliver(JSON.stringify({ t: "pong", nonce: first.nonce }));
  h.net.current().deliver(JSON.stringify({ t: "pong", nonce: "unknown" }));
  h.clock.advanceTo(RESUME_PROBE_MS);
  expect(h.net.closes).toHaveLength(1);
});
