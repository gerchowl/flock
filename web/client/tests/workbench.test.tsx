/**
 * The MVP workbench (ADR-0038 §2a, #1594): the three desktop regions in
 * fixture mode and in wire mode, the latter driven through the wire
 * transport's scripted replay (`tests/wire/harness.ts`).
 */
import { afterEach, describe, expect, onTestFinished, test } from "bun:test";
import { createSignal } from "solid-js";
import { render } from "solid-js/web";

import { App } from "../src/App";
import type { ServerFrame } from "../src/gen/ServerFrame";
import { createFixtureFleet, type AgentRow, type FleetObserver, type FleetSource } from "../src/fleet";
import { createManager, emptyLayout, type Layout } from "../src/layout/model";
import { createFixtureTransport } from "../src/transport/fixture";
import { createWireTransport } from "../src/wire/transport";
import { STATUS_LABELS, phaseLine, rowLine, sendHint, type Sender } from "../src/workbench/Composer";
import type { Phase } from "../src/gen/Phase";
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

/** The generated `AgentState` a real fleet row carries (ADR-0028 §3a). */
const workingState = { turn: "working", background: "none", attention: "none", waiting: "none", lifecycle: "live", phase: "working", source: {} };

const $ = (root: ParentNode, id: string): Element | null => root.querySelector(`[data-testid="${id}"]`);
const $$ = (root: ParentNode, id: string): Element[] => [...root.querySelectorAll(`[data-testid="${id}"]`)];
const tick = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));
/** The fleet stream's sub id: the connection allocates every id. */
const fleetSub = (h: Harness): string => (sentFrames(h).find((frame) => (frame as { t: string; stream?: string }).t === "sub" && (frame as { stream?: string }).stream === "fleet") as { sub_id: string }).sub_id;
/** The sub id of the latest `sub` for `agent`: ids are unique per connection, not per view. */
const agentSub = (h: Harness, agent: string): string =>
  (sentFrames(h).filter((frame) => (frame as { t: string; stream?: string }).t === "sub" && (frame as { stream?: string }).stream === `agent/${agent}`).at(-1) as { sub_id: string }).sub_id;

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
    expect($(root, "connection-bar")?.querySelector("strong")?.textContent).toBe("Connecting");
    h.step(step({ step: "open" }));
    h.step(step({ step: "send", frame: WELCOME }));
    const sub = sentFrames(h).find((frame) => (frame as { stream?: string }).stream === "fleet") as { sub_id: string };
    const row = { id: "atlas", label: "atlas", state: workingState, activity: "Reading the ADR", waiting_on_human: [] };
    h.step(step({ step: "send", frame: { t: "gap", sub_id: sub.sub_id, from: 1, to: 0, snapshot_to: 1 } }));
    h.step(step({ step: "send", frame: { t: "ev", sub_id: sub.sub_id, stream: "fleet", seq: 1, ts_ms: 0, ev: { type: "fleet.upsert", row } } }));
    expect($$(root, "agent-row").map((agent) => agent.textContent)).toEqual(["atlasReading the ADR"]);
    expect($(root, "agent-summary")?.textContent).toBe("1 agents · 1 working");
    h.net.current().drop();
    expect($(root, "connection-bar")?.textContent).toContain("ReconnectingConnection lost");
    expect($(root, "connection-bar")?.textContent).not.toContain("secret");
  });

  test("an unreadable stored outbox record is named in the connection strip, and the app still loads", () => {
    const h = new Harness();
    h.store.setItem("flock.outbox.v1", JSON.stringify({ principal: "p", commands: [{ cmd_id: "c9", verb: 1 }] }));
    h.step(step({ step: "given", cursors: {}, outbox: [] }));
    const config = { transport: "wire", serverUrl: "https://fixture.invalid", token: "secret" } as const;
    const root = mount(() => <App config={{ ok: true, config }} env={{ clock: h.clock, sockets: h.net.factory, random: () => 0, store: h.store }} />);
    expect($(root, "workbench")).not.toBeNull();
    expect($(root, "outbox-problem")?.textContent).toBe("Dropped unreadable stored command: c9.");
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
      const root = mount(() => <Workbench fleet={fleet} transport={createFixtureTransport("")} inbox={[]} sender={null} layout={emptyLayout()} saveLayout={() => undefined} source="replay" />);
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
        sender={null} saveLayout={() => undefined} source="live"
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
    expect(root.querySelector(".transcript")?.textContent).not.toMatch(/recording|Replay/);
  });

  test("a live agent's refused history says so, without replay wording", async () => {
    const h = new Harness();
    live(h);
    const root = mount(() => (
      <Workbench fleet={createFixtureFleet(SAMPLE_AGENTS)} transport={createWireTransport(h.connection)} inbox={[]} sender={null} layout={emptyLayout()} saveLayout={() => undefined} source="live" />
    ));
    await tick();
    ($$(root, "agent-row")[1] as HTMLButtonElement).click();
    const sub = sentFrames(h).at(-1) as { sub_id: string };
    h.step(step({ step: "send", frame: { t: "subfail", sub_id: sub.sub_id, code: "scope", message: "not for this token" } }));
    expect($(root, "focus-pane")?.querySelector('[role="alert"]')?.textContent).toBe("Couldn't load this agent's history: not for this token");
  });
});

