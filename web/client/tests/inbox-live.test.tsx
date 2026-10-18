/**
 * The live inbox of open permission prompts (#1749 PR 1): fleet-lite's
 * `waiting_on_human` ids choose the streams, and the server's open/closed
 * permission record on `agent/<id>` (#2224) decides the cards.
 * Scripted fake server, virtual time, no sleeps.
 */
import { afterEach, describe, expect, test } from "bun:test";
import { render } from "solid-js/web";

import type { AgentState } from "../src/gen/AgentState";
import type { ServerFrame } from "../src/gen/ServerFrame";
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

type App = { h: Harness; root: HTMLElement; fleet: (id: string, waiting: string[]) => void; event: (agent: string, id: string, tool?: string, input?: object, requestSeq?: number) => void;
  close: (agent: string, id: string) => void; synced: (agent: string) => void };

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
    event: (agent, id, tool = "Bash", input = { command: "just check" }, requestSeq = 7) =>
      send({
        t: "ev",
        sub_id: agentSub(agent),
        stream: `agent/${agent}`,
        seq: ++agentSeq,
        ts_ms: 0,
        ev: { type: "flk_permission_request", state: "open", request_id: id, request_seq: requestSeq, request: { subtype: "can_use_tool", tool_name: tool, input, tool_use_id: `tool-${id}` } } as never,
      }),
    close: (agent, id) =>
      send({ t: "ev", sub_id: agentSub(agent), stream: `agent/${agent}`, seq: ++agentSeq, ts_ms: 0, ev: { type: "flk_permission_request", state: "closed", request_id: id, decision: "allow" } as never }),
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

  test("an open record shows a card even when fleet-lite does not list its id", () => {
    const { root, fleet, event } = mountInbox();
    fleet("atlas", ["listed"]);
    event("atlas", "unlisted", "Edit", { file_path: "src/b.rs" });
    expect(cards(root).map((text) => text.includes("src/b.rs"))).toEqual([true, false]);
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
    test(`two agents are ordered by first-listed age, then matched event seq (${mode})`, () => {
      const { root, fleet, event } = mountInbox(mode);
      fleet("kite", ["k1"]);
      fleet("atlas", ["a1", "a2"]);
      event("atlas", "a2", "Write", { file_path: "later.rs" });
      event("kite", "k1", "Bash", { command: "kite cmd" });
      event("atlas", "a1", "Read", { file_path: "a-first.rs" });
      const order = cards(root).map((text) => (text.match(/kite cmd|later\.rs|a-first\.rs/) ?? [""])[0]);
      // kite was listed first; atlas's cards follow by event seq (a2's came before a1's).
      expect(order).toEqual(["kite cmd", "later.rs", "a-first.rs"]);
    });
  }

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
