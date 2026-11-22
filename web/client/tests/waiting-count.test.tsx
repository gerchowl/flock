import { afterEach, expect, test } from "bun:test";
import { render } from "solid-js/web";

import type { BridgeSocket } from "../src/config";
import { App } from "../src/App";
import { createFixtureFleet, type AgentRow } from "../src/fleet";
import type { FleetNodeRow } from "../src/gen/FleetNodeRow";
import { emptyLayout } from "../src/layout/model";
import { createFixtureTransport } from "../src/transport/fixture";
import { createBadgeWriter } from "../src/workbench/badge";
import { runPageShortcut } from "../src/workbench/keys";
import { Workbench } from "../src/workbench/Workbench";
import { Harness, WELCOME } from "./wire/harness";

let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); dispose = undefined; document.body.innerHTML = ""; localStorage.clear(); });
const tick = (): Promise<void> => new Promise((resolve) => queueMicrotask(resolve));
const node = (id: string, waiting: number | null): FleetNodeRow => ({ node_id: id, name: id, online: true, counts: { total: 20, by_phase: { working: 20 }, waiting_on_human: waiting }, health: null, latency_ms: null, last_ok: null, version: null, error: null, route: null, link_diagnostics: [], unbound_configured_peers: null });
const agents: AgentRow[] = [
  { id: "working", label: "working", state: "working", waitingOnHuman: [], activity: "" },
  { id: "blocked", label: "blocked", state: "blocked", waitingOnHuman: [], activity: "" },
  { id: "limited", label: "limited", state: "limit_reached", waitingOnHuman: [], activity: "" },
  { id: "permission", label: "permission", state: "needs_human", waitingOnHuman: ["r1", "r2"], activity: "" },
];
const mount = (view: () => ReturnType<typeof App>): void => { const root = document.createElement("div"); document.body.append(root); dispose = render(view, root); };

for (const scenario of [
  { name: "all-known zero", nodes: [node("a", 0), node("b", 0)], text: "Agents waiting on you", title: "flock", badge: [0, null], available: false, description: "Agents waiting on you" },
  { name: "all-known N", nodes: [node("a", 2), node("b", 5)], text: "7 agents waiting on you", title: "(7) flock", badge: [7, null], available: true, description: "7 agents waiting on you" },
  { name: "some-unknown", nodes: [node("a", 2), node("b", null)], text: "2+ agents waiting on you", title: "(2+) flock", badge: [2, "2+"], available: true, description: "2 or more agents waiting; 1 node not reporting" },
  { name: "all-unknown", nodes: [node("a", null), { ...node("b", null), counts: null }], text: "? agents waiting on you", title: "(?) flock", badge: [0, "?"], available: true, description: "agents waiting: unknown; 2 nodes not reporting" },
  { name: "unknown peers with a known zero", nodes: [node("a", 0), node("b", null)], text: "0+ agents waiting on you", title: "(0+) flock", badge: [0, "0+"], available: true, description: "0 or more agents waiting; 1 node not reporting" },
  { name: "offline retained counts", nodes: [{ ...node("a", 9), online: false }], text: "? agents waiting on you", title: "(?) flock", badge: [0, "?"], available: true, description: "agents waiting: unknown; 1 node not reporting" },
]) {
  test(`server waiting count: ${scenario.name}`, async () => {
    const badges: unknown[] = [];
    const setBadgeCount = async (count: number, label?: string | null): Promise<void> => { badges.push([count, label]); };
    mount(() => <Workbench fleet={createFixtureFleet(agents, scenario.nodes.map((row) => ({ op: "upsert", kind: "node", row })))} transport={createFixtureTransport("")} inbox={[]} sender={null} layout={emptyLayout()} saveLayout={() => {}} source="replay" setBadgeCount={setBadgeCount} />);
    await tick();
    const button = document.querySelector<HTMLButtonElement>('[data-testid="needs-you-count"]')!;
    expect(button.textContent).toBe(scenario.text);
    expect(button.getAttribute("aria-label")).toContain(scenario.description);
    expect(document.title).toBe(scenario.title);
    expect(badges.at(-1)).toEqual(scenario.badge);
    expect(button.disabled).toBe(!scenario.available);
    expect(runPageShortcut("attention.first")).toBe(scenario.available);
    expect([...document.querySelectorAll('[data-testid="needs-you-section"] [data-agent-id]')].map((el) => el.getAttribute("data-agent-id"))).toEqual(["blocked", "limited", "permission"]);
    if (scenario.available) expect(document.querySelector('[data-testid="focus-pane"] h1')?.textContent).toBe("blocked");
  });
}

