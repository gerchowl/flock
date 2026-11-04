/**
 * The five surfaces that word refusals and connection states (#2902): the
 * composer, the inbox, the transcript, the toolbar's popover and the banner.
 * Each is rendered with every refusal or state, and its visible text, with the
 * Details content left out, must carry no protocol string. The tables alone
 * cannot show this: a surface could still build its line from the raw message.
 */
import { afterEach, describe, expect, test } from "bun:test";
import { createRoot } from "solid-js";
import { render } from "solid-js/web";

import { App } from "../src/App";
import type { ServerFrame } from "../src/gen/ServerFrame";
import type { SubfailCode } from "../src/gen/SubfailCode";
import type { WireRetry } from "../src/gen/WireRetry";
import type { SessionObserver, Transport } from "../src/transport";
import { Transcript } from "../src/views/Transcript";
import { WIRE_PROTOCOL, type ConnectionState } from "../src/wire/connection";
import type { OutboxEntry } from "../src/wire/outbox";
import { Composer, createComposerState, type Sender } from "../src/workbench/Composer";
import type { InboxCard } from "../src/workbench/inbox";
import { InboxCards, createClosedIds } from "../src/workbench/parts";
import { createPermissionInbox } from "../src/workbench/permissions";
import { CONNECTION_COPY, REFUSALS } from "../src/workbench/refusals";
import { Harness, WELCOME, sentFrames, step } from "./wire/harness";

/** What primary copy must never show: snake_case codes, CamelCase state names, ids and wire jargon. */
const PROTOCOL = /\b[a-z]+_[a-z_]+\b|\b[A-Z][a-z]+[A-Z][A-Za-z]*\b|ADR-\d|#\d|\bwire\b|\bprotocol\b|\bsub(fail| id)\b|\back\b/;

const RETRIES: readonly WireRetry[] = ["n/a", "never", "maybe", "yes", "later", "after_inspecting", "after_human", "after_handoff"];
/** Every code with copy, then a code without any under each retry class. */
const ERRORS = [
  ...Object.keys(REFUSALS).map((code) => ({ code, retry: "never" as WireRetry })),
  ...RETRIES.map((retry) => ({ code: "brand_new_code", retry })),
].map(({ code, retry }) => ({ code, message: `command refused: ${code}`, exit: 1, retry }));
const SUBFAILS: readonly SubfailCode[] = ["invalid_agent", "cursor_ahead", "operator_only", "unknown_stream", "scope", "session_busy", "sub_id_reused"];

let disposers: (() => void)[] = [];
afterEach(() => {
  for (const dispose of disposers.splice(0)) dispose();
  document.body.innerHTML = "";
  localStorage.clear();
});
function mount(view: () => ReturnType<typeof App>): HTMLElement {
  const root = document.createElement("div");
  document.body.append(root);
  disposers.push(render(view, root));
  return root;
}

/** The text a person reads without opening Details: every disclosure's body is left out. */
function primary(root: Element): string {
  const copy = root.cloneNode(true) as Element;
  for (const body of copy.querySelectorAll("details > :not(summary)")) body.remove();
  // Text node by text node, so neighbouring elements never run together into one word.
  const walker = document.createTreeWalker(copy, NodeFilter.SHOW_TEXT);
  const texts: string[] = [];
  for (let node = walker.nextNode(); node !== null; node = walker.nextNode()) texts.push(node.textContent ?? "");
  return texts.join(" ");
}
/** The protocol strings in `text`, so a failure names them. */
const leaks = (text: string): string[] => [...text.matchAll(new RegExp(PROTOCOL, "g"))].map((match) => match[0]);

const sender = (commands: readonly OutboxEntry[]): Sender => ({ commands: () => commands, send: () => "c", decide: () => "c", blocked: () => null });
const failed = (index: number, verb: "agent.send" | "agent.decide", args: OutboxEntry["args"], error: OutboxEntry["error"]): OutboxEntry =>
  ({ cmd_id: `c${index}`, verb, args, principal: "p", created_ms: 0, status: "failed", ...(error === undefined ? {} : { error }) }) as OutboxEntry;

describe("the composer", () => {
  test("every refused message reads in plain words, its code only behind Details", () => {
    const entries = ERRORS.map((error, index) => failed(index, "agent.send", { target: "atlas", text: `message ${index}`, submit: true }, error));
    const root = mount(() => <Composer agent="atlas" row={undefined} sender={sender(entries)} state={createComposerState()} />);
    expect(root.querySelectorAll('[data-testid="sent-message"] [data-testid="details"]')).toHaveLength(ERRORS.length);
    expect(leaks(primary(root))).toEqual([]);
  });
});

