/**
 * The sidebar's Fleet and Projects trees, the scope toggle and the node
 * status badges (ADR-0038 §1, §2, §9; ADR-0043's node-row contract, #1596).
 */
import { afterEach, describe, expect, test } from "bun:test";
import { createSignal } from "solid-js";
import { render } from "solid-js/web";

import { createFixtureFleet, type AgentRow, type FleetChange, type FleetObserver, type FleetSource } from "../src/fleet";
import type { FleetNodeRow } from "../src/gen/FleetNodeRow";
import { emptyLayout, type Layout } from "../src/layout/model";
import { DEFAULT_SIDEBAR, createSidebarStore, type SidebarPrefs } from "../src/layout/sidebar";
import { createFixtureTransport } from "../src/transport/fixture";
import { Workbench } from "../src/workbench/Workbench";
import { buildTrees, homeNodeId } from "../src/workbench/trees";

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  dispose = undefined;
  document.body.innerHTML = "";
  localStorage.clear();
});

const $ = (root: ParentNode, id: string): Element | null => root.querySelector(`[data-testid="${id}"]`);
const $$ = (root: ParentNode, id: string): Element[] => [...root.querySelectorAll(`[data-testid="${id}"]`)];
const tick = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));
const click = (element: Element | null | undefined): void => (element as HTMLElement).click();

const NODE_BASE = { health: null, latency_ms: null, last_ok: null, version: null, error: null, unbound_configured_peers: null } as const;
const ROUTE = { node: "n-b", name: "node-b", next_hop: "n-b", hops: 1 } as const;
const HOME: FleetNodeRow = { ...NODE_BASE, link_diagnostics: [], node_id: "n-a", name: "node-a", online: true, route: null, counts: { total: 4, by_phase: { working: 4 }, waiting_on_human: 0 } };
const REMOTE: FleetNodeRow = { ...NODE_BASE, link_diagnostics: [], node_id: "n-b", name: "node-b", online: true, route: ROUTE, counts: null };

const agent = (id: string, nodeId?: string, project?: string): AgentRow => ({
  id,
  label: id,
  state: "working",
  activity: "",
  waitingOnHuman: [],
  ...(nodeId === undefined ? {} : { nodeId }),
  ...(project === undefined ? {} : { project }),
});
/** Two nodes; three projects, two on the home node and one sharing its path with a project on the other. */
const AGENTS = [agent("a1", "n-a", "/p/one"), agent("a2", "n-a", "/p/two"), agent("a3", "n-b", "/p/one"), agent("a4"), agent("a5", "n-a", "/p/gone"), agent("a6", "n-a")];
/** The envelope time (home clock) of the frames that carry the node rows. */
const SENT = 1_700_000_000_000;
const entities = (nodes: readonly FleetNodeRow[]): FleetChange[] => [
  ...nodes.map((row): FleetChange => ({ op: "upsert", kind: "node", row, tsMs: SENT })),
  { op: "upsert", kind: "project", row: { id: "/p/one", node_id: "n-a", name: "one" } },
  { op: "upsert", kind: "project", row: { id: "/p/two", node_id: "n-a", name: "two" } },
  { op: "upsert", kind: "project", row: { id: "/p/one", node_id: "n-b", name: "one" } },
];

type Options = { nodes?: readonly FleetNodeRow[]; prefs?: SidebarPrefs; now?: () => number; saved?: SidebarPrefs[]; layouts?: Layout[]; mode?: "desktop" | "phone" };
async function mount(options: Options = {}): Promise<HTMLElement> {
  const root = document.createElement("div");
  document.body.append(root);
  dispose = render(
    () => (
      <Workbench
        fleet={createFixtureFleet(AGENTS, entities(options.nodes ?? [HOME, REMOTE]))}
        transport={createFixtureTransport("")}
        inbox={[]}
        sender={null}
        layout={emptyLayout()}
        saveLayout={(next) => void options.layouts?.push(next)}
        sidebar={options.prefs}
        saveSidebar={(next) => void options.saved?.push(next)}
        now={options.now}
        mode={options.mode ?? "desktop"}
        source="replay"
      />
    ),
    root,
  );
  await tick();
  return root;
}

const heading = (group: Element | null): string => [...(group?.children ?? [])].find((child) => child.classList.contains("group__head"))?.textContent ?? "";
/** The node group and project group heading an agent row sits under in the Fleet tree. */
const place = (row: Element): [string, string] => [
  row.closest('[data-testid="group-node"]')?.querySelector(".group__name")?.textContent ?? heading(row.closest('[data-testid="group-node"]')),
  heading(row.closest('[data-testid="group-project"]')),
];
const rowIds = (root: ParentNode, id = "agent-row"): string[] => $$(root, id).map((row) => row.querySelector(".agent-row__name")?.textContent ?? "");
const button = (root: ParentNode, label: string): HTMLButtonElement => [...root.querySelectorAll("button")].find((each) => each.textContent?.includes(label)) as HTMLButtonElement;

