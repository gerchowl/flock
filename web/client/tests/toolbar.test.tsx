/**
 * The unified toolbar (#2901, ADR-0048 D7 amendment 5): one bar with the
 * connection's status dot and popover, the ⌘K search and the trailing
 * actions; a banner only for problems.
 */
import { afterEach, describe, expect, test } from "bun:test";
import { render } from "solid-js/web";

import { App, BRIDGE_HOST } from "../src/App";
import { createFixtureFleet } from "../src/fleet";
import type { FleetNodeRow } from "../src/gen/FleetNodeRow";
import type { ServerFrame } from "../src/gen/ServerFrame";
import { emptyLayout } from "../src/layout/model";
import { createFixtureTransport } from "../src/transport/fixture";
import { WIRE_PROTOCOL, type ConnectionState } from "../src/wire/connection";
import { healthOf, type Health, type HostView } from "../src/workbench/HostStatus";
import { CONNECTION_COPY } from "../src/workbench/refusals";
import { SAMPLE_AGENTS } from "../src/workbench/samples";
import { COMMAND, LAYER_COMMANDS } from "./keys-command";
import { Workbench } from "../src/workbench/Workbench";
import { readFixture } from "./fixture-replay";
import { Harness, WELCOME, sentFrames, step } from "./wire/harness";

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

const $ = (root: ParentNode, id: string): HTMLElement | null => root.querySelector(`[data-testid="${id}"]`);
const tick = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));
const key = (target: EventTarget, init: KeyboardEventInit): boolean => target.dispatchEvent(new KeyboardEvent("keydown", { bubbles: true, cancelable: true, ...init }));
const config = { transport: "wire", serverUrl: "https://fixture.invalid", token: "secret" } as const;

/** Settles the fleet stream with no rows, which takes a welcomed connection to Live. */
function settle(h: Harness): void {
  const fleet = ([...sentFrames(h)].reverse().find((frame) => (frame as { stream?: string }).stream === "fleet") as { sub_id: string }).sub_id;
  h.step(step({ step: "send", frame: { t: "gap", sub_id: fleet, from: 1, to: 0, snapshot_to: 0 } }));
  h.step(step({ step: "send", frame: { t: "synced", sub_id: fleet, seq: 0 } }));
}

/** The App on a scripted server, opened and, unless `welcome` is null, welcomed and settled. */
function wireApp(welcome: ServerFrame | null = WELCOME): { h: Harness; root: HTMLElement } {
  const h = new Harness();
  h.step(step({ step: "given", cursors: {}, outbox: [] }));
  const root = mount(() => <App config={{ ok: true, config }} env={{ clock: h.clock, sockets: h.net.factory, random: () => 0, store: h.store }} />);
  h.step(step({ step: "open" }));
  if (welcome !== null) {
    h.step(step({ step: "send", frame: welcome }));
    settle(h);
  }
  return { h, root };
}

/** The App's connection state, as the popover's state line starts. */
/** The connection state the popover's plain line words (Hello reads as Connecting). */
const state = (root: ParentNode): string => {
  const line = $(root, "host-detail")?.textContent ?? "";
  return (Object.keys(CONNECTION_COPY) as (keyof typeof CONNECTION_COPY)[]).find((name) => line.startsWith(CONNECTION_COPY[name])) ?? line;
};

const health = (root: ParentNode): string | null | undefined => $(root, "host-status")?.getAttribute("data-health");
/** What the banner says; empty while nothing needs the person. */
const banner = (root: ParentNode): string => $(root, "connection-banner")?.textContent ?? "";
const occurrences = (text: string, word: string): number => text.split(word).length - 1;

