/**
 * The fleet store owns the `fleet` stream's entities (ADR-0038 §3, §1, §9):
 * agents by id, nodes by node_id, projects by (node_id, id). A counting wire
 * harness stands in for the server; every assertion reads the store.
 */
import { afterEach, describe, expect, test } from "bun:test";
import { createRoot } from "solid-js";
import { render } from "solid-js/web";

import type { AgentState } from "../src/gen/AgentState";
import type { FleetNodeRow } from "../src/gen/FleetNodeRow";
import type { FleetProjectRow } from "../src/gen/FleetProjectRow";
import type { ServerFrame } from "../src/gen/ServerFrame";
import { emptyLayout } from "../src/layout/model";
import { fleetStore, type FleetReader } from "../src/store/fleet";
import type { AgentRow, FleetObserver, FleetSource } from "../src/fleet";
import { createWireFleet } from "../src/wire/fleet";
import { Workbench } from "../src/workbench/Workbench";
import { Harness, live, sentFrames, step } from "./wire/harness";

const disposers: (() => void)[] = [];
afterEach(() => {
  for (const dispose of disposers.splice(0)) dispose();
  document.body.innerHTML = "";
});

const A = "a".repeat(64);
const B = "b".repeat(64);
const state: AgentState = { turn: "idle", background: "none", attention: "none", waiting: "none", lifecycle: "live", phase: "working", source: {} };
const agent = (id: string, extra: object = {}): object => ({ id, label: id, state, waiting_on_human: [], ...extra });
const node = (node_id: string, name = node_id.slice(0, 4)): FleetNodeRow => ({
  node_id, name, health: null, latency_ms: null, last_ok: null, version: null, error: null, online: true, route: null, counts: null, unbound_configured_peers: null,
});
const project = (node_id: string, id: string, name = "app"): FleetProjectRow => ({ id, node_id, name });

function wire() {
  const h = new Harness();
  live(h);
  const source = createWireFleet(h.connection);
  let seq = 0;
  /** The fleet sub's current id: a `cursor_ahead` re-subscribes under a fresh one. */
  const sub = (): string => (sentFrames(h).filter((f) => (f as { t: string; stream?: string }).t === "sub" && (f as { stream?: string }).stream === "fleet").at(-1) as { sub_id: string }).sub_id;
  const frame = (f: ServerFrame): void => h.step(step({ step: "send", frame: f }));
  const ev = (payload: object): void => frame({ t: "ev", sub_id: sub(), stream: "fleet", seq: ++seq, ts_ms: 0, ev: payload as never });
  const subs = (): number => sentFrames(h).filter((f) => (f as { t: string; stream?: string }).t === "sub" && (f as { stream?: string }).stream === "fleet").length;
  const unsubs = (): number => sentFrames(h).filter((f) => (f as { t: string }).t === "unsub").length;
  /** A complete snapshot of `payloads` after the cursor, then synced. */
  const snapshot = (payloads: object[]): void => {
    frame({ t: "gap", sub_id: sub(), from: seq + 1, to: seq, snapshot_to: seq + payloads.length });
    payloads.forEach(ev);
    frame({ t: "synced", sub_id: sub(), seq });
  };
  return { h, source, ev, frame, snapshot, subs, unsubs, cursor: () => h.connection.streams().get(sub())?.cursor };
}

const read = (source: FleetSource): FleetReader => {
  let reader!: FleetReader;
  disposers.push(createRoot((dispose) => ((reader = fleetStore(source).read()), dispose)));
  return reader;
};
const ids = (reader: FleetReader) => ({
  agents: reader.agents.items().map((row) => row.id),
  nodes: reader.nodes.items().map((row) => row.node_id),
  projects: reader.projects.items().map((row) => `${row.node_id.slice(0, 1)}:${row.id}`),
});

describe("one record serves every reader", () => {
  test("two readers open exactly one fleet subscription; releasing one keeps it, releasing both closes it", () => {
    const w = wire();
    const first = read(w.source);
    const second = read(w.source);
    expect(w.subs()).toBe(1);
    expect(first.agents).toBe(second.agents);
    first.release();
    first.release();
    expect(w.unsubs()).toBe(0);
    second.release();
    expect(w.unsubs()).toBe(1);
    expect(w.h.connection.streams().has("s1")).toBe(false);
  });

  test("a failed record leaves the store: readers keep the failure and the next reader subscribes afresh", () => {
    const w = wire();
    const held = read(w.source);
    w.frame({ t: "subfail", sub_id: "s1", code: "scope", message: "This token cannot read the fleet." });
    expect(held.agents.progress()).toEqual({ phase: "failed", message: "This token cannot read the fleet." });
    expect(held.nodes.progress()).toEqual({ phase: "failed", message: "This token cannot read the fleet." });
    const next = read(w.source);
    expect(w.subs()).toBe(2);
    expect(next.agents.progress()).toEqual({ phase: "loading" });
    held.release();
    expect(w.unsubs()).toBe(0);
  });
});

