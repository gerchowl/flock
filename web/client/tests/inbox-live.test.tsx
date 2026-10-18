/**
 * The live inbox of open permission prompts (#1749 PR 1): fleet-lite's
 * `waiting_on_human` ids choose the streams, and the server's open/closed
 * permission record on `agent/<id>` (#2224) decides the cards.
 * Scripted fake server, virtual time, no sleeps.
 */
import { afterEach, describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { render } from "solid-js/web";

import type { AgentState } from "../src/gen/AgentState";
import type { ServerFrame } from "../src/gen/ServerFrame";
import { SUMMARY_MAX, TITLE_MAX } from "../src/views/turns";
import { emptyLayout } from "../src/layout/model";
import type { Mode } from "../src/layout/phone";
import { createWireFleet } from "../src/wire/fleet";
import { createWireTransport } from "../src/wire/transport";
import { Workbench } from "../src/workbench/Workbench";
import { Harness, live, sentFrames, step } from "./wire/harness";

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  dispose = undefined;
  document.body.innerHTML = "";
  localStorage.clear();
});

const state: AgentState = { turn: "idle", background: "none", attention: "none", waiting: "none", lifecycle: "live", phase: "needs_human", source: {} };
const $$ = (root: ParentNode, id: string): Element[] => [...root.querySelectorAll(`[data-testid="${id}"]`)];

type App = { h: Harness; root: HTMLElement; fleet: (id: string, waiting: string[]) => void; event: (agent: string, id: string, tool?: string, input?: object, requestSeq?: number, tsMs?: number) => void;
  close: (agent: string, id: string) => void;
  raw: (agent: string, ev: object) => void;
  gap: (agent: string) => void;
  reset: (agent: string) => void; synced: (agent: string) => void };

function mountInbox(mode: Mode = "desktop"): App {
  const h = new Harness();
  live(h);
  const root = document.createElement("div");
  document.body.append(root);
  dispose = render(
    () => <Workbench fleet={createWireFleet(h.connection)} transport={createWireTransport(h.connection)} inbox={[]} sender={null} layout={emptyLayout()} saveLayout={() => undefined} mode={mode} source="live" />,
    root,
  );
  const fleetSub = (sentFrames(h).find((frame) => (frame as { stream?: string }).stream === "fleet") as { sub_id: string }).sub_id;
  const send = (frame: ServerFrame): void => h.step(step({ step: "send", frame }));
  send({ t: "gap", sub_id: fleetSub, from: 1, to: 0, snapshot_to: 0 });
  send({ t: "synced", sub_id: fleetSub, seq: 0 });
  let fleetSeq = 0;
  let agentSeq = 0;
  const agentSub = (agent: string): string => (sentFrames(h).filter((f) => (f as { stream?: string }).stream === `agent/${agent}`).at(-1) as { sub_id: string }).sub_id;
  return {
    h,
    root,
    fleet: (id, waiting) => send({ t: "ev", sub_id: fleetSub, stream: "fleet", seq: ++fleetSeq, ts_ms: 0, ev: { type: "fleet.upsert", row: { id, label: id.toUpperCase(), state, waiting_on_human: waiting } } }),
    event: (agent, id, tool = "Bash", input = { command: "just check" }, requestSeq = 7, tsMs = 0) =>
      send({
        t: "ev",
        sub_id: agentSub(agent),
        stream: `agent/${agent}`,
        seq: ++agentSeq,
        ts_ms: tsMs,
        ev: { type: "flk_permission_request", state: "open", request_id: id, request_seq: requestSeq, request: { subtype: "can_use_tool", tool_name: tool, input, tool_use_id: `tool-${id}` } } as never,
      }),
    close: (agent, id) =>
      send({ t: "ev", sub_id: agentSub(agent), stream: `agent/${agent}`, seq: ++agentSeq, ts_ms: 0, ev: { type: "flk_permission_request", state: "closed", request_id: id, decision: "allow_once", message: null, principal: "p", source: "token" } as never }),
    raw: (agent, ev) => send({ t: "ev", sub_id: agentSub(agent), stream: `agent/${agent}`, seq: ++agentSeq, ts_ms: 0, ev: ev as never }),
    reset: (agent) => send({ t: "subfail", sub_id: agentSub(agent), code: "cursor_ahead", message: "ahead" }),
    gap: (agent) => {
      send({ t: "gap", sub_id: agentSub(agent), from: agentSeq + 1, to: agentSeq + 2 });
      agentSeq += 2;
    },
    synced: (agent) => send({ t: "synced", sub_id: agentSub(agent), seq: agentSeq }),
  };
}