describe("phone", () => {
  async function phone(source: "replay" | "live" = "replay"): Promise<HTMLElement> {
    const root = mount(() => (
      <Workbench fleet={createFixtureFleet(SAMPLE_AGENTS)} transport={createFixtureTransport("")} inbox={SAMPLE_INBOX} layout={emptyLayout()} sender={null} saveLayout={() => undefined} mode="phone" source={source} />
    ));
    await tick();
    return root;
  }
  const tab = (root: HTMLElement, label: string): HTMLButtonElement | undefined =>
    [...root.querySelectorAll<HTMLButtonElement>(".tabbar > button")].find((button) => button.textContent === label);

  test("opens on the Inbox tab, which says what is blocked on the person", async () => {
    const root = await phone();
    expect($(root, "workbench")).toBeNull();
    expect(tab(root, "Inbox")?.getAttribute("aria-current")).toBe("page");
    expect($(root, "inbox-headline")?.textContent).toBe("1 blocked on you.");
    expect($$(root, "inbox-card").length).toBe(2);
    const activity = [...root.querySelectorAll<HTMLButtonElement>(".tabbar button")].find((button) => button.textContent === "Activity");
    expect(activity?.getAttribute("aria-disabled")).toBe("true");
    expect(document.getElementById(activity?.getAttribute("aria-describedby") ?? "")?.textContent).toBe("Waits for the events stream (S7-rest)");
  });

  test("live, as the wire App mounts it, the inbox shows no sample cards and says it arrives with G5", async () => {
    const root = mount(() => (
      <Workbench fleet={createFixtureFleet(SAMPLE_AGENTS)} transport={createFixtureTransport("")} inbox={[]} sender={null} layout={emptyLayout()} saveLayout={() => undefined} mode="phone" source="live" />
    ));
    await tick();
    expect($(root, "inbox-headline")?.textContent).toBe("Your inbox arrives with G5 (#1597).");
    expect($$(root, "inbox-card")).toHaveLength(0);
    expect($(root, "phone")?.textContent).not.toMatch(/Sample|blocked on you/);
  });

  test("the agent screen is a history entry: system back returns to its tab, and the URL is untouched", async () => {
    // A push without a URL keeps the page's own, `?layout=` and fixture parameters included.
    const pushes: unknown[][] = [];
    const push = history.pushState.bind(history);
    history.pushState = (...args: Parameters<History["pushState"]>) => (pushes.push(args), push(...args));
    onTestFinished(() => void (history.pushState = push));
    const root = await phone();
    tab(root, "Agents")?.click();
    const before = history.length;
    ($$(root, "agent-row")[1] as HTMLButtonElement).click();
    await tick();
    expect($(root, "phone-agent")).not.toBeNull();
    expect(history.length).toBe(before + 1);
    expect(pushes.map((args) => args.length)).toEqual([2]);
    history.back();
    await tick();
    expect($(root, "phone-agent")).toBeNull();
    expect(tab(root, "Agents")?.getAttribute("aria-current")).toBe("page");
  });

  test("an agent already focused by the saved layout: back pops nothing this screen did not push", async () => {
    const manager = createManager(emptyLayout());
    manager.open("agent/fixture", "push");
    manager.open("agent/atlas", "push");
    const saved: Layout = structuredClone(manager.state);
    const layouts: Layout[] = [];
    const root = mount(() => (
      <Workbench fleet={createFixtureFleet(SAMPLE_AGENTS)} transport={createFixtureTransport("")} inbox={[]} sender={null} layout={saved} saveLayout={(next) => void layouts.push(next)} mode="phone" source="replay" />
    ));
    await tick();
    tab(root, "Agents")?.click();
    ($$(root, "agent-row")[1] as HTMLButtonElement).click();
    await tick();
    expect($(root, "phone-agent")?.querySelector("h1")?.textContent).toBe("atlas");
    [...root.querySelectorAll<HTMLButtonElement>("button")].find((button) => button.textContent?.includes("Back"))?.click();
    await tick();
    expect($(root, "phone-agent")).toBeNull();
    const focusedSubject = (layout: Layout): string | undefined => (layout.focused === null ? undefined : layout.tiles[layout.focused]?.subject);
    expect(layouts.map(focusedSubject).filter((subject) => subject !== "agent/atlas")).toEqual([]);
  });

  test("widening past the breakpoint with the agent screen open consumes its history entry", async () => {
    const [mode, setMode] = createSignal<"phone" | "desktop">("phone");
    const root = mount(() => (
      <Workbench fleet={createFixtureFleet(SAMPLE_AGENTS)} transport={createFixtureTransport("")} inbox={[]} sender={null} layout={emptyLayout()} saveLayout={() => undefined} mode={mode()} source="replay" />
    ));
    await tick();
    const prior: unknown = history.state;
    tab(root, "Agents")?.click();
    ($$(root, "agent-row")[1] as HTMLButtonElement).click();
    await tick();
    expect(history.state).toEqual({ "flock.phone-agent": "atlas" });
    setMode("desktop");
    await tick();
    expect($(root, "workbench")).not.toBeNull();
    expect(history.state).toEqual(prior);
    expect($(root, "focus-pane")?.querySelector("h1")?.textContent).toBe("atlas");
  });

  test("Agents → an agent screen with its transcript; back returns to the tabs", async () => {
    const root = await phone();
    tab(root, "Agents")?.click();
    expect($$(root, "agent-row").length).toBe(4);
    ($$(root, "agent-row")[0] as HTMLButtonElement).click();
    await tick();
    expect($(root, "phone-agent")?.querySelector("h1")?.textContent).toBe("fixture");
    expect([...root.querySelectorAll(".segmented button")].map((button) => [button.textContent?.trim(), button.getAttribute("aria-disabled") === "true"])).toEqual([
      ["Summary", true],
      ["Transcript", false],
      ["PR", true],
      ["Terminal", true],
    ]);
    const terminal = [...root.querySelectorAll(".segmented button")].find((button) => button.textContent === "Terminal");
    expect(document.getElementById(terminal?.getAttribute("aria-describedby") ?? "")?.textContent).toBe("Terminal waits for the terminal tile.");
    expect($(root, "segmented-reasons")?.textContent).toContain("PR waits for a PR source.");
    expect($(root, "transcript-synced")).not.toBeNull();
    [...root.querySelectorAll<HTMLButtonElement>("button")].find((button) => button.textContent?.includes("Back"))?.click();
    await tick();
    expect($(root, "phone-agent")).toBeNull();
    expect($$(root, "agent-row").length).toBe(4);
  });

  test("unmounting the workbench with the agent screen open, without crossing the breakpoint, leaves history alone", async () => {
    const root = await phone();
    tab(root, "Agents")?.click();
    ($$(root, "agent-row")[1] as HTMLButtonElement).click();
    await tick();
    const entry = { "flock.phone-agent": "atlas" };
    expect(history.state).toEqual(entry);
    dispose?.();
    dispose = undefined;
    await tick();
    expect(history.state).toEqual(entry);
    // Consume the entry this test left, so later tests start from their own.
    history.back();
    await tick();
  });

  test("the wire phone path shares one transport: open, close, reopen, widen; sub ids stay unique and late frames are ignored", async () => {
    const width = Object.getOwnPropertyDescriptor(window, "innerWidth");
    const setWidth = (value: number): void => void Object.defineProperty(window, "innerWidth", { value, configurable: true, writable: true });
    onTestFinished(() => void (width === undefined ? Reflect.deleteProperty(window, "innerWidth") : Object.defineProperty(window, "innerWidth", width)));
    setWidth(375);
    const h = new Harness();
    h.step(step({ step: "given", cursors: {}, outbox: [] }));
    const config = { transport: "wire", serverUrl: "https://fixture.invalid", token: "secret" } as const;
    const root = mount(() => <App config={{ ok: true, config }} env={{ clock: h.clock, sockets: h.net.factory, random: () => 0, store: h.store }} />);
    const send = (frame: ServerFrame): void => h.step(step({ step: "send", frame }));
    h.step(step({ step: "open" }));
    send(WELCOME);
    const fleet = (sentFrames(h).find((frame) => (frame as { stream?: string }).stream === "fleet") as { sub_id: string }).sub_id;
    send({ t: "gap", sub_id: fleet, from: 1, to: 0, snapshot_to: 1 });
    send({ t: "ev", sub_id: fleet, stream: "fleet", seq: 1, ts_ms: 0, ev: { type: "fleet.upsert", row: { id: "atlas", label: "atlas", state: workingState, waiting_on_human: [] } } });
    send({ t: "synced", sub_id: fleet, seq: 1 });
    expect($(root, "phone")).not.toBeNull();
    const backButton = (): HTMLButtonElement | undefined => [...root.querySelectorAll<HTMLButtonElement>("button")].find((button) => button.textContent?.includes("Back"));
    tab(root, "Agents")?.click();
    ($$(root, "agent-row")[0] as HTMLButtonElement).click();
    await tick();
    backButton()?.click();
    await tick();
    expect($(root, "phone-agent")).toBeNull();
    ($$(root, "agent-row")[0] as HTMLButtonElement).click();
    await tick();
    expect($(root, "phone-agent")).not.toBeNull();
    setWidth(1024);
    window.dispatchEvent(new Event("resize"));
    await tick();
    expect($(root, "workbench")).not.toBeNull();
    const subs = sentFrames(h).flatMap((frame) => {
      const f = frame as { t: string; sub_id: string; stream?: string };
      return f.t === "sub" && f.stream === "agent/atlas" ? [f.sub_id] : [];
    });
    expect(subs).toHaveLength(3);
    expect(new Set(subs).size).toBe(3);
    const [first, second, current] = subs as [string, string, string];
    send({ t: "ev", sub_id: current, stream: "agent/atlas", seq: 1, ts_ms: 0, ev: { type: "user", message: { role: "user", content: "the current line" } } });
    send({ t: "synced", sub_id: current, seq: 1 });
    const pane = $(root, "focus-pane")?.textContent ?? "";
    send({ t: "ev", sub_id: first, stream: "agent/atlas", seq: 2, ts_ms: 0, ev: { type: "user", message: { role: "user", content: "a late line" } } });
    send({ t: "synced", sub_id: second, seq: 0 });
    send({ t: "subfail", sub_id: second, code: "unknown_stream", message: "gone" });
    expect(root.querySelector(".connection-bar__state")?.textContent).toBe("Live");
    expect(pane).toContain("the current line");
    expect($(root, "focus-pane")?.textContent).toBe(pane);
  });
});