describe("the Fleet tree", () => {
  test("every agent appears once, under its node and project or an explicit group", async () => {
    const root = await mount();
    expect(rowIds(root).sort()).toEqual(["a1", "a2", "a3", "a4", "a5", "a6"]);
    const where = Object.fromEntries($$(root, "agent-row").map((row) => [row.querySelector(".agent-row__name")?.textContent, place(row)]));
    expect(where["a1"]).toEqual(["node-a", "▾one"]);
    expect(where["a2"]).toEqual(["node-a", "▾two"]);
    expect(where["a3"]).toEqual(["node-b", "▾one"]);
    // An unknown project and an absent one are never matched by label or basename.
    expect(where["a5"]).toEqual(["node-a", "▾No project"]);
    expect(where["a6"]).toEqual(["node-a", "▾No project"]);
    expect(where["a4"]![0]).toBe("▾Not bound to a node");
  });

  test("an agent on a node with no row is listed under that id, not dropped", async () => {
    const root = await mount({ nodes: [HOME] });
    expect($$(root, "agent-row").length).toBe(6);
    expect(place($$(root, "agent-row").find((row) => row.textContent?.startsWith("a3"))!)[0]).toBe("n-b");
  });

  test("the Projects tree shows project → agent, and a shared path as two entries labelled by node", async () => {
    const root = await mount();
    expect(rowIds(root, "project-agent-row").sort()).toEqual(["a1", "a2", "a3", "a4", "a5", "a6"]);
    const projects = $$(root, "group-tree")[1]!;
    const labels = [...projects.querySelectorAll('[data-testid="group-project"] > .group__head')].map((head) => head.textContent);
    expect(labels).toEqual(["▾one · node-a", "▾two · node-a", "▾one · node-b", "▾No project"]);
  });

  test("two nodes with one name are told apart by node_id", () => {
    const twin: FleetNodeRow = { ...REMOTE, name: "node-a" };
    const trees = buildTrees([], [HOME, twin], [{ id: "/p", node_id: "n-b", name: "p" }], "all", "n-a");
    expect(trees.projects.projects[0]?.label).toBe("p · node-a (n-b)");
  });
});

describe("the scope toggle", () => {
  test("defaults to all nodes; this node shows only the home node's subtree", async () => {
    const root = await mount();
    expect(button(root, "All nodes").getAttribute("aria-pressed")).toBe("true");
    click(button(root, "This node"));
    expect(rowIds(root).sort()).toEqual(["a1", "a2", "a5", "a6"]);
    expect(rowIds(root, "project-agent-row").sort()).toEqual(["a1", "a2", "a5", "a6"]);
    expect($(root, "agent-summary")?.textContent).toBe("4 agents · 4 working");
  });

  test("with no local online row, or two, this node is unknown and nothing is guessed", async () => {
    for (const nodes of [[REMOTE], [HOME, { ...HOME, node_id: "n-c", name: "node-c" }]]) {
      const root = await mount({ nodes, prefs: { scope: "node", collapsed: [] } });
      expect($(root, "home-unknown")).not.toBeNull();
      expect($$(root, "agent-row").length).toBe(0);
      expect($(root, "agent-summary")?.textContent).toBe("0 agents · 0 working");
      dispose?.();
      document.body.innerHTML = "";
    }
    expect(homeNodeId([{ ...HOME, online: false }])).toBeNull();
  });

  test("the choice and the collapsed groups survive a remount from persisted state", async () => {
    const saved: SidebarPrefs[] = [];
    const first = await mount({ saved });
    click(button(first, "This node"));
    click(first.querySelector('[data-testid="group-node"] > .group__head'));
    click(button(first, "Projects"));
    const store = createSidebarStore({ serverKey: "s" });
    store.save(saved.at(-1)!);
    dispose?.();
    document.body.innerHTML = "";
    const second = await mount({ prefs: store.load() });
    expect(button(second, "This node").getAttribute("aria-pressed")).toBe("true");
    expect(second.querySelector('[data-testid="group-node"] > .group__head')?.getAttribute("aria-expanded")).toBe("false");
    expect($$(second, "project-agent-row").length).toBe(0);
    expect(rowIds(second)).toEqual([]);
  });

  test("storage that throws on read and write leaves the defaults, and the sidebar renders", async () => {
    const storage = { getItem: () => { throw new Error("denied"); }, setItem: () => { throw new Error("quota"); } };
    const store = createSidebarStore({ serverKey: "s", storage });
    expect(store.load()).toEqual(DEFAULT_SIDEBAR);
    expect(() => store.save({ scope: "node", collapsed: ["tree:fleet"] })).not.toThrow();
    const root = await mount({ prefs: store.load() });
    expect(button(root, "All nodes").getAttribute("aria-pressed")).toBe("true");
    expect($$(root, "agent-row").length).toBe(6);
  });

  test("an unreadable stored record falls back to the defaults", () => {
    const storage = { getItem: () => '{"version":1,"prefs":{"scope":"mars","collapsed":[]}}', setItem() {} };
    expect(createSidebarStore({ serverKey: "s", storage }).load()).toEqual(DEFAULT_SIDEBAR);
  });

  test("the phone list, the palette and the summary count the agents in scope", async () => {
    const phone = await mount({ mode: "phone", prefs: { scope: "node", collapsed: [] } });
    click([...phone.querySelectorAll<HTMLButtonElement>(".tabbar > button")].find((each) => each.textContent === "Agents"));
    expect(rowIds(phone).sort()).toEqual(["a1", "a2", "a5", "a6"]);
    dispose?.();
    document.body.innerHTML = "";
    const root = await mount({ prefs: { scope: "node", collapsed: [] } });
    click($(root, "palette-trigger"));
    expect($$(root, "palette-item").length).toBe(4);
  });
});