describe("the inbox", () => {
  test("every refused answer reads in plain words, its code only behind Details", () => {
    const cards: InboxCard[] = ERRORS.map((_, index) => ({ id: `r${index}`, kind: "permission", agent: "atlas", agentLabel: "atlas", title: "Bash", detail: "just check", choices: ["Allow once", "Always allow", "Deny"], blocking: true, requestSeq: 1 }));
    const entries = ERRORS.map((error, index) => failed(index, "agent.decide", { agent: "atlas", request_id: `r${index}`, request_seq: 1, decision: "deny" }, error));
    const root = mount(() => <InboxCards cards={cards} closed={createClosedIds(() => cards)} source="live" sender={sender(entries)} preview={false} />);
    expect(root.querySelectorAll('[data-testid="inbox-card"] [data-testid="details"]')).toHaveLength(ERRORS.length);
    expect(leaks(primary(root))).toEqual([]);
  });

  test("a card whose stream was refused reads in plain words, for every refusal and for none", () => {
    for (const code of [...SUBFAILS, undefined]) {
      let observer: SessionObserver | undefined;
      const transport: Transport = { subscribe: (_stream, _after, seen) => ((observer = seen), { close: () => undefined }) };
      let inbox!: ReturnType<typeof createPermissionInbox>;
      disposers.push(createRoot((dispose) => ((inbox = createPermissionInbox(transport)), dispose)));
      inbox.track([{ id: "atlas", label: "atlas", waitingOnHuman: ["r1"] } as never]);
      observer?.failed({ ...(code === undefined ? {} : { code }), message: `subscription refused: ${code ?? "none"}` });
      const root = mount(() => <InboxCards cards={inbox.cards()} closed={createClosedIds(inbox.cards)} source="live" sender={null} preview={false} />);
      expect(root.querySelector('[data-testid="inbox-card"] [data-testid="details"]')?.textContent).toContain(`subscription refused: ${code ?? "none"}`);
      expect({ code, leaks: leaks(primary(root)) }).toEqual({ code, leaks: [] });
    }
  });
});

describe("the transcript", () => {
  test("every refused history reads in plain words with Retry, its code only behind Details", () => {
    for (const code of [...SUBFAILS, undefined]) {
      let observer: SessionObserver | undefined;
      const transport: Transport = { subscribe: (_stream, _after, seen) => ((observer = seen), { close: () => undefined }) };
      const root = mount(() => <Transcript transport={transport} stream="agent/atlas" source="live" />);
      observer?.failed({ ...(code === undefined ? {} : { code }), message: `subscription refused: ${code ?? "none"}` });
      expect(root.querySelector('[data-testid="retry"]')).not.toBeNull();
      expect({ code, leaks: leaks(primary(root)) }).toEqual({ code, leaks: [] });
    }
  });
});

describe("the toolbar's popover and the banner", () => {
  const config = { transport: "wire", serverUrl: "https://fixture.invalid", token: "secret" } as const;
  const settle = (h: Harness): void => {
    const fleet = ([...sentFrames(h)].reverse().find((frame) => (frame as { stream?: string }).stream === "fleet") as { sub_id: string }).sub_id;
    h.step(step({ step: "send", frame: { t: "gap", sub_id: fleet, from: 1, to: 0, snapshot_to: 0 } }));
    h.step(step({ step: "send", frame: { t: "synced", sub_id: fleet, seq: 0 } }));
  };
  /** How the App reaches each state from a fresh scripted server. Stale moves on to Reconnecting at once, so no person sees it. */
  const reach: { readonly [S in Exclude<ConnectionState, "Stale">]: (h: Harness, root: HTMLElement) => void } = {
    Connecting: () => undefined,
    Hello: (h) => h.step(step({ step: "open" })),
    Syncing: (h) => (h.step(step({ step: "open" })), h.step(step({ step: "send", frame: WELCOME }))),
    Live: (h) => (h.step(step({ step: "open" })), h.step(step({ step: "send", frame: WELCOME })), settle(h)),
    ReadOnly: (h) => (h.step(step({ step: "open" })), h.step(step({ step: "send", frame: { ...WELCOME, protocol: WIRE_PROTOCOL - 1 } as ServerFrame }))),
    Reconnecting: (h) => (reach.Live(h, document.body), h.net.current().drop()),
    NeedsToken: (h) => (h.step(step({ step: "open" })), h.step(step({ step: "token_refused", code: 4401 }))),
    Disconnected: (h, root) => (reach.Live(h, root), (root.querySelector('[data-testid="host-toggle"]') as HTMLButtonElement).click()),
  };
  for (const [state, go] of Object.entries(reach)) {
    test(`${state}: the popover and the banner word it without its name or a diagnosis`, () => {
      const h = new Harness();
      h.step(step({ step: "given", cursors: {}, outbox: [] }));
      const root = mount(() => <App config={{ ok: true, config }} env={{ clock: h.clock, sockets: h.net.factory, random: () => 0, store: h.store }} />);
      go(h, root);
      expect(root.querySelector('[data-testid="host-detail"]')?.textContent).toStartWith(CONNECTION_COPY[state as ConnectionState]);
      const trigger = root.querySelector('[data-testid="host-status"]') as HTMLButtonElement;
      if (trigger.getAttribute("aria-expanded") !== "true") trigger.click();
      const shown = [root.querySelector('[data-testid="host-panel"]'), root.querySelector('[data-testid="connection-banner"]')].map((el) => primary(el as Element)).join(" ");
      expect(leaks(shown)).toEqual([]);
      // "Connecting…" and "Reconnecting…" are the plain words themselves; every other state name must not show.
      if (state !== "Connecting" && state !== "Reconnecting") expect(shown).not.toMatch(new RegExp(`\\b${state}\\b`));
    });
  }
});
