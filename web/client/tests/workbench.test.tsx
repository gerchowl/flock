/**
 * The MVP workbench (ADR-0038 §2a, #1594): the three desktop regions in
 * fixture mode and in wire mode, the latter driven through the wire
 * transport's scripted replay (`tests/wire/harness.ts`).
 */
import { afterEach, describe, expect, test } from "bun:test";
import { render } from "solid-js/web";

import { App } from "../src/App";
import type { ServerFrame } from "../src/gen/ServerFrame";
import { createFixtureFleet, type AgentRow, type FleetObserver, type FleetSource } from "../src/fleet";
import { emptyLayout } from "../src/layout/model";
import { createFixtureTransport } from "../src/transport/fixture";
import { createWireTransport } from "../src/wire/transport";
import { SAMPLE_INBOX } from "../src/workbench/inbox";
import { markFor } from "../src/workbench/marks";
import { SAMPLE_AGENTS } from "../src/workbench/samples";
import { shortcutLabel } from "../src/workbench/Palette";
import { Workbench } from "../src/workbench/Workbench";
import { readFixture } from "./fixture-replay";
import { Harness, WELCOME, live, sentFrames, step } from "./wire/harness";

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  dispose = undefined;
  document.body.innerHTML = "";
  localStorage.clear();
});

function mount(view: () => ReturnType<typeof App>): HTMLElement {
  const root = document.createElement("div");
  document.body.append(root);
  dispose = render(view, root);
  return root;
}

const $ = (root: ParentNode, id: string): Element | null => root.querySelector(`[data-testid="${id}"]`);
const $$ = (root: ParentNode, id: string): Element[] => [...root.querySelectorAll(`[data-testid="${id}"]`)];
const tick = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));

describe("fixture mode", () => {
  test("renders the top bar, sidebar, focus pane and inbox under the replay badge", async () => {
    const config = { transport: "fixture", fixtureJsonl: await readFixture(), source: "events.jsonl" } as const;
    const root = mount(() => <App config={{ ok: true, config }} />);
    await tick();
    expect($(root, "replay-badge")).not.toBeNull();
    expect($(root, "agent-summary")?.textContent).toBe("4 agents · 1 working");
    expect($$(root, "agent-row").map((row) => row.querySelector(".agent-row__name")?.textContent)).toEqual(["fixture", "atlas", "kite", "wren"]);
    expect($(root, "focus-pane")?.querySelector("h1")?.textContent).toBe("fixture");
    expect($(root, "transcript-synced")?.textContent).toMatch(/^End of recording · \d+ events$/);
    expect($$(root, "inbox-card").map((card) => card.querySelector("h3")?.textContent)).toEqual(["Run Bash?", "Which fixture set should the replay use?"]);
    expect(root.textContent).not.toMatch(/subject|placement/i);
  });

  test("a click on an agent row focuses it in the centre, and the layout persists", async () => {
    const config = { transport: "fixture", fixtureJsonl: "", source: "x" } as const;
    const root = mount(() => <App config={{ ok: true, config }} />);
    await tick();
    ($$(root, "agent-row")[1] as HTMLButtonElement).click();
    await tick();
    expect($(root, "focus-pane")?.querySelector("h1")?.textContent).toBe("atlas");
    expect($$(root, "agent-row")[1]?.getAttribute("aria-current")).toBe("true");
    expect(root.querySelector('[role="alert"]')?.textContent).toContain("serves only agent/fixture");
    expect(localStorage.getItem("flock.layout:fixture")).toContain("agent/atlas");
  });

  test("post-MVP actions are inert and show what they wait for as text", async () => {
    const config = { transport: "fixture", fixtureJsonl: "", source: "x" } as const;
    const root = mount(() => <App config={{ ok: true, config }} />);
    await tick();
    const reason = (button: HTMLButtonElement | undefined): string | null | undefined => {
      const id = button?.getAttribute("aria-describedby");
      return id ? document.getElementById(id)?.textContent : undefined;
    };
    const fork = [...root.querySelectorAll("button")].find((button) => button.textContent === "Fork");
    expect(fork?.getAttribute("aria-disabled")).toBe("true");
    expect(fork?.disabled).toBe(false);
    expect(reason(fork)).toBe("Waits for forks (ADR-0036 S12)");
    expect(fork?.parentElement?.textContent).toBe("ForkWaits for forks (ADR-0036 S12)");
    const before = root.innerHTML;
    fork?.click();
    expect(root.innerHTML).toBe(before);
    const allow = [...root.querySelectorAll("button")].find((button) => button.textContent === "Allow once");
    expect(reason(allow)).toBe("Answering waits for the inbox stream (G5 #1597).");
    expect(allow?.closest("article")?.textContent).toContain("Answering waits for the inbox stream (G5 #1597).");
  });
});