describe("node status badges", () => {
  const node = (root: ParentNode, name: string): Element => $$(root, "group-node").find((each) => each.querySelector(".group__name")?.textContent === name)!;

  test("two nodes with one name get different Fleet headings, as in the Projects tree", async () => {
    const root = await mount({ nodes: [HOME, { ...REMOTE, name: "node-a" }] });
    const headings = $$(root, "group-node").map((each) => each.querySelector(".group__name")?.textContent);
    expect(headings).toEqual(["node-a (n-a)", "node-a (n-b)"]);
  });

  test("shows online or offline, and staleness that follows the injected clock", async () => {
    const [now, setNow] = createSignal(5_000);
    const root = await mount({ now, nodes: [{ ...HOME, last_ok: SENT }, { ...REMOTE, online: false, route: null, last_ok: SENT - 60_000 }] });
    expect($(node(root, "node-a"), "node-status")?.getAttribute("data-online")).toBe("true");
    expect($(node(root, "node-b"), "node-status")?.getAttribute("data-online")).toBe("false");
    expect($(node(root, "node-a"), "node-seen")?.textContent).toBe("seen just now");
    expect($(node(root, "node-b"), "node-seen")?.textContent).toBe("seen 1m ago");
    // Only the monotonic clock moved: the age follows it.
    setNow(5_000 + 3_600_000);
    expect($(node(root, "node-a"), "node-seen")?.textContent).toBe("seen 1h ago");
  });

  test("a browser wall clock skewed by an hour does not change the age", async () => {
    const real = Date.now;
    Date.now = () => real() + 3_600_000;
    try {
      const [now] = createSignal(5_000);
      const root = await mount({ now, nodes: [{ ...HOME, last_ok: SENT - 60_000 }, REMOTE] });
      expect($(node(root, "node-a"), "node-seen")?.textContent).toBe("seen 1m ago");
    } finally {
      Date.now = real;
    }
  });

  test("a snapshot-sourced row and a delta-sourced row are each aged from their own ts_ms", async () => {
    const [now, setNow] = createSignal(1_000);
    let observer!: FleetObserver;
    const source: FleetSource = { subscribe: (each) => ((observer = each), queueMicrotask(() => each.snapshot([{ op: "upsert", kind: "node", row: { ...HOME, last_ok: SENT }, tsMs: SENT }])), () => undefined) };
    const root = document.createElement("div");
    document.body.append(root);
    dispose = render(() => <Workbench fleet={source} transport={createFixtureTransport("")} inbox={[]} sender={null} layout={emptyLayout()} saveLayout={() => undefined} now={now} mode="desktop" source="replay" />, root);
    await tick();
    setNow(31_000);
    // The delta's frame was sent 30 s later by the home clock and says the other node was last seen at that moment minus 10 s.
    observer.change({ op: "upsert", kind: "node", row: { ...REMOTE, last_ok: SENT + 20_000 }, tsMs: SENT + 30_000 });
    expect($(node(root, "node-a"), "node-seen")?.textContent).toBe("seen 30s ago");
    expect($(node(root, "node-b"), "node-seen")?.textContent).toBe("seen 10s ago");
    setNow(91_000);
    expect($(node(root, "node-a"), "node-seen")?.textContent).toBe("seen 1m ago");
    expect($(node(root, "node-b"), "node-seen")?.textContent).toBe("seen 1m ago");
  });

  test("a row whose frame carried no time has an unknown age, and a null last_ok was never observed", async () => {
    const source = createFixtureFleet(AGENTS, [{ op: "upsert", kind: "node", row: { ...HOME, last_ok: SENT } }, { op: "upsert", kind: "node", row: REMOTE, tsMs: SENT }]);
    const root = document.createElement("div");
    document.body.append(root);
    dispose = render(() => <Workbench fleet={source} transport={createFixtureTransport("")} inbox={[]} sender={null} layout={emptyLayout()} saveLayout={() => undefined} mode="desktop" source="replay" />, root);
    await tick();
    expect($(node(root, "node-a"), "node-seen")?.textContent).toBe("age unknown");
    expect($(node(root, "node-b"), "node-seen")?.textContent).toBe("never observed");
  });

  test("shows the error code and message, and a never-seen node as such", async () => {
    const error = { code: "host_key_changed", message: "The host key changed." };
    const root = await mount({ nodes: [HOME, { ...REMOTE, online: false, error }] });
    expect($(node(root, "node-b"), "node-error")?.textContent).toBe("host_key_changed: The host key changed.");
    expect($(node(root, "node-b"), "node-seen")?.textContent).toBe("never observed");
    expect($(node(root, "node-a"), "node-error")).toBeNull();
  });

  test("counts: null is unknown, never 0, and an offline node keeps its last counts marked stale", async () => {
    const counts = { total: 7, by_phase: { working: 7 }, waiting_on_human: null };
    const root = await mount({ nodes: [HOME, { ...REMOTE, counts: null }] });
    expect($(node(root, "node-b"), "node-counts")?.textContent).toBe("agents ?");
    dispose?.();
    document.body.innerHTML = "";
    const offline = await mount({ nodes: [HOME, { ...REMOTE, online: false, route: null, counts, version: "2.1.0" }] });
    expect($(node(offline, "node-b"), "node-counts")?.textContent).toBe("7 agents · ? waiting (stale)");
    expect($(node(offline, "node-b"), "node-counts")?.getAttribute("data-stale")).toBe("true");
    expect($(node(offline, "node-b"), "node-version")?.textContent).toBe("last v2.1.0");
    expect($(node(offline, "node-a"), "node-version")).toBeNull();
  });

  test("a node that goes offline keeps its latency, marked stale", async () => {
    const root = await mount({ nodes: [{ ...HOME, latency_ms: 12 }, { ...REMOTE, online: false, route: null, latency_ms: 340 }] });
    expect($(node(root, "node-a"), "node-latency")?.textContent).toBe("12 ms");
    expect($(node(root, "node-a"), "node-latency")?.getAttribute("data-stale")).toBeNull();
    expect($(node(root, "node-b"), "node-latency")?.textContent).toBe("last 340 ms (stale)");
    expect($(node(root, "node-b"), "node-latency")?.getAttribute("data-stale")).toBe("true");
    expect($(root, "node-latency")?.textContent).toBe("12 ms");
  });

  test("a row received between ticks is aged from its own receipt, on one clock for store and view", async () => {
    const real = performance.now;
    let t = 1_000;
    performance.now = () => t;
    try {
      let observer!: FleetObserver;
      const source: FleetSource = { subscribe: (each) => ((observer = each), queueMicrotask(() => each.snapshot([{ op: "upsert", kind: "node", row: HOME, tsMs: SENT }])), () => undefined) };
      const root = document.createElement("div");
      document.body.append(root);
      dispose = render(() => <Workbench fleet={source} transport={createFixtureTransport("")} inbox={[]} sender={null} layout={emptyLayout()} saveLayout={() => undefined} mode="desktop" source="replay" />, root);
      await tick();
      // No tick of the view's 30 s timer has fired; the clock moved 20 s before the row arrived.
      t = 21_000;
      observer.change({ op: "upsert", kind: "node", row: { ...REMOTE, last_ok: SENT - 60_000 }, tsMs: SENT });
      expect($(node(root, "node-b"), "node-seen")?.textContent).toBe("seen 1m ago");
    } finally {
      performance.now = real;
    }
  });

  test("health and latency go in the tooltip, and only what was supplied", async () => {
    const root = await mount({ nodes: [{ ...HOME, latency_ms: 12, health: { cpu_percent: 40 } }, REMOTE] });
    expect($(node(root, "node-a"), "node-status")?.getAttribute("title")).toBe("latency 12 ms · cpu 40%");
  });
});

describe("opening a tile", () => {
  test("clicking an agent under any group opens agent/<id>", async () => {
    const layouts: Layout[] = [];
    const root = await mount({ layouts });
    for (const row of $$(root, "agent-row")) {
      click(row);
      const last = layouts.at(-1)!;
      expect(last.tiles[last.focused!]?.subject).toBe(`agent/${row.querySelector(".agent-row__name")?.textContent}`);
    }
  });
});