const cards = (root: HTMLElement): string[] => $$(root, "inbox-card").map((card) => card.textContent ?? "");
const frames = (h: Harness, t: string, stream?: string): { sub_id: string }[] =>
  sentFrames(h).filter((f) => (f as { t: string }).t === t && (stream === undefined || (f as { stream?: string }).stream === stream)) as { sub_id: string }[];

describe("live permission inbox", () => {
  test("an open record shows a card with its tool, input and agent; the closed record removes it", () => {
    const { h, root, fleet, event, close } = mountInbox();
    fleet("atlas", ["request-one"]);
    event("atlas", "request-one");
    expect(cards(root)).toHaveLength(1);
    const text = cards(root)[0] ?? "";
    expect(text).toContain("ATLAS");
    expect(text).toContain("Bash");
    expect(text).toContain("just check");
    expect(frames(h, "sub", "agent/atlas")).toHaveLength(1);
    close("atlas", "request-one");
    expect(cards(root)).toEqual([]);
    // The fleet may still list the id for a moment; the closed record wins, so no pending card returns.
    fleet("atlas", ["request-one"]);
    expect(cards(root)).toEqual([]);
  });

  test("request_seq is carried verbatim on the card model", () => {
    const seen: (number | undefined)[] = [];
    const { root, fleet, event } = mountInbox();
    fleet("atlas", ["r1", "r2"]);
    event("atlas", "r1", "Bash", {}, 41);
    event("atlas", "r2", "Bash", {}, 3);
    for (const card of $$(root, "inbox-card")) seen.push(Number(card.getAttribute("data-request-seq")));
    expect(seen).toEqual([41, 3]);
  });

  test("a closed record for an unknown id is ignored without crashing", () => {
    const { root, fleet, event, close } = mountInbox();
    fleet("atlas", ["request-one"]);
    close("atlas", "never-opened");
    expect(cards(root)).toHaveLength(1);
    expect(cards(root)[0]).toContain("Loading details…");
    event("atlas", "request-one");
    expect(cards(root)[0]).toContain("just check");
  });

  test("an open record for an unlisted id, an automatic request, shows no card", () => {
    const { root, fleet, event } = mountInbox();
    fleet("atlas", ["listed"]);
    event("atlas", "automatic", "Edit", { file_path: "src/b.rs" });
    expect(cards(root)).toHaveLength(1);
    expect(cards(root)[0]).not.toContain("src/b.rs");
  });

  test("an unlisted id with an open record shows a card once it is listed, an automatic request turned manual", () => {
    const { root, fleet, event } = mountInbox();
    fleet("atlas", ["listed"]);
    event("atlas", "automatic", "Edit", { file_path: "src/b.rs" });
    fleet("atlas", ["listed", "automatic"]);
    expect(cards(root).map((text) => text.includes("src/b.rs"))).toEqual([true, false]);
  });

  test("a card leaves when the id drops out of the fleet list, without a closed record", () => {
    const { root, fleet, event } = mountInbox();
    fleet("atlas", ["r1", "r2"]);
    event("atlas", "r1");
    fleet("atlas", ["r2"]);
    expect(cards(root)).toHaveLength(1);
    expect(cards(root)[0]).toContain("permission r2");
  });

  test("a listed id without its record is a visible pending card; once synced it says there is none", () => {
    const { root, fleet, synced } = mountInbox();
    fleet("atlas", ["request-one"]);
    expect(cards(root)[0]).toContain("ATLAS · permission request-one");
    expect(cards(root)[0]).toContain("Loading details…");
    synced("atlas");
    expect(cards(root)).toHaveLength(1);
    expect(cards(root)[0]).toContain("no open permission record");
  });

  for (const mode of ["desktop", "phone"] as const) {
    test(`cards sort by each request's own server age, not the agent's (${mode})`, () => {
      const { root, fleet, event } = mountInbox(mode);
      fleet("kite", ["k1"]);
      fleet("atlas", ["a1", "a2"]);
      event("atlas", "a2", "Write", { file_path: "oldest.rs" }, 1, 10);
      event("kite", "k1", "Bash", { command: "newest cmd" }, 2, 30);
      event("atlas", "a1", "Read", { file_path: "middle.rs" }, 3, 20);
      const order = cards(root).map((text) => (text.match(/oldest\.rs|middle\.rs|newest cmd/) ?? [""])[0]);
      // kite's agent was listed first, but atlas's a2 request is older on the wire.
      expect(order).toEqual(["oldest.rs", "middle.rs", "newest cmd"]);
    });
  }

  test("equal server times fall back to the agent's age, then the stream seq", () => {
    const { root, fleet, event } = mountInbox();
    fleet("kite", ["k1"]);
    fleet("atlas", ["a1", "a2"]);
    event("atlas", "a2", "Write", { file_path: "second.rs" }, 1, 5);
    event("atlas", "a1", "Read", { file_path: "third.rs" }, 2, 5);
    event("kite", "k1", "Bash", { command: "first cmd" }, 3, 5);
    expect(cards(root).map((text) => (text.match(/second\.rs|third\.rs|first cmd/) ?? [""])[0])).toEqual(["first cmd", "second.rs", "third.rs"]);
  });

  test("the phone headline counts waiting_on_human, like the badge, even after a closed record", () => {
    const { root, fleet, event, close } = mountInbox("phone");
    fleet("atlas", ["r1", "r2"]);
    event("atlas", "r1");
    close("atlas", "r1");
    expect(cards(root)).toHaveLength(1);
    expect(root.querySelector('[data-testid="inbox-headline"]')?.textContent).toBe("2 blocked on you.");
  });

  test("a record that does not match the decided shape fails visibly, naming what is wrong", () => {
    const { root, fleet, raw } = mountInbox();
    fleet("atlas", ["r1"]);
    // Today's server: an open record without request_seq (before #1941).
    raw("atlas", { type: "flk_permission_request", state: "open", request_id: "r1", request: { tool_name: "Bash", input: {}, tool_use_id: "t", subtype: "can_use_tool" } });
    expect(cards(root)).toHaveLength(1);
    expect(cards(root)[0]).toContain("permission r1");
    expect(cards(root)[0]).toContain("Unusable permission record: request_seq is missing or not an integer");
    expect(cards(root)[0]).not.toContain("Loading details");
  });

  test("an unlisted or unreadable bad record gets a notice card, and a good record for a listed id replaces the error", () => {
    const { root, fleet, event, raw } = mountInbox();
    fleet("atlas", ["r1"]);
    raw("atlas", { type: "flk_permission_request", state: "open", request_id: "auto-1" });
    raw("atlas", { type: "flk_permission_request", state: "open" });
    expect(cards(root).length).toBe(3);
    expect(cards(root)[1]).toContain("permission auto-1 (not awaiting you)");
    expect(cards(root)[2]).toContain("unreadable permission record");
    expect(cards(root)[2]).toContain("request_id is missing");
    raw("atlas", { type: "flk_permission_request", state: "open", request_id: "r1" });
    expect(cards(root)[0]).toContain("Unusable permission record");
    event("atlas", "r1");
    expect(cards(root)[0]).toContain("just check");
  });

  test("an invalid closed record fails visibly; a closed record with a typed reason closes", () => {
    const { root, fleet, event, raw, close } = mountInbox();
    fleet("atlas", ["r1", "r2"]);
    event("atlas", "r1");
    event("atlas", "r2");
    raw("atlas", { type: "flk_permission_request", state: "closed", request_id: "r1", decision: "maybe", principal: "p", source: "token" });
    expect(cards(root)).toHaveLength(2);
    expect(cards(root)[0]).toContain('decision "maybe" is not one of allow_once, allow_always, deny');
    raw("atlas", { type: "flk_permission_request", state: "closed", request_id: "r2", reason: "ended" });
    expect(cards(root)).toHaveLength(1);
    raw("atlas", { type: "flk_permission_request", state: "closed", request_id: "r1", reason: "bogus" });
    expect(cards(root)[0]).toContain("reason \"bogus\" is not one of ended, cancelled, result, interrupted");
    close("atlas", "r1");
    expect(cards(root)).toEqual([]);
  });

  test("a tool with no known input key shows a typed placeholder, and a long input is bounded", () => {
    const { root, fleet, event } = mountInbox();
    fleet("atlas", ["r1", "r2"]);
    event("atlas", "r1", "Mystery", { n: 1 }, 1, 1);
    event("atlas", "r2", "Bash", { command: "x".repeat(SUMMARY_MAX * 3) }, 2, 2);
    expect(cards(root)[0]).toContain("(no summary for Mystery)");
    const detail = root.querySelectorAll('[data-testid="inbox-card"] .card__detail')[1]?.textContent ?? "";
    expect(detail).toBe(`${"x".repeat(SUMMARY_MAX - 1)}…`);
  });

  test("session_ended, the reason #2236 decided, closes a request", () => {
    const { root, fleet, event, raw } = mountInbox();
    fleet("atlas", ["r1"]);
    event("atlas", "r1");
    raw("atlas", { type: "flk_permission_request", state: "closed", request_id: "r1", reason: "session_ended" });
    expect(cards(root)).toEqual([]);
  });

  test("the stale style is its own rule, so no other card text is styled stale", () => {
    const css = readFileSync(new URL("../src/workbench/workbench.css", import.meta.url), "utf8");
    const rule = /([^{}]*)\.card__stale([^{}]*)\{/.exec(css);
    expect(rule?.[1]?.trim()).toBe("");
    expect(rule?.[2]?.trim()).toBe("");
    expect(/\.card__meta\s*,\s*\.card__stale|\.card__stale\s*,/.test(css)).toBe(false);
  });

  test("synced and reset both clear stale, and the next entry clears it when no synced comes", () => {
    const { root, fleet, event, gap, synced, reset } = mountInbox();
    fleet("atlas", ["r1"]);
    event("atlas", "r1");
    gap("atlas");
    synced("atlas");
    expect($$(root, "inbox-stale")).toHaveLength(0);
    gap("atlas");
    expect($$(root, "inbox-stale")).toHaveLength(1);
    reset("atlas");
    expect($$(root, "inbox-stale")).toHaveLength(0);
    // After the first synced the transport passes on no other, so the replay's entry is the signal.
    event("atlas", "r1");
    gap("atlas");
    expect($$(root, "inbox-stale")).toHaveLength(1);
    event("atlas", "r1");
    expect($$(root, "inbox-stale")).toHaveLength(0);
  });

  test("a notice for an unlisted id's bad record expires on the next synced or reset", () => {
    const { root, fleet, raw, synced, reset } = mountInbox();
    fleet("atlas", ["r1"]);
    raw("atlas", { type: "flk_permission_request", state: "open", request_id: "auto-1" });
    expect(cards(root)).toHaveLength(2);
    synced("atlas");
    expect(cards(root)).toHaveLength(1);
    raw("atlas", { type: "flk_permission_request", state: "open" });
    expect(cards(root)).toHaveLength(2);
    reset("atlas");
    expect(cards(root)).toHaveLength(1);
  });

  test("a code point is never split at the summary bound, and a long or non-string title is bounded", () => {
    const { root, fleet, event, raw } = mountInbox();
    fleet("atlas", ["r1", "r2"]);
    event("atlas", "r1", "Bash", { command: `${"x".repeat(SUMMARY_MAX - 2)}😀${"y".repeat(10)}` }, 1, 1);
    expect(root.querySelectorAll('[data-testid="inbox-card"] .card__detail')[0]?.textContent).toBe(`${"x".repeat(SUMMARY_MAX - 2)}😀…`);
    raw("atlas", { type: "flk_permission_request", state: "open", request_id: "r2", request_seq: 2, request: { subtype: "can_use_tool", tool_name: "Edit", display_name: 42, input: {}, tool_use_id: "t" } });
    expect(root.querySelectorAll('[data-testid="inbox-card"] .card__title')[0]?.textContent).toBe("Edit");
    raw("atlas", { type: "flk_permission_request", state: "open", request_id: "r2", request_seq: 2, request: { subtype: "can_use_tool", tool_name: "Edit", display_name: "T".repeat(TITLE_MAX * 2), input: {}, tool_use_id: "t" } });
    expect(root.querySelectorAll('[data-testid="inbox-card"] .card__title')[0]?.textContent).toBe(`${"T".repeat(TITLE_MAX - 1)}…`);
    event("atlas", "r2", "M".repeat(500), { n: 1 }, 3, 3);
    expect((root.querySelectorAll('[data-testid="inbox-card"] .card__detail')[1]?.textContent ?? "").length).toBeLessThanOrEqual(TITLE_MAX);
  });

  test("a history gap marks the agent's cards stale until its next entry", () => {
    const { root, fleet, event, gap } = mountInbox();
    fleet("atlas", ["r1"]);
    event("atlas", "r1");
    expect($$(root, "inbox-stale")).toHaveLength(0);
    gap("atlas");
    expect($$(root, "inbox-stale")).toHaveLength(1);
    event("atlas", "r1");
    expect($$(root, "inbox-stale")).toHaveLength(0);
  });

  test("the agent subscription opens with the first id and closes when the list empties", () => {
    const { h, fleet } = mountInbox();
    fleet("atlas", ["r1"]);
    fleet("atlas", ["r1", "r2"]);
    expect(frames(h, "sub", "agent/atlas")).toHaveLength(1);
    const sub = frames(h, "sub", "agent/atlas")[0]?.sub_id ?? "";
    expect(frames(h, "unsub")).toHaveLength(0);
    fleet("atlas", []);
    expect(frames(h, "unsub").map((f) => f.sub_id)).toEqual([sub]);
  });

  test("a focused transcript tile keeps its own subscription when the inbox's closes", () => {
    const { h, root, fleet } = mountInbox();
    fleet("atlas", ["r1"]);
    (root.querySelector('[data-testid="agent-row"]') as HTMLButtonElement).click();
    const subs = frames(h, "sub", "agent/atlas").map((f) => f.sub_id);
    expect(subs).toHaveLength(2);
    fleet("atlas", []);
    const unsubbed = frames(h, "unsub").map((f) => f.sub_id);
    expect(unsubbed).toHaveLength(1);
    expect(subs).toContain(unsubbed[0] ?? "");
    expect(unsubbed[0]).toBe(subs[0]);
  });

  test("the decide actions are disabled with their reason, and nothing sends a command", () => {
    const { h, root, fleet, event } = mountInbox();
    fleet("atlas", ["r1"]);
    event("atlas", "r1");
    const buttons = [...(root.querySelector('[data-testid="inbox-card"]')?.querySelectorAll("button") ?? [])];
    expect(buttons.map((b) => b.textContent)).toEqual(expect.arrayContaining(["Allow once", "Always allow", "Deny"]));
    for (const button of buttons) {
      expect(button.getAttribute("aria-disabled")).toBe("true");
      expect(document.getElementById(button.getAttribute("aria-describedby") ?? "")?.textContent).toBe("Answering waits for #1749 PR 2 (answering prompts).");
      button.click();
    }
    expect(frames(h, "cmd")).toHaveLength(0);
  });
});
