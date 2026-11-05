import { afterEach, expect, test } from "bun:test";
import { render } from "solid-js/web";

import { App } from "../src/App";
import { readShellConfig, SHELL_CONFIG_GLOBAL } from "../src/config";
import { createFixtureFleet, type AgentRow, type FleetObserver } from "../src/fleet";
import { emptyLayout } from "../src/layout/model";
import { createFixtureTransport } from "../src/transport/fixture";
import { runPageShortcut } from "../src/workbench/keys";
import { Workbench } from "../src/workbench/Workbench";
import { Harness, WELCOME, sentFrames, step } from "./wire/harness";

let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); document.body.innerHTML = ""; });
const tick = async (): Promise<void> => { await new Promise((resolve) => queueMicrotask(() => resolve(undefined))); };
const row = (id: string, state: AgentRow["state"] = "working", waitingOnHuman: string[] = []): AgentRow => ({ id, label: id, state, activity: "", waitingOnHuman });
const button = (): HTMLButtonElement | null => document.querySelector('[data-testid="needs-you-count"]');
function mount(view: () => ReturnType<typeof App>): void {
  const root = document.createElement("div"); document.body.append(root); dispose = render(view, root);
}

test("counts agents once, includes typed blocked and limit states, and keeps remaining sidebar order", async () => {
  const rows = [row("working"), row("permission", "needs_human", ["a", "b"]), row("done", "finished"), row("blocked", "blocked"), row("limited", "limit_reached")];
  mount(() => <Workbench fleet={createFixtureFleet(rows)} transport={createFixtureTransport("")} inbox={[]} sender={null} layout={emptyLayout()} saveLayout={() => {}} source="replay" />);
  await tick();
  expect(button()?.textContent).toBe("3 agents need you");
  expect(document.title).toBe("(3) flock");
  const nav = document.querySelector("nav")!;
  expect(nav.querySelector("section")?.getAttribute("aria-label")).toBe("Needs you");
  expect([...nav.querySelectorAll('[data-testid="agent-row"]')].map((el) => el.getAttribute("data-agent-id"))).toEqual(["permission", "blocked", "limited", "working", "done"]);
  expect([...document.querySelectorAll('[data-testid="inbox-card"] h3')].map((el) => el.textContent)).toEqual(["Agent is blocked", "Agent reached a limit"]);
  button()!.click();
  expect(document.querySelector('[data-testid="focus-pane"] h1')?.textContent).toBe("permission");
  expect(runPageShortcut("attention.first")).toBe(true);
  document.querySelector<HTMLButtonElement>('[data-testid="palette-trigger"]')?.click();
  expect(document.querySelector('[data-testid="palette"]')?.textContent ?? document.body.textContent).toContain("First agent needing you");
});

test("server fleet frames update and clear title and count, ignoring terminal events and node totals", async () => {
  const h = new Harness(); h.step(step({ step: "given", cursors: {}, outbox: [] }));
  mount(() => <App config={{ ok: true, config: { transport: "wire", serverUrl: "https://fixture.invalid", token: "secret" } }} env={{ clock: h.clock, sockets: h.net.factory, random: () => 0, store: h.store }} />);
  h.step(step({ step: "open" })); h.step(step({ step: "send", frame: WELCOME }));
  const sub_id = (sentFrames(h).find((f) => (f as { stream?: string }).stream === "fleet") as { sub_id: string }).sub_id;
  const send = (frame: object): void => h.step(step({ step: "send", frame: frame as never }));
  send({ t: "gap", sub_id, from: 1, to: 0, snapshot_to: 1 });
  const agent = { id: "structured", label: "structured", state: { phase: "limit_reached" }, waiting_on_human: [] };
  send({ t: "ev", sub_id, stream: "fleet", seq: 1, ts_ms: 0, ev: { type: "fleet.upsert", row: agent } });
  send({ t: "synced", sub_id, seq: 1 }); await tick();
  expect(button()?.textContent).toBe("1 agent needs you"); expect(document.title).toBe("(1) flock");
  send({ t: "ev", sub_id, stream: "fleet", seq: 2, ts_ms: 0, ev: { type: "pane.upsert", row: { ...agent, id: "pty" } } });
  send({ t: "ev", sub_id, stream: "fleet", seq: 3, ts_ms: 0, ev: { type: "fleet.node.upsert", row: { node_id: "fixture", name: "fixture", online: true, health: null, latency_ms: null, last_ok: null, version: null, error: null, route: null, counts: { total: 99, waiting_on_human: 99 }, unbound_configured_peers: null } } });
  await tick(); expect(document.title).toBe("(1) flock"); expect(document.body.textContent).not.toContain("pty");
  send({ t: "ev", sub_id, stream: "fleet", seq: 4, ts_ms: 0, ev: { type: "fleet.remove", id: "structured" } });
  await tick(); expect(document.title).toBe("flock"); expect(button()?.disabled).toBe(true);
});