describe("wire mode", () => {
  test("the App connects, lists fleet-lite's agents, and shows the connection state", async () => {
    const h = new Harness();
    h.step(step({ step: "given", cursors: {}, outbox: [] }));
    const config = { transport: "wire", serverUrl: "https://fixture.invalid", token: "secret" } as const;
    const root = mount(() => <App config={{ ok: true, config }} env={{ clock: h.clock, sockets: h.net.factory, random: () => 0, store: h.store }} />);
    expect(h.net.attempts[0]?.url).toBe("wss://fixture.invalid/v1/ws");
    expect($(root, "replay-badge")).toBeNull();
    expect($(root, "sidebar")?.textContent).toBe("Loading the agent list…");
    expect(root.querySelector(".topbar__status")?.textContent).toBe("Connecting");
    h.step(step({ step: "open" }));
    h.step(step({ step: "send", frame: WELCOME }));
    const sub = sentFrames(h).find((frame) => (frame as { stream?: string }).stream === "fleet") as { sub_id: string };
    const row = { id: "atlas", label: "atlas", state: "working", activity: "Reading the ADR", waiting_on_human: [] };
    h.step(step({ step: "send", frame: { t: "gap", sub_id: sub.sub_id, from: 1, to: 0, snapshot_to: 1 } }));
    h.step(step({ step: "send", frame: { t: "ev", sub_id: sub.sub_id, stream: "fleet", seq: 1, ts_ms: 0, ev: { type: "fleet.upsert", row } } }));
    expect($$(root, "agent-row").map((agent) => agent.textContent)).toEqual(["atlasReading the ADR"]);
    expect($(root, "agent-summary")?.textContent).toBe("1 agents · 1 working");
    h.net.current().drop();
    expect(root.querySelector(".topbar__status")?.textContent).toContain("Reconnecting · Connection lost");
  });

  test("an agent list that failed says why, and an empty one says so", async () => {
    for (const [deliver, note] of [
      [(observer: FleetObserver) => observer.failed("This token cannot read the fleet."), "This token cannot read the fleet."],
      [(observer: FleetObserver) => observer.rows([]), "No agents on this server yet."],
    ] as const) {
      const fleet: FleetSource = {
        subscribe: (observer) => {
          queueMicrotask(() => deliver(observer));
          return () => undefined;
        },
      };
      const root = mount(() => <Workbench fleet={fleet} transport={createFixtureTransport("")} inbox={[]} layout={emptyLayout()} saveLayout={() => undefined} source="replay" />);
      await tick();
      expect($(root, "sidebar")?.textContent).toBe(note);
      dispose?.();
    }
  });

  test("the focus pane renders an agent's stream through the wire transport", async () => {
    const h = new Harness();
    live(h);
    const root = mount(() => (
      <Workbench
        fleet={createFixtureFleet(SAMPLE_AGENTS)}
        transport={createWireTransport(h.connection)}
        inbox={SAMPLE_INBOX}
        layout={emptyLayout()}
        saveLayout={() => undefined}
        source="live"
      />
    ));
    await tick();
    ($$(root, "agent-row")[1] as HTMLButtonElement).click();
    const sub = sentFrames(h).at(-1) as { t: string; sub_id: string; stream: string };
    expect(sub).toMatchObject({ t: "sub", stream: "agent/atlas" });
    h.step(step({ step: "send", frame: { t: "ev", sub_id: sub.sub_id, stream: "agent/atlas", seq: 1, ts_ms: 0, ev: { type: "user", message: { role: "user", content: "hello from the wire" } } } }));
    expect($(root, "focus-pane")?.textContent).toContain("Loading history…");
    h.step(step({ step: "send", frame: { t: "synced", sub_id: sub.sub_id, seq: 1 } }));
    expect($(root, "focus-pane")?.textContent).toContain("hello from the wire");
    expect($(root, "transcript-synced")?.textContent).toBe("Up to date");
    expect($(root, "focus-pane")?.textContent).not.toMatch(/recording|Replay/);
  });

  test("a live agent's refused history says so, without replay wording", async () => {
    const h = new Harness();
    live(h);
    const root = mount(() => (
      <Workbench fleet={createFixtureFleet(SAMPLE_AGENTS)} transport={createWireTransport(h.connection)} inbox={[]} layout={emptyLayout()} saveLayout={() => undefined} source="live" />
    ));
    await tick();
    ($$(root, "agent-row")[1] as HTMLButtonElement).click();
    const sub = sentFrames(h).at(-1) as { sub_id: string };
    h.step(step({ step: "send", frame: { t: "subfail", sub_id: sub.sub_id, code: "scope", message: "not for this token" } }));
    expect($(root, "focus-pane")?.querySelector('[role="alert"]')?.textContent).toBe("Couldn't load this agent's history: not for this token");
  });
});

