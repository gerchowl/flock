/**
 * The phone workbench (ADR-0038 §2a, "Phone: inbox first"): a bottom tab
 * bar with Inbox (the default), Agents and Activity, and an agent screen
 * pushed over the tabs. There are no splits; back returns to the origin
 * (§2 Phone), through the layout's history.
 *
 * @module
 */
import { For, Match, Show, Switch, createSignal, createUniqueId, onCleanup, type JSX } from "solid-js";

import type { AgentRow } from "../fleet";
import type { Transport } from "../transport";
import { Transcript, type TranscriptSource } from "../views/Transcript";
import type { InboxCard } from "./inbox";
import { AgentRowButton, AgentTitle, InboxCards, Waits } from "./parts";

/** What the phone screens show, and what they can do. */
export type PhoneProps = {
  readonly rows: () => readonly AgentRow[];
  /** Why the agent list is empty or ended, if it is. */
  readonly fleetNote: () => string | null;
  readonly agent: () => string | null;
  readonly row: () => AgentRow | undefined;
  readonly open: (agentId: string) => void;
  readonly back: () => void;
  readonly transport: Transport;
  readonly source: TranscriptSource;
  readonly inbox: readonly InboxCard[];
};

type Tab = "inbox" | "agents";

/** The phone's tabs and agent screen. */
export function PhoneWorkbench(props: PhoneProps): JSX.Element {
  const [tab, setTab] = createSignal<Tab>("inbox");
  const [onAgent, setOnAgent] = createSignal(false);
  const blocked = (): number => props.inbox.filter((card) => card.blocking).length;
  // Live mode has no inbox source until G5: its cards are samples, so the headline never counts them.
  const headline = (): string =>
    props.source === "live" ? "Your inbox arrives with G5 (#1597)." : blocked() === 0 ? "Nothing is blocked on you." : `${blocked()} blocked on you.`;
  /*
   * The agent screen is a history entry, so the system back gesture returns
   * to the tab it came from instead of leaving the app. `pushState` without
   * a URL keeps the current one, `?layout=` and fixture parameters included.
   */
  const AGENT_ENTRY = "flock.phone-agent";
  const close = (): void => {
    if (!onAgent()) return;
    props.back();
    setOnAgent(false);
  };
  const onPop = (): void => close();
  window.addEventListener("popstate", onPop);
  onCleanup(() => window.removeEventListener("popstate", onPop));
  const open = (id: string): void => {
    props.open(id);
    if (!onAgent()) history.pushState({ [AGENT_ENTRY]: id }, "");
    setOnAgent(true);
  };
  /** Leaves the agent screen through its history entry when it has one. */
  const back = (): void => {
    if (onAgent() && typeof history.state === "object" && history.state !== null && AGENT_ENTRY in history.state) history.back();
    else close();
  };
  const tabButton = (id: Tab, label: string): JSX.Element => (
    <button type="button" class="tabbar__tab" aria-current={!onAgent() && tab() === id ? "page" : undefined} onClick={() => (back(), setTab(id))}>
      {label}
    </button>
  );
  return (
    <div class="phone" data-testid="phone">
      <Show
        when={onAgent() && props.agent()}
        keyed
        fallback={
          <main class="phone__screen">
            <Switch>
              <Match when={tab() === "inbox"}>
                <h1 class="phone__title">Inbox</h1>
                <p class="phone__headline" data-testid="inbox-headline">
                  {headline()}
                </p>
                <InboxCards cards={props.inbox} />
              </Match>
              <Match when={tab() === "agents"}>
                <h1 class="phone__title">Agents</h1>
                <Show when={props.fleetNote()}>{(note) => <p class="sidebar__note">{note()}</p>}</Show>
                <For each={props.rows()}>{(row) => <AgentRowButton row={row} focused={false} onFocus={() => open(row.id)} />}</For>
              </Match>
            </Switch>
          </main>
        }
      >
        {(agent) => {
          const [summary, pr, terminal] = [createUniqueId(), createUniqueId(), createUniqueId()];
          return (
          <main class="phone__screen phone__screen--agent" data-testid="phone-agent">
            <header class="phone__agent-header">
              <button type="button" class="action" onClick={back}>
                ‹ Back
              </button>
              <AgentTitle agent={agent} row={props.row()} />
            </header>
            <div class="segmented" role="group" aria-label="Agent view">
              <Waits label="Summary" reasonId={summary} />
              <button type="button" class="action" aria-pressed="true">
                Transcript
              </button>
              <Waits label="PR" reasonId={pr} />
              <Waits label="Terminal" reasonId={terminal} />
            </div>
            <p class="waits__reason" data-testid="segmented-reasons">
              <span id={summary}>Summary waits for goal and step records (ADR-0038 §8).</span> <span id={pr}>PR waits for a PR source.</span>{" "}
              <span id={terminal}>Terminal waits for the terminal tile.</span>
            </p>
            <div class="phone__body">
              <Transcript transport={props.transport} stream={`agent/${agent}`} source={props.source} />
            </div>
          </main>
          );
        }}
      </Show>
      <nav class="tabbar" aria-label="Sections">
        {tabButton("inbox", "Inbox")}
        {tabButton("agents", "Agents")}
        <Waits label="Activity" waitsFor="the events stream (S7-rest)" />
      </nav>
    </div>
  );
}
