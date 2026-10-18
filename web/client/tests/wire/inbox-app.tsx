/**
 * A mounted live workbench over the scripted fake server, shared by the inbox
 * tests: fleet-lite chooses the streams, `event`/`close` send permission
 * records on `agent/<id>`. With `sender`, answering goes through the real
 * outbox (`createWireSender`).
 */
import { createSignal } from "solid-js";
import { render } from "solid-js/web";

import type { AgentState } from "../../src/gen/AgentState";
import { fixtureExamples } from "../../src/gen/FixtureExamples";
import type { ServerFrame } from "../../src/gen/ServerFrame";
import { emptyLayout } from "../../src/layout/model";
import type { Mode } from "../../src/layout/phone";
import { createWireFleet } from "../../src/wire/fleet";
import { createWireTransport } from "../../src/wire/transport";
import { createWireSender } from "../../src/workbench/sender";
import { Workbench } from "../../src/workbench/Workbench";
import { Harness, live, sentFrames, step } from "./harness";

/** #2244's real open and closed records: the helpers vary them, never hand-build the shape. */
const [OPEN, CLOSED] = fixtureExamples["permission-decision"].flatMap((line) => {
  const ev = line.step === "send" && line.frame?.t === "ev" ? (line.frame.ev as unknown as { type?: string }) : null;
  return ev?.type === "flk_permission_request" ? [ev as Record<string, unknown> & { request: Record<string, unknown> }] : [];
}) as [Record<string, unknown> & { request: Record<string, unknown> }, Record<string, unknown>];

let dispose: (() => void) | undefined;
/** Tears the mounted workbench down; call from `afterEach`. */
export function unmountInbox(): void {
  dispose?.();
  dispose = undefined;
  document.body.innerHTML = "";
  localStorage.clear();
}

const state: AgentState = { turn: "idle", background: "none", attention: "none", waiting: "none", lifecycle: "live", phase: "needs_human", source: {} };
export const $$ = (root: ParentNode, id: string): Element[] => [...root.querySelectorAll(`[data-testid="${id}"]`)];

export type App = { h: Harness; root: HTMLElement; fleet: (id: string, waiting: string[]) => void; event: (agent: string, id: string, tool?: string, input?: object, requestSeq?: number, tsMs?: number) => void;
  close: (agent: string, id: string) => void;
  raw: (agent: string, ev: object) => void;
  gap: (agent: string) => void;
  reset: (agent: string) => void; synced: (agent: string) => void };

export function mountInbox(mode: Mode = "desktop", options: { readonly sender?: boolean } = {}): App {
  const h = new Harness();
  live(h);
  const root = document.createElement("div");
  document.body.append(root);
  const [revision, setRevision] = createSignal(0);
  h.connection.onChange(() => setRevision(revision() + 1));
  const sender = options.sender === true ? createWireSender(h.connection, revision) : null;
  dispose = render(
    () => <Workbench fleet={createWireFleet(h.connection)} transport={createWireTransport(h.connection)} inbox={[]} sender={sender} layout={emptyLayout()} saveLayout={() => undefined} mode={mode} source="live" />,
    root,
  );
  const fleetSub = (sentFrames(h).find((frame) => (frame as { stream?: string }).stream === "fleet") as { sub_id: string }).sub_id;
  const send = (frame: ServerFrame): void => h.step(step({ step: "send", frame }));
  send({ t: "gap", sub_id: fleetSub, from: 1, to: 0, snapshot_to: 0 });
  send({ t: "synced", sub_id: fleetSub, seq: 0 });
  /** Each open record's request_seq, so a close echoes it. */
  const openSeqs = new Map<string, number>();
  let fleetSeq = 0;
  let agentSeq = 0;
  const agentSub = (agent: string): string => (sentFrames(h).filter((f) => (f as { stream?: string }).stream === `agent/${agent}`).at(-1) as { sub_id: string }).sub_id;
  return {
    h,
    root,
    fleet: (id, waiting) => send({ t: "ev", sub_id: fleetSub, stream: "fleet", seq: ++fleetSeq, ts_ms: 0, ev: { type: "fleet.upsert", row: { id, label: id.toUpperCase(), state, waiting_on_human: waiting } } }),
    event: (agent, id, tool = "Bash", input = { command: "just check" }, requestSeq = 7, tsMs = 0) => {
      openSeqs.set(id, requestSeq);
      send({
        t: "ev",
        sub_id: agentSub(agent),
        stream: `agent/${agent}`,
        seq: ++agentSeq,
        ts_ms: tsMs,
        ev: { ...OPEN, request_id: id, request_seq: requestSeq, request: { ...OPEN.request, tool_name: tool, input, tool_use_id: `tool-${id}` } } as never,
      });
    },
    close: (agent, id) => send({ t: "ev", sub_id: agentSub(agent), stream: `agent/${agent}`, seq: ++agentSeq, ts_ms: 0, ev: { ...CLOSED, request_id: id, request_seq: openSeqs.get(id) ?? 0 } as never }),
    raw: (agent, ev) => send({ t: "ev", sub_id: agentSub(agent), stream: `agent/${agent}`, seq: ++agentSeq, ts_ms: 0, ev: ev as never }),
    reset: (agent) => send({ t: "subfail", sub_id: agentSub(agent), code: "cursor_ahead", message: "ahead" }),
    gap: (agent) => {
      send({ t: "gap", sub_id: agentSub(agent), from: agentSeq + 1, to: agentSeq + 2 });
      agentSeq += 2;
    },
    synced: (agent) => send({ t: "synced", sub_id: agentSub(agent), seq: agentSeq }),
  };
}

export const cards = (root: HTMLElement): string[] => $$(root, "inbox-card").map((card) => card.textContent ?? "");
export const frames = (h: Harness, t: string, stream?: string): { sub_id: string }[] =>
  sentFrames(h).filter((f) => (f as { t: string }).t === t && (stream === undefined || (f as { stream?: string }).stream === stream)) as { sub_id: string }[];