describe("command palette", () => {
  async function workbench(fleet: FleetSource): Promise<HTMLElement> {
    const root = mount(() => <Workbench fleet={fleet} transport={createFixtureTransport("")} inbox={[]} layout={emptyLayout()} saveLayout={() => undefined} source="replay" />);
    await tick();
    return root;
  }
  const key = (target: EventTarget, init: KeyboardEventInit): boolean => target.dispatchEvent(new KeyboardEvent("keydown", { bubbles: true, ...init }));

  test("⌘K opens it; typing filters agents; Enter focuses the match", async () => {
    const root = await workbench(createFixtureFleet(SAMPLE_AGENTS));
    expect($(root, "palette")).toBeNull();
    key(document, { key: "k", metaKey: true });
    const input = $(root, "palette")?.querySelector("input") as HTMLInputElement;
    expect($$(root, "palette-item").length).toBe(4);
    input.value = "wr";
    input.dispatchEvent(new InputEvent("input", { bubbles: true }));
    expect($$(root, "palette-item").map((item) => item.textContent)).toEqual(["wrenTurn finished"]);
    key(input, { key: "Enter" });
    expect($(root, "palette")).toBeNull();
    expect($(root, "focus-pane")?.querySelector("h1")?.textContent).toBe("wren");
  });

  test("the input names the selected option, and closing returns focus to the trigger", async () => {
    const root = await workbench(createFixtureFleet(SAMPLE_AGENTS));
    const trigger = $(root, "palette-trigger") as HTMLButtonElement;
    trigger.click();
    const input = $(root, "palette")?.querySelector("input") as HTMLInputElement;
    const active = (): string | null | undefined => document.getElementById(input.getAttribute("aria-activedescendant") ?? "")?.textContent;
    expect(active()).toBe("fixtureWaiting for permission to run Bash");
    key(input, { key: "ArrowDown" });
    expect(active()).toBe("atlasRunning the wire script suite");
    key(input, { key: "Escape" });
    expect(document.activeElement).toBe(trigger);
  });

  test("Tab and Shift+Tab stay in the dialog, and options hold no nested controls", async () => {
    const root = await workbench(createFixtureFleet(SAMPLE_AGENTS));
    ($(root, "palette-trigger") as HTMLButtonElement).click();
    await tick();
    const input = $(root, "palette")?.querySelector("input") as HTMLInputElement;
    expect(document.activeElement).toBe(input);
    for (const shiftKey of [false, true]) {
      const tab = new KeyboardEvent("keydown", { key: "Tab", shiftKey, bubbles: true, cancelable: true });
      input.dispatchEvent(tab);
      expect(tab.defaultPrevented).toBe(true);
      expect(document.activeElement).toBe(input);
    }
    expect($(root, "palette")?.querySelectorAll('[role="option"] button, [role="option"] a, [role="option"] input').length).toBe(0);
    expect($$(root, "palette-item").every((item) => item.getAttribute("role") === "option")).toBe(true);
  });

  test("the shortcut reads ⌘K on Apple platforms and Ctrl K elsewhere", () => {
    expect(["MacIntel", "iPhone", "Win32", "Linux x86_64"].map(shortcutLabel)).toEqual(["⌘K", "⌘K", "Ctrl K", "Ctrl K"]);
  });

  test("a query that matches no listed agent opens one by id; Escape closes", async () => {
    const root = await workbench(createFixtureFleet([]));
    ($(root, "palette-trigger") as HTMLButtonElement).click();
    const input = $(root, "palette")?.querySelector("input") as HTMLInputElement;
    input.value = "agent-7";
    input.dispatchEvent(new InputEvent("input", { bubbles: true }));
    expect($$(root, "palette-item").map((item) => item.textContent)).toEqual(["Open agent agent-7by id"]);
    ($$(root, "palette-item")[0] as HTMLElement).click();
    expect($(root, "focus-pane")?.querySelector("h1")?.textContent).toBe("agent-7");
    key(document, { key: "k", ctrlKey: true });
    key($(root, "palette")?.querySelector("input") as HTMLInputElement, { key: "Escape" });
    expect($(root, "palette")).toBeNull();
  });
});