describe("one toolbar", () => {
  test("live: one header, no wordmark, the state named once, and no banner", () => {
    const { root } = wireApp();
    expect(root.querySelectorAll("header").length).toBe(1);
    const toolbar = root.querySelector("header") as HTMLElement;
    expect(toolbar.textContent).not.toContain("flock");
    expect(toolbar.contains($(root, "host-status"))).toBe(true);
    expect(toolbar.contains($(root, "palette-trigger"))).toBe(true);
    expect($(root, "host-status")?.getAttribute("aria-label")).toBe("Connection to fixture.invalid");
    expect(health(root)).toBe("live");
    expect(occurrences(root.textContent ?? "", "Connected.")).toBe(1);
    expect($(root, "host-detail")?.textContent).toBe("Connected.");
    expect(root.textContent).not.toContain("Live");
    expect(banner(root)).toBe("");
  });

  test("fixture mode has no connection, so no status dot: the replay badge says what it is", async () => {
    const fixture = { transport: "fixture", fixtureJsonl: await readFixture(), source: "events.jsonl" } as const;
    const root = mount(() => <App config={{ ok: true, config: fixture }} />);
    await tick();
    // The focused agent's header sits in the focus pane; the window has one toolbar.
    expect([...root.querySelectorAll("header")].filter((header) => header.closest("main") === null).length).toBe(1);
    expect($(root, "host-status")).toBeNull();
    expect(root.querySelector("header")?.textContent).not.toContain("flock");
  });

  test("the server is named by its home node once the fleet sends one, else by its address", async () => {
    const node: FleetNodeRow = {
      node_id: "n-a",
      name: "node-a",
      online: true,
      route: null,
      health: null,
      latency_ms: null,
      last_ok: null,
      version: null,
      error: null,
      unbound_configured_peers: null,
      link_diagnostics: [],
      counts: { total: 0, by_phase: {}, waiting_on_human: 0 },
    };
    const view: HostView = { name: BRIDGE_HOST, health: () => "live", detail: () => "Live", diagnosis: () => null, connected: () => true, connect: () => undefined, disconnect: () => undefined };
    for (const [nodes, name] of [[[], BRIDGE_HOST], [[node], "node-a"]] as const) {
      const fleet = createFixtureFleet(SAMPLE_AGENTS, nodes.map((row) => ({ op: "upsert", kind: "node", row, tsMs: 0 })));
      const root = mount(() => <Workbench fleet={fleet} transport={createFixtureTransport("")} inbox={[]} layout={emptyLayout()} sender={null} saveLayout={() => undefined} source="live" host={view} />);
      await tick();
      expect($(root, "host-status")?.textContent).toBe(name);
      dispose?.();
      document.body.innerHTML = "";
    }
  });
});

describe("the connection popover", () => {
  test("the dot opens it with the state line and Disconnect; Disconnect stops the connection and offers Connect", async () => {
    const { root } = wireApp();
    const trigger = $(root, "host-status") as HTMLButtonElement;
    expect([trigger.getAttribute("aria-expanded"), $(root, "host-panel")?.hidden]).toEqual(["false", true]);
    expect(trigger.getAttribute("aria-controls")).toBe($(root, "host-panel")?.id ?? "missing");
    trigger.click();
    await tick();
    expect([trigger.getAttribute("aria-expanded"), $(root, "host-panel")?.hidden]).toEqual(["true", false]);
    expect(document.activeElement).toBe($(root, "host-toggle"));
    expect($(root, "host-toggle")?.textContent).toBe("Disconnect");
    $(root, "host-toggle")?.click();
    expect(state(root)).toBe("Disconnected");
    expect([health(root), $(root, "host-detail")?.textContent, $(root, "host-toggle")?.textContent]).toEqual(["off", "Not connected.", "Connect"]);
    // A chosen disconnect is no problem: no banner.
    expect(banner(root)).toBe("");
    $(root, "host-toggle")?.click();
    expect(state(root)).toBe("Connecting");
  });

  test("Esc closes it and returns focus to the dot; a click elsewhere closes it", async () => {
    const { root } = wireApp();
    const trigger = $(root, "host-status") as HTMLButtonElement;
    trigger.click();
    await tick();
    key(document.activeElement ?? document, { key: "Escape" });
    expect([$(root, "host-panel")?.hidden, document.activeElement]).toEqual([true, trigger]);
    trigger.click();
    expect($(root, "host-panel")?.hidden).toBe(false);
    $(root, "focus-pane")?.dispatchEvent(new Event("pointerdown", { bubbles: true }));
    expect($(root, "host-panel")?.hidden).toBe(true);
  });

  test("⌘K lists the connection actions as commands, and choosing Disconnect disconnects", async () => {
    const { root } = wireApp();
    key(document, { key: "k", ...COMMAND });
    const commands = [...root.querySelectorAll('[data-testid="palette-command"]')] as HTMLElement[];
    expect(commands.map((command) => command.firstElementChild?.textContent)).toEqual(["Connection to fixture.invalid", "Disconnect", ...LAYER_COMMANDS.map((label) => label === "First agent needing you" ? `${label} (? agents waiting on you; agents waiting: unknown; no nodes reporting)` : label)]);
    commands[1]?.click();
    expect([state(root), $(root, "palette")]).toEqual(["Disconnected", null]);
    key(document, { key: "k", ...COMMAND });
    const reopen = [...root.querySelectorAll('[data-testid="palette-command"]')] as HTMLElement[];
    expect(reopen.map((command) => command.firstElementChild?.textContent)).toEqual(["Connection to fixture.invalid", "Connect", ...LAYER_COMMANDS.map((label) => label === "First agent needing you" ? `${label} (? agents waiting on you; agents waiting: unknown; no nodes reporting)` : label)]);
    reopen[0]?.click();
    await tick();
    expect($(root, "host-panel")?.hidden).toBe(false);
  });
});