test("only a fleet node count frame changes the count, not time or agent attention rows", async () => {
  const h = new Harness();
  const sent: string[] = [];
  const badges: unknown[] = [];
  const socket: BridgeSocket = { send: (text) => { sent.push(text); }, close: () => {}, onmessage: null, onerror: null, onclose: null };
  mount(() => <App config={{ ok: true, config: { transport: "bridge", openBridge: async () => ({ socket }), setBadgeCount: async (count, label) => { badges.push([count, label]); } } }} env={{ clock: h.clock, sockets: h.net.factory, random: () => 0, store: h.store }} />);
  for (let turn = 0; turn < 20 && socket.onmessage === null; turn++) await tick();
  expect(socket.onmessage).not.toBeNull();
  const send = (frame: object): void => socket.onmessage?.(JSON.stringify(frame));
  send(WELCOME);
  const sub_id = sent.map((text) => JSON.parse(text) as { stream?: string; sub_id: string }).find((frame) => frame.stream === "fleet")!.sub_id;
  send({ t: "gap", sub_id, from: 1, to: 0, snapshot_to: 1 });
  send({ t: "ev", sub_id, stream: "fleet", seq: 1, ts_ms: 0, ev: { type: "fleet.node.upsert", row: node("a", 4) } });
  send({ t: "synced", sub_id, seq: 1 }); await tick();
  expect(document.title).toBe("(4) flock"); expect(badges.at(-1)).toEqual([4, null]);
  const writes = badges.length;
  h.clock.advanceUntil(h.clock.now() + 10_000, () => false); await tick();
  expect(document.title).toBe("(4) flock"); expect(badges).toHaveLength(writes);
  send({ t: "ev", sub_id, stream: "fleet", seq: 2, ts_ms: 10_000, ev: { type: "fleet.upsert", row: { id: "blocked", label: "blocked", state: { phase: "limit_reached" }, waiting_on_human: ["r1", "r2"] } } });
  await tick(); expect(document.title).toBe("(4) flock"); expect(badges).toHaveLength(writes);
  send({ t: "ev", sub_id, stream: "fleet", seq: 3, ts_ms: 10_000, ev: { type: "fleet.node.upsert", row: node("a", 1) } });
  await tick(); expect(document.title).toBe("(1) flock"); expect(badges.at(-1)).toEqual([1, null]);
  send({ t: "ev", sub_id, stream: "fleet", seq: 4, ts_ms: 10_000, ev: { type: "fleet.node.upsert", row: node("a", null) } });
  await tick(); expect(document.title).toBe("(?) flock"); expect(badges.at(-1)).toEqual([0, "?"]);
  send({ t: "ev", sub_id, stream: "fleet", seq: 5, ts_ms: 10_000, ev: { type: "fleet.node.upsert", row: node("a", 0) } });
  await tick(); expect(document.title).toBe("flock"); expect(badges.at(-1)).toEqual([0, null]);
});


test("serialized badge writes preserve labels and coalesce to the latest clear", async () => {
  const calls: unknown[] = [];
  const pending: (() => void)[] = [];
  let visible: unknown;
  const write = createBadgeWriter((count, label) => new Promise<void>((resolve) => {
    calls.push([count, label]);
    pending.push(() => { visible = [count, label]; resolve(); });
  }));
  write(2, "2+"); write(0, "?"); write(0);
  expect(calls).toEqual([[2, "2+"]]);
  pending.shift()!(); await tick();
  expect(calls).toEqual([[2, "2+"], [0, null]]);
  pending.shift()!(); await tick();
  expect(visible).toEqual([0, null]);
});