describe("wire mode, one transport per connection", () => {
  const config = { transport: "wire", serverUrl: "https://fixture.invalid", token: "secret" } as const;
  const send = (h: Harness, frame: ServerFrame): void => h.step(step({ step: "send", frame }));
  const agentSubs = (h: Harness): string[] =>
    sentFrames(h).flatMap((frame) => {
      const f = frame as { t: string; sub_id: string; stream?: string };
      return f.t === "sub" && f.stream?.startsWith("agent/") ? [f.sub_id] : [];
    });

  /** Mounts the wire App, Live with fleet rows atlas, kite and ember. */
  function liveApp(): { h: Harness; root: HTMLElement } {
    const h = new Harness();
    h.step(step({ step: "given", cursors: {}, outbox: [] }));
    const root = mount(() => <App config={{ ok: true, config }} env={{ clock: h.clock, sockets: h.net.factory, random: () => 0, store: h.store }} />);
    h.step(step({ step: "open" }));
    h.step(step({ step: "send", frame: WELCOME }));
    const fleet = (sentFrames(h).find((frame) => (frame as { stream?: string }).stream === "fleet") as { sub_id: string }).sub_id;
    const ids = ["atlas", "kite", "ember"];
    send(h, { t: "gap", sub_id: fleet, from: 1, to: 0, snapshot_to: ids.length });
    ids.forEach((id, i) => send(h, { t: "ev", sub_id: fleet, stream: "fleet", seq: i + 1, ts_ms: 0, ev: { type: "fleet.upsert", row: { id, label: id, state: "working", waiting_on_human: [] } } }));
    send(h, { t: "synced", sub_id: fleet, seq: ids.length });
    return { h, root };
  }

  test("switching agents twice opens three distinct sub ids, and the old subs' late frames are ignored", () => {
    const { h, root } = liveApp();
    const rows = (): HTMLButtonElement[] => $$(root, "agent-row") as HTMLButtonElement[];
    rows()[0]?.click();
    rows()[1]?.click();
    rows()[2]?.click();
    const subs = agentSubs(h);
    expect(subs).toHaveLength(3);
    expect(new Set(subs).size).toBe(3);
    const [first, second, current] = subs as [string, string, string];
    send(h, { t: "ev", sub_id: current, stream: "agent/ember", seq: 1, ts_ms: 0, ev: { type: "user", message: { role: "user", content: "ember's own line" } } });
    send(h, { t: "synced", sub_id: current, seq: 1 });
    expect(root.querySelector(".topbar__status")?.textContent).toBe("Live");
    const pane = $(root, "focus-pane")?.textContent;
    send(h, { t: "ev", sub_id: first, stream: "agent/atlas", seq: 2, ts_ms: 0, ev: { type: "user", message: { role: "user", content: "late atlas line" } } });
    send(h, { t: "synced", sub_id: second, seq: 0 });
    send(h, { t: "subfail", sub_id: first, code: "unknown_stream", message: "gone" });
    expect(root.querySelector(".topbar__status")?.textContent).toBe("Live");
    expect($(root, "focus-pane")?.textContent).toBe(pane ?? "");
    expect(pane).toContain("ember's own line");
  });

  test("a layout-load notice does not hide the connection status: after 4401 it shows NeedsToken", () => {
    localStorage.setItem(`flock.layout:${encodeURIComponent("wss://fixture.invalid/v1/ws")}`, "not json");
    const h = new Harness();
    h.step(step({ step: "given", cursors: {}, outbox: [] }));
    const root = mount(() => <App config={{ ok: true, config }} env={{ clock: h.clock, sockets: h.net.factory, random: () => 0, store: h.store }} />);
    h.step(step({ step: "open" }));
    h.step(step({ step: "token_refused", code: 4401 }));
    const status = root.querySelector(".topbar__status")?.textContent ?? "";
    expect(status).toContain("Saved layout could not be loaded");
    expect(status).toContain("NeedsToken · The server refused the token.");
  });

  test("wire mode shows no sample inbox cards; the inbox says it arrives with G5", () => {
    const { root } = liveApp();
    expect($$(root, "inbox-card")).toHaveLength(0);
    expect($(root, "inbox")?.textContent).toContain("The live inbox arrives with G5 (#1597).");
    expect($(root, "inbox")?.textContent).not.toMatch(/Sample/);
  });
});