describe("normalised by kind and id", () => {
  test("a snapshot fills every kind, a delta per kind changes one entity, and a later snapshot replaces all kinds", () => {
    const w = wire();
    const fleet = read(w.source);
    expect(fleet.agents.progress()).toEqual({ phase: "loading" });
    w.snapshot([
      { type: "fleet.upsert", row: agent("atlas") },
      { type: "fleet.upsert", row: agent("kite") },
      { type: "fleet.node.upsert", row: node(A) },
      { type: "fleet.node.upsert", row: node(B) },
      { type: "fleet.project.upsert", row: project(A, "/work/app") },
      { type: "fleet.project.upsert", row: project(A, "/work/lib") },
    ]);
    expect(ids(fleet)).toEqual({ agents: ["atlas", "kite"], nodes: [A, B], projects: ["a:/work/app", "a:/work/lib"] });
    expect([fleet.agents, fleet.nodes, fleet.projects].map((kind) => kind.progress().phase)).toEqual(["current", "current", "current"]);

    w.ev({ type: "fleet.upsert", row: agent("kite", { label: "renamed" }) });
    expect(fleet.agents.items().map((row) => row.label)).toEqual(["atlas", "renamed"]);
    expect(ids(fleet).nodes).toEqual([A, B]);
    w.ev({ type: "fleet.node.upsert", row: { ...node(A), name: "home", online: false } });
    expect(fleet.nodes.items().map((row) => [row.name, row.online])).toEqual([["home", false], [B.slice(0, 4), true]]);
    w.ev({ type: "fleet.project.upsert", row: project(A, "/work/app", "renamed") });
    expect(fleet.projects.items().map((row) => row.name)).toEqual(["renamed", "app"]);

    w.ev({ type: "fleet.remove", id: "atlas" });
    expect(ids(fleet)).toEqual({ agents: ["kite"], nodes: [A, B], projects: ["a:/work/app", "a:/work/lib"] });
    w.ev({ type: "fleet.node.remove", node_id: B });
    expect(ids(fleet)).toEqual({ agents: ["kite"], nodes: [A], projects: ["a:/work/app", "a:/work/lib"] });
    w.ev({ type: "fleet.project.remove", node_id: A, id: "/work/lib" });
    expect(ids(fleet)).toEqual({ agents: ["kite"], nodes: [A], projects: ["a:/work/app"] });

    // A later snapshot lists only a node and a project: the agent is gone, and so is every other entity it omits.
    w.snapshot([{ type: "fleet.node.upsert", row: node(B) }, { type: "fleet.project.upsert", row: project(B, "/other") }]);
    expect(ids(fleet)).toEqual({ agents: [], nodes: [B], projects: ["b:/other"] });
  });

  test("a reset empties every kind until the next snapshot", () => {
    const w = wire();
    const fleet = read(w.source);
    w.snapshot([{ type: "fleet.upsert", row: agent("atlas") }, { type: "fleet.node.upsert", row: node(A) }, { type: "fleet.project.upsert", row: project(A, "/p") }]);
    w.frame({ t: "subfail", sub_id: "s1", code: "cursor_ahead", message: "ahead" });
    expect(w.subs()).toBe(2);
    expect(ids(fleet)).toEqual({ agents: [], nodes: [], projects: [] });
    expect([fleet.agents, fleet.nodes, fleet.projects].map((kind) => kind.progress().phase)).toEqual(["loading", "loading", "loading"]);
    w.snapshot([{ type: "fleet.node.upsert", row: node(B) }]);
    expect(ids(fleet).nodes).toEqual([B]);
  });

  test("two projects with one id on different nodes are both kept, and removing one leaves the other", () => {
    const w = wire();
    const fleet = read(w.source);
    w.snapshot([{ type: "fleet.project.upsert", row: project(A, "/work/app", "on a") }, { type: "fleet.project.upsert", row: project(B, "/work/app", "on b") }]);
    expect(fleet.projects.items().map((row) => [row.node_id, row.name])).toEqual([[A, "on a"], [B, "on b"]]);
    w.ev({ type: "fleet.project.remove", node_id: A, id: "/work/app" });
    expect(fleet.projects.items().map((row) => [row.node_id, row.name])).toEqual([[B, "on b"]]);
    w.ev({ type: "fleet.project.upsert", row: project(B, "/work/app", "renamed") });
    expect(fleet.projects.items()).toHaveLength(1);
  });

  test("an agent keeps node_id and project as the server sent them, and invents none", () => {
    const w = wire();
    const fleet = read(w.source);
    w.snapshot([
      { type: "fleet.upsert", row: agent("bound", { node_id: A, project: "/work/app" }) },
      { type: "fleet.upsert", row: agent("legacy") },
      { type: "fleet.node.upsert", row: node(A) },
      { type: "fleet.project.upsert", row: project(A, "/work/app") },
    ]);
    const [bound, legacy] = fleet.agents.items();
    expect([bound?.nodeId, bound?.project]).toEqual([A, "/work/app"]);
    expect("nodeId" in (legacy ?? {})).toBe(false);
    expect("project" in (legacy ?? {})).toBe(false);
  });
});