test("shell badge receives the same count and clears on updates and disposal", async () => {
  const counts: number[] = [];
  const setBadgeCount = async (count: number): Promise<void> => { counts.push(count); };
  const result = readShellConfig({ [SHELL_CONFIG_GLOBAL]: { transport: "bridge", openBridge: async () => undefined, setBadgeCount } });
  expect(result.ok && "setBadgeCount" in result.config && result.config.setBadgeCount).toBe(setBadgeCount);
  let observer!: FleetObserver;
  mount(() => <Workbench fleet={{ subscribe(next) { observer = next; return () => {}; } }} transport={createFixtureTransport("")} inbox={[]} sender={null} layout={emptyLayout()} saveLayout={() => {}} source="replay" {...{ setBadgeCount }} />);
  observer.rows([row("blocked", "blocked"), row("limited", "limit_reached")]); await tick();
  expect(counts.at(-1)).toBe(2);
  observer.rows([]); await tick(); expect(counts.at(-1)).toBe(0);
  observer.rows([row("blocked", "blocked")]); await tick(); dispose?.(); dispose = undefined;
  await tick(); expect(counts.at(-1)).toBe(0); expect(document.title).toBe("flock");
});

test("a desktop shell's real bridge frames reach its badge through App", async () => {
  const counts: number[] = [];
  const sent: string[] = [];
  const socket: import("../src/config").BridgeSocket = {
    send: (text) => { sent.push(text); }, close: () => {}, onmessage: null, onclose: null, onerror: null,
  };
  const config = readShellConfig({ [SHELL_CONFIG_GLOBAL]: {
    transport: "bridge", openBridge: async () => ({ socket }), setBadgeCount: async (count: number) => { counts.push(count); },
  } });
  const h = new Harness();
  mount(() => <App config={config} env={{ clock: h.clock, sockets: h.net.factory, random: () => 0, store: h.store }} />);
  for (let turn = 0; turn < 20 && socket.onmessage === null; turn++) await tick();
  expect(socket.onmessage).not.toBeNull();
  socket.onmessage?.(JSON.stringify(WELCOME));
  const sub = sent.map((text) => JSON.parse(text) as { stream?: string; sub_id: string }).find((frame) => frame.stream === "fleet")!;
  const deliver = (frame: object): void => socket.onmessage?.(JSON.stringify(frame));
  deliver({ t: "gap", sub_id: sub.sub_id, from: 1, to: 0, snapshot_to: 1 });
  deliver({ t: "ev", sub_id: sub.sub_id, stream: "fleet", seq: 1, ts_ms: 0, ev: { type: "fleet.upsert", row: { id: "limited", label: "limited", state: { phase: "limit_reached" }, waiting_on_human: [] } } });
  deliver({ t: "synced", sub_id: sub.sub_id, seq: 1 });
  await tick();
  expect(counts.at(-1)).toBe(1); expect(document.title).toBe("(1) flock");
});

for (const clear of ["update", "dispose"] as const) {
  test(`a delayed badge write cannot overwrite the latest clear on ${clear}`, async () => {
    let observer!: FleetObserver;
    let visible = 0;
    let inFlight = 0;
    let maximum = 0;
    const pending: (() => void)[] = [];
    const setBadgeCount = (count: number): Promise<void> => new Promise((resolve) => {
      maximum = Math.max(maximum, ++inFlight);
      pending.push(() => { visible = count; inFlight--; resolve(); });
    });
    mount(() => <Workbench fleet={{ subscribe(next) { observer = next; return () => {}; } }} transport={createFixtureTransport("")} inbox={[]} sender={null} layout={emptyLayout()} saveLayout={() => {}} source="replay" setBadgeCount={setBadgeCount} />);
    pending.pop()?.(); await tick();
    observer.rows([row("blocked", "blocked")]); await tick();
    if (clear === "update") observer.rows([]);
    else { dispose?.(); dispose = undefined; }
    await tick();
    // Finish newer requests first: the old implementation then applies its stale one last.
    for (let turn = 0; turn < 4 && pending.length > 0; turn++) { pending.pop()?.(); await tick(); }
    expect(inFlight).toBe(0);
    expect(visible).toBe(0);
    expect(maximum).toBe(1);
    expect(document.title).toBe("flock");
  });
}

test("the registry, palette and toolbar share attention command availability", async () => {
  let observer!: FleetObserver;
  mount(() => <Workbench fleet={{ subscribe(next) { observer = next; return () => {}; } }} transport={createFixtureTransport("")} inbox={[]} sender={null} layout={emptyLayout()} saveLayout={() => {}} source="replay" />);
  observer.rows([]); await tick();
  document.querySelector<HTMLButtonElement>('[data-testid="palette-trigger"]')!.click();
  const command = (): HTMLElement => [...document.querySelectorAll<HTMLElement>('[data-testid="palette-command"]')].find((el) => el.textContent?.startsWith("First agent needing you"))!;
  expect(command().getAttribute("aria-disabled")).toBe("true");
  command().click();
  expect(document.querySelector('[data-testid="palette"]')).not.toBeNull();
  expect(runPageShortcut("attention.first")).toBe(false);
  expect(button()?.disabled).toBe(true);
  observer.rows([row("blocked", "blocked")]); await tick();
  expect(command().getAttribute("aria-disabled")).toBeNull();
  expect(button()?.disabled).toBe(false);
  expect(runPageShortcut("attention.first")).toBe(true);
  expect(document.querySelector('[data-testid="focus-pane"] h1')?.textContent).toBe("blocked");
  observer.rows([]); await tick();
  expect(command().getAttribute("aria-disabled")).toBe("true");
  expect(runPageShortcut("attention.first")).toBe(false);
});