describe("status marks", () => {
  const row = (state: AgentRow["state"], waiting: string[] = []): AgentRow => ({ id: "a", label: "a", state, activity: "", waitingOnHuman: waiting });

  test("map the server's phase, and an open item always shows as needs-you", () => {
    expect([row("working"), row("needs_human"), row("finished"), row("blocked"), row("finished", ["q1"])].map(markFor)).toEqual([
      "working",
      "needs",
      "idle",
      "failed",
      "needs",
    ]);
  });

  test("differ in shape, not only in hue", async () => {
    const root = mount(() => (
      <Workbench fleet={createFixtureFleet(SAMPLE_AGENTS)} transport={createFixtureTransport("")} inbox={[]} layout={emptyLayout()} saveLayout={() => undefined} source="replay" />
    ));
    await tick();
    const shapes = $$(root, "agent-row").map((agent) => {
      const mark = agent.querySelector("[data-mark]");
      const shape = mark?.querySelector("svg > *");
      return `${mark?.getAttribute("data-mark")}:${shape?.tagName.toLowerCase()}${shape?.getAttribute("fill") === "none" ? "-outline" : ""}`;
    });
    expect(shapes).toEqual(["needs:circle", "working:circle", "failed:circle-outline", "idle:rect"]);
    expect($$(root, "agent-row")[0]?.querySelector(".mark__count")?.textContent).toBe("1");
    expect($(root, "focus-pane")?.querySelector(".chip svg > *")).toBeNull();
    ($$(root, "agent-row")[0] as HTMLButtonElement).click();
    expect($(root, "focus-pane")?.querySelector(".chip svg > *")?.tagName.toLowerCase()).toBe("circle");
    expect($$(root, "agent-row")[0]?.querySelector("svg > *")).not.toBeNull();
  });
});