describe("the banner", () => {
  test("is empty while connecting and live, and shows the failure while reconnecting", () => {
    const { h, root } = wireApp(null);
    expect([health(root), banner(root)]).toEqual(["pending", ""]);
    h.step(step({ step: "send", frame: WELCOME }));
    settle(h);
    expect([health(root), banner(root)]).toEqual(["live", ""]);
    h.net.current().drop();
    expect(health(root)).toBe("problem");
    expect($(root, "banner-detail")?.textContent).toBe("Connection lost. Reconnecting…");
    expect($(root, "connection-banner")?.querySelector('[data-testid="details"]')?.textContent).toMatch(/^Details Connection lost: /);
    // The retry's Connecting still carries the failure: the banner stays instead of flickering.
    h.clock.advanceTo(h.clock.now() + 1_000);
    expect([state(root), health(root)]).toEqual(["Connecting", "problem"]);
    expect(banner(root)).toContain("Connection lost");
  });

  test("a refused token asks for a new one in the banner", () => {
    const { h, root } = wireApp(null);
    h.step(step({ step: "token_refused", code: 4401 }));
    expect(health(root)).toBe("problem");
    expect($(root, "banner-detail")?.textContent).toContain("The server refused the token.");
    expect($(root, "connection-banner")?.querySelector('input[aria-label="New client token"]')).not.toBeNull();
  });

  test("read-only names the version gap in the banner", () => {
    const { root } = wireApp({ ...WELCOME, protocol: WIRE_PROTOCOL - 1 } as ServerFrame);
    expect(health(root)).toBe("problem");
    expect($(root, "read-only")?.textContent).toBe("This server runs an older version, so sending waits until it is updated.");
    expect($(root, "connection-banner")?.querySelector('[data-testid="details"]')?.textContent).toBe(`Details The server speaks wire protocol ${WIRE_PROTOCOL - 1}; this client speaks ${WIRE_PROTOCOL}.`);
  });

  test("a disconnect that stopped on a failure offers Connect in the banner", () => {
    const { h, root } = wireApp();
    h.net.current().drop();
    ($(root, "host-toggle") as HTMLButtonElement).click();
    expect([state(root), health(root)]).toEqual(["Disconnected", "problem"]);
    $(root, "banner-connect")?.click();
    expect(state(root)).toBe("Connecting");
  });
});

test("the phone shows the same status dot beside the Inbox title", () => {
  const h = new Harness();
  h.step(step({ step: "given", cursors: {}, outbox: [] }));
  const dom = (globalThis as unknown as { happyDOM: { setURL: (url: string) => void } }).happyDOM;
  dom.setURL("https://fixture.invalid/?layout=phone");
  try {
    const root = mount(() => <App config={{ ok: true, config }} env={{ clock: h.clock, sockets: h.net.factory, random: () => 0, store: h.store }} />);
    h.step(step({ step: "open" }));
    h.step(step({ step: "send", frame: WELCOME }));
    settle(h);
    expect($(root, "phone")).not.toBeNull();
    expect($(root, "host-status")?.parentElement?.parentElement?.querySelector("h1")?.textContent).toBe("Inbox");
    expect(health(root)).toBe("live");
  } finally {
    dom.setURL("about:blank");
  }
});

test("healthOf: live, pending while a token is on its way, problem on any failure, off when the person disconnected", () => {
  const cases: [ConnectionState, string | null, boolean, Health][] = [
    ["Live", null, false, "live"],
    ["Connecting", null, false, "pending"],
    ["Hello", null, false, "pending"],
    ["Syncing", null, false, "pending"],
    ["Connecting", "Connection lost: x.", false, "problem"],
    ["Reconnecting", "Connection lost: x.", false, "problem"],
    ["Stale", null, false, "problem"],
    ["ReadOnly", null, false, "problem"],
    ["NeedsToken", null, false, "problem"],
    ["NeedsToken", null, true, "pending"],
    ["NeedsToken", "The token was revoked.", true, "problem"],
    ["Disconnected", null, false, "off"],
    ["Disconnected", null, true, "pending"],
    ["Disconnected", "Connection lost: x.", false, "problem"],
  ];
  expect(cases.map(([state, error, asking]) => healthOf(state, error, asking))).toEqual(cases.map((each) => each[3]));
});