describe("command palette", () => {
  async function workbench(fleet: FleetSource): Promise<HTMLElement> {
    const root = mount(() => <Workbench fleet={fleet} transport={createFixtureTransport("")} inbox={[]} layout={emptyLayout()} sender={null} saveLayout={() => undefined} source="replay" />);
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

describe("composer", () => {
  test("in fixture mode it is disabled and says why", async () => {
    const config = { transport: "fixture", fixtureJsonl: "", source: "x" } as const;
    const root = mount(() => <App config={{ ok: true, config }} />);
    await tick();
    expect($(root, "agent-phase")?.textContent).toBe("Replay: sending needs a live server.");
    expect(($(root, "composer")?.querySelector("textarea") as HTMLTextAreaElement).disabled).toBe(true);
  });

  test("sends agent.send through the outbox and shows the command's state", async () => {
    const h = new Harness();
    const config = { transport: "wire", serverUrl: "https://fixture.invalid", token: "secret" } as const;
    h.step(step({ step: "given", cursors: {}, outbox: [] }));
    const root = mount(() => <App config={{ ok: true, config }} env={{ clock: h.clock, sockets: h.net.factory, random: () => 0, store: h.store }} />);
    expect($(root, "agent-phase")).toBeNull();
    h.net.current().open();
    h.net.current().deliver(WELCOME);
    h.net.current().deliver({ t: "gap", sub_id: fleetSub(h), from: 1, to: 0, snapshot_to: 0 });
    h.net.current().deliver({ t: "synced", sub_id: fleetSub(h), seq: 0 });
    ($(root, "palette-trigger") as HTMLButtonElement).click();
    const search = $(root, "palette")?.querySelector("input") as HTMLInputElement;
    search.value = "atlas";
    search.dispatchEvent(new InputEvent("input", { bubbles: true }));
    ($$(root, "palette-item")[0] as HTMLButtonElement).click();
    expect($(root, "agent-phase")?.textContent).toBe("Agent not in the agent list");

    const box = $(root, "composer")?.querySelector("textarea") as HTMLTextAreaElement;
    box.value = "please rebase";
    box.dispatchEvent(new InputEvent("input", { bubbles: true }));
    ($(root, "composer") as HTMLFormElement).requestSubmit();
    // Opening atlas made the connection Syncing: the command waits for Live.
    expect($$(root, "sent-message").map((item) => item.getAttribute("data-status"))).toEqual(["pending"]);
    expect(sentFrames(h).some((frame) => (frame as { t: string }).t === "cmd")).toBe(false);
    h.net.current().deliver({ t: "synced", sub_id: agentSub(h, "atlas"), seq: 0 });
    const cmd = sentFrames(h).find((frame) => (frame as { t: string }).t === "cmd") as { cmd_id: string };
    expect(cmd).toMatchObject({ t: "cmd", verb: "agent.send", args: { target: "atlas", text: "please rebase", submit: true }, resume_only: false });
    expect($$(root, "sent-message").map((item) => item.getAttribute("data-status"))).toEqual(["sent"]);
    expect(box.value).toBe("");
    h.net.current().deliver({ t: "ack", cmd_id: cmd.cmd_id, ok: false, error: { code: "agent_busy", message: "try later", exit: 3, retry: "later" } });
    expect($(root, "sent-message")?.textContent).toBe("please rebaserefused by the server: try later");
  });

  test("an accepted message is never in the transcript before the server's event, and shows once after it", async () => {
    const h = new Harness();
    const config = { transport: "wire", serverUrl: "https://fixture.invalid", token: "secret" } as const;
    h.step(step({ step: "given", cursors: {}, outbox: [] }));
    const root = mount(() => <App config={{ ok: true, config }} env={{ clock: h.clock, sockets: h.net.factory, random: () => 0, store: h.store }} />);
    h.net.current().open();
    h.net.current().deliver(WELCOME);
    h.net.current().deliver({ t: "gap", sub_id: fleetSub(h), from: 1, to: 0, snapshot_to: 0 });
    h.net.current().deliver({ t: "synced", sub_id: fleetSub(h), seq: 0 });
    ($(root, "palette-trigger") as HTMLButtonElement).click();
    const search = $(root, "palette")?.querySelector("input") as HTMLInputElement;
    search.value = "atlas";
    search.dispatchEvent(new InputEvent("input", { bubbles: true }));
    ($$(root, "palette-item")[0] as HTMLButtonElement).click();
    h.net.current().deliver({ t: "synced", sub_id: agentSub(h, "atlas"), seq: 0 });
    const box = $(root, "composer")?.querySelector("textarea") as HTMLTextAreaElement;
    box.value = "please rebase";
    box.dispatchEvent(new InputEvent("input", { bubbles: true }));
    ($(root, "composer") as HTMLFormElement).requestSubmit();
    const cmd = sentFrames(h).find((frame) => (frame as { t: string }).t === "cmd") as { cmd_id: string };
    h.net.current().deliver({ t: "ack", cmd_id: cmd.cmd_id, ok: true });
    expect($$(root, "sent-message").map((item) => item.textContent)).toEqual(["please rebaseaccepted by the server"]);
    expect(root.querySelector(".transcript")?.textContent).not.toContain("please rebase");
    h.net.current().deliver({ t: "ev", sub_id: agentSub(h, "atlas"), stream: "agent/atlas", seq: 1, ts_ms: 0, ev: { type: "user", message: { role: "user", content: "please rebase" } } });
    expect(root.querySelector(".transcript")?.textContent).toContain("please rebase");
    expect($(root, "focus-pane")?.textContent?.split("please rebase").length).toBe(2);
    expect($$(root, "sent-message")).toEqual([]);
  });

  /** The wire App, focused on atlas, whose history holds `history` user texts; returns a sender and an event source. */
  function liveAtlas(history: readonly string[]): { root: HTMLElement; send: (text: string) => string; ack: (id: string) => void; said: (seq: number, text: string) => void } {
    const h = new Harness();
    const config = { transport: "wire", serverUrl: "https://fixture.invalid", token: "secret" } as const;
    h.step(step({ step: "given", cursors: {}, outbox: [] }));
    const root = mount(() => <App config={{ ok: true, config }} env={{ clock: h.clock, sockets: h.net.factory, random: () => 0, store: h.store }} />);
    h.net.current().open();
    h.net.current().deliver(WELCOME);
    h.net.current().deliver({ t: "gap", sub_id: fleetSub(h), from: 1, to: 0, snapshot_to: 0 });
    h.net.current().deliver({ t: "synced", sub_id: fleetSub(h), seq: 0 });
    ($(root, "palette-trigger") as HTMLButtonElement).click();
    const search = $(root, "palette")?.querySelector("input") as HTMLInputElement;
    search.value = "atlas";
    search.dispatchEvent(new InputEvent("input", { bubbles: true }));
    ($$(root, "palette-item")[0] as HTMLElement).click();
    const said = (seq: number, text: string): void =>
      h.net.current().deliver({ t: "ev", sub_id: agentSub(h, "atlas"), stream: "agent/atlas", seq, ts_ms: 0, ev: { type: "user", message: { role: "user", content: text } } });
    history.forEach((text, index) => said(index + 1, text));
    h.net.current().deliver({ t: "synced", sub_id: agentSub(h, "atlas"), seq: history.length });
    const send = (text: string): string => {
      const box = $(root, "composer")?.querySelector("textarea") as HTMLTextAreaElement;
      box.value = text;
      box.dispatchEvent(new InputEvent("input", { bubbles: true }));
      ($(root, "composer") as HTMLFormElement).requestSubmit();
      const cmds = sentFrames(h).filter((frame) => (frame as { t: string }).t === "cmd") as { cmd_id: string }[];
      return cmds.at(-1)?.cmd_id ?? "";
    };
    return { root, send, ack: (id) => h.net.current().deliver({ t: "ack", cmd_id: id, ok: true }), said };
  }
  const listed = (root: HTMLElement): (string | null)[] => $$(root, "sent-message").map((item) => item.textContent);

  test("the same text already in the history is not this message's event", () => {
    const { root, send, ack, said } = liveAtlas(["continue"]);
    ack(send("continue"));
    expect(listed(root)).toEqual(["continueaccepted by the server"]);
    said(2, "continue");
    expect(listed(root)).toEqual([]);
  });

  test("two identical sends need two events", () => {
    const { root, send, ack, said } = liveAtlas([]);
    const first = send("continue");
    const second = send("continue");
    ack(first);
    ack(second);
    said(1, "continue");
    expect(listed(root)).toEqual(["continueaccepted by the server"]);
    said(2, "continue");
    expect(listed(root)).toEqual([]);
  });

  test("the phase line states every reported phase as a fact", () => {
    const expected: { readonly [P in Phase]: string } = {
      working: "Agent working",
      settling: "Agent finishing its turn",
      finished: "Agent idle",
      waiting_on_event: "Agent waiting on an event",
      finished_with_background: "Agent idle, background work running",
      blocked: "Agent blocked",
      needs_human: "Agent needs you",
      offline: "Agent offline",
      hibernated: "Agent hibernated",
      limit_reached: "Agent at its usage limit",
      unknown: "Agent state unknown",
      gone: "Agent ended",
    };
    for (const [state, line] of Object.entries(expected) as [Phase, string][]) {
      expect(phaseLine(state)).toEqual({ line, open: state !== "gone" });
    }
    expect(rowLine(undefined)).toEqual({ line: "Agent not in the agent list", open: true });
  });

  test("no composer string predicts delivery: every phase and every outbox state", async () => {
    const phases: Phase[] = ["working", "settling", "finished", "waiting_on_event", "finished_with_background", "blocked", "needs_human", "offline", "hibernated", "limit_reached", "unknown", "gone"];
    const statuses = Object.keys(STATUS_LABELS) as (keyof typeof STATUS_LABELS)[];
    const commands = statuses.map((status, i) => ({ cmd_id: `c${i}`, verb: "agent.send", args: { target: "p0", text: `m${i}`, submit: true }, principal: "p", created_ms: 0, status }));
    const all: Sender = { commands: () => commands, send: () => "c9", blocked: () => null };
    const agents: AgentRow[] = [...phases.map((state, i) => ({ id: `p${i}`, label: `p${i}`, state, activity: "", waitingOnHuman: [] })), { id: "unlisted", label: "unlisted", state: "working", activity: "", waitingOnHuman: [] }];
    let shown: AgentRow[] = agents;
    const root = mount(() => <Workbench fleet={{ subscribe: (o) => (queueMicrotask(() => o.rows(shown)), () => undefined) }} transport={createFixtureTransport("")} inbox={[]} sender={all} layout={emptyLayout()} saveLayout={() => undefined} source="live" />);
    await tick();
    const texts: string[] = [];
    for (const row of $$(root, "agent-row") as HTMLButtonElement[]) {
      row.click();
      texts.push($(root, "composer")?.textContent ?? "");
    }
    expect($$(root, "sent-message")).toHaveLength(0);
    ($$(root, "agent-row")[0] as HTMLButtonElement).click();
    expect($$(root, "sent-message")).toHaveLength(statuses.length);
    texts.push($(root, "composer")?.textContent ?? "");
    shown = [];
    expect(texts).toHaveLength(agents.length + 1);
    for (const text of texts) expect(text.toLowerCase()).not.toMatch(/deliver|queued until/);
  });

  test("a message sent before the transcript is Current is not foldable: an older identical line never hides it", () => {
    const h = new Harness();
    const config = { transport: "wire", serverUrl: "https://fixture.invalid", token: "secret" } as const;
    h.step(step({ step: "given", cursors: {}, outbox: [] }));
    const root = mount(() => <App config={{ ok: true, config }} env={{ clock: h.clock, sockets: h.net.factory, random: () => 0, store: h.store }} />);
    h.net.current().open();
    h.net.current().deliver(WELCOME);
    h.net.current().deliver({ t: "gap", sub_id: fleetSub(h), from: 1, to: 0, snapshot_to: 0 });
    h.net.current().deliver({ t: "synced", sub_id: fleetSub(h), seq: 0 });
    ($(root, "palette-trigger") as HTMLButtonElement).click();
    const search = $(root, "palette")?.querySelector("input") as HTMLInputElement;
    search.value = "atlas";
    search.dispatchEvent(new InputEvent("input", { bubbles: true }));
    ($$(root, "palette-item")[0] as HTMLElement).click();
    const box = $(root, "composer")?.querySelector("textarea") as HTMLTextAreaElement;
    box.value = "continue";
    box.dispatchEvent(new InputEvent("input", { bubbles: true }));
    ($(root, "composer") as HTMLFormElement).requestSubmit();
    h.net.current().deliver({ t: "ev", sub_id: agentSub(h, "atlas"), stream: "agent/atlas", seq: 1, ts_ms: 0, ev: { type: "user", message: { role: "user", content: "continue" } } });
    h.net.current().deliver({ t: "synced", sub_id: agentSub(h, "atlas"), seq: 1 });
    const cmd = sentFrames(h).find((frame) => (frame as { t: string }).t === "cmd") as { cmd_id: string };
    h.net.current().deliver({ t: "ack", cmd_id: cmd.cmd_id, ok: true });
    expect($$(root, "sent-message").map((item) => item.textContent)).toEqual(["continueaccepted by the server"]);
  });

  test("an acked message whose echo differs in text shows as accepted, then folds at the next person's turn", () => {
    const { root, send, ack, said } = liveAtlas(["older"]);
    ack(send("please  rebase"));
    expect(listed(root)).toEqual(["please  rebaseaccepted by the server"]);
    said(2, "please rebase");
    expect(listed(root)).toEqual([]);
  });

  const sender: Sender = { commands: () => [], send: () => "c1", blocked: () => null };
  const rows: AgentRow[] = [
    { id: "atlas", label: "atlas", state: "working", activity: "", waitingOnHuman: [] },
    { id: "kite", label: "kite", state: "gone", activity: "", waitingOnHuman: [] },
  ];
  async function composing(): Promise<HTMLElement> {
    const root = mount(() => <Workbench fleet={createFixtureFleet(rows)} transport={createFixtureTransport("")} inbox={[]} sender={sender} layout={emptyLayout()} saveLayout={() => undefined} source="live" />);
    await tick();
    return root;
  }
  const textarea = (root: HTMLElement): HTMLTextAreaElement => $(root, "composer")?.querySelector("textarea") as HTMLTextAreaElement;

  test("a gone agent's composer is closed and says the agent has ended", async () => {
    const root = await composing();
    ($$(root, "agent-row")[1] as HTMLButtonElement).click();
    expect($(root, "agent-phase")?.textContent).toBe("Agent ended");
    expect(textarea(root).disabled).toBe(true);
    expect(($(root, "composer")?.querySelector('button[type="submit"]') as HTMLButtonElement).disabled).toBe(true);
  });

  test("drafts survive switching agents, and the send shortcut is shown", async () => {
    const root = await composing();
    ($$(root, "agent-row")[0] as HTMLButtonElement).click();
    textarea(root).value = "half a thought";
    textarea(root).dispatchEvent(new InputEvent("input", { bubbles: true }));
    ($$(root, "agent-row")[1] as HTMLButtonElement).click();
    expect(textarea(root).value).toBe("");
    ($$(root, "agent-row")[0] as HTMLButtonElement).click();
    expect(textarea(root).value).toBe("half a thought");
    expect($(root, "send-hint")?.textContent).toMatch(/^(⌘|Ctrl) \+ Enter to send$/);
    expect(["MacIntel", "Win32"].map(sendHint)).toEqual(["⌘ + Enter to send", "Ctrl + Enter to send"]);
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
    ids.forEach((id, i) => send(h, { t: "ev", sub_id: fleet, stream: "fleet", seq: i + 1, ts_ms: 0, ev: { type: "fleet.upsert", row: { id, label: id, state: workingState, waiting_on_human: [] } } }));
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
    expect(root.querySelector(".connection-bar__state")?.textContent).toBe("Live");
    const pane = $(root, "focus-pane")?.textContent;
    send(h, { t: "ev", sub_id: first, stream: "agent/atlas", seq: 2, ts_ms: 0, ev: { type: "user", message: { role: "user", content: "late atlas line" } } });
    send(h, { t: "synced", sub_id: second, seq: 0 });
    send(h, { t: "subfail", sub_id: first, code: "unknown_stream", message: "gone" });
    expect(root.querySelector(".connection-bar__state")?.textContent).toBe("Live");
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
    expect(root.querySelector(".topbar__status")?.textContent).toContain("Saved layout could not be loaded");
    expect(root.querySelector(".connection-bar__state")?.textContent).toBe("NeedsToken");
    expect($(root, "connection-bar")?.textContent).toContain("The server refused the token.");
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
      <Workbench fleet={createFixtureFleet(SAMPLE_AGENTS)} transport={createFixtureTransport("")} inbox={[]} layout={emptyLayout()} sender={null} saveLayout={() => undefined} source="replay" />
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