describe("what the wire refuses and skips", () => {
  test("a malformed node delta and a malformed project delta each end the subscription visibly", () => {
    const bad = [
      { type: "fleet.node.upsert", row: { node_id: A } },
      { type: "fleet.node.upsert", row: { ...node(A), online: "yes" } },
      { type: "fleet.node.upsert", row: { ...node(A), error: { code: "x" } } },
      { type: "fleet.node.remove" },
      { type: "fleet.project.upsert", row: { id: "/p", node_id: A } },
      { type: "fleet.project.remove", id: "/p" },
    ];
    for (const payload of bad) {
      const w = wire();
      const fleet = read(w.source);
      w.snapshot([]);
      w.ev(payload);
      expect(fleet.agents.progress()).toEqual({ phase: "failed", message: expect.stringContaining("not a fleet delta") });
      expect(fleet.projects.progress().phase).toBe("failed");
      expect(w.unsubs()).toBe(1);
      expect(w.h.connection.streams().has("s1")).toBe(false);
    }
  });

  test("a malformed node inside a snapshot fails it whole, with no partial fleet", () => {
    const w = wire();
    const fleet = read(w.source);
    w.frame({ t: "gap", sub_id: "s1", from: 1, to: 0, snapshot_to: 2 });
    w.ev({ type: "fleet.upsert", row: agent("atlas") });
    w.ev({ type: "fleet.node.upsert", row: { node_id: A } });
    expect(fleet.agents.progress().phase).toBe("failed");
    expect(ids(fleet)).toEqual({ agents: [], nodes: [], projects: [] });
  });

  test("an unknown type is skipped and still advances the cursor", () => {
    const w = wire();
    const fleet = read(w.source);
    w.snapshot([{ type: "fleet.upsert", row: agent("atlas") }]);
    w.ev({ type: "unknown", raw: "future row", shape: "fixture:future" });
    w.ev({ type: "fleet.future", id: "x" });
    expect(ids(fleet).agents).toEqual(["atlas"]);
    expect(fleet.agents.progress().phase).toBe("current");
    expect(w.cursor()).toBe(3);
  });
});

describe("views read the one record", () => {
  test("a change through the store reaches the sidebar row and the permission tracker", () => {
    let observer!: FleetObserver;
    const source: FleetSource = { subscribe: (o) => ((observer = o), () => undefined) };
    const opened: string[] = [];
    const transport = { subscribe: (stream: string) => (opened.push(stream), { close: () => undefined }) };
    const root = document.createElement("div");
    document.body.append(root);
    disposers.push(
      render(
        () => <Workbench fleet={source} transport={transport as never} inbox={[]} sender={null} layout={emptyLayout()} saveLayout={() => undefined} mode="desktop" source="live" />,
        root,
      ),
    );
    const sidebar = (): string => root.querySelector('[data-testid="sidebar"]')?.textContent ?? "";
    expect(sidebar()).toContain("Loading the agent list…");
    const row = (label: string, waitingOnHuman: string[]): AgentRow => ({ id: "atlas", label, state: "working", activity: "", waitingOnHuman });
    observer.snapshot([{ op: "upsert", kind: "agent", row: row("Atlas", []) }]);
    expect(sidebar()).toContain("Atlas");
    expect(opened).toEqual([]);
    observer.change({ op: "upsert", kind: "agent", row: row("Atlas 2", ["r1"]) });
    expect(sidebar()).toContain("Atlas 2");
    expect(opened).toEqual(["agent/atlas"]);
    expect(root.querySelector('[data-testid="agent-summary"]')?.textContent).toContain("1 agents");
  });
});

test("fixture mode runs through the same store", () => {
  const source = { subscribe: (o: FleetObserver) => (o.rows([{ id: "a", label: "a", state: "working", activity: "", waitingOnHuman: [] }]), () => undefined) };
  const fleet = read(source);
  expect(ids(fleet).agents).toEqual(["a"]);
  expect(fleet.agents.progress().phase).toBe("current");
});
