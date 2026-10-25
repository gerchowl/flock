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
import { Composer, type ComposerState, type Sender } from "./Composer";
import type { InboxCard } from "./inbox";
import { AgentRowButton, AgentTitle, InboxCards, Waits, type ClosedIds } from "./parts";

/** What the phone screens show, and what they can do. */
export type PhoneProps = {
  readonly rows: () => readonly AgentRow[];
  /** Why the agent list is empty or ended, if it is. */
  readonly fleetNote: () => string | null;
  readonly agent: () => string | null;
  readonly row: () => AgentRow | undefined;
  /** Focuses the agent; true when that pushed a layout tile. */
  readonly open: (agentId: string) => boolean;
  readonly back: () => void;
  /** Whether the workbench has left phone mode: read when this screen unmounts. */
  readonly widened: () => boolean;
  readonly transport: Transport;
  readonly source: TranscriptSource;
  readonly inbox: readonly InboxCard[];
  readonly closed: ClosedIds;
  readonly sender: Sender | null;
  readonly composer: ComposerState;
};

type Tab = "inbox" | "agents";

/** The phone's tabs and agent screen. */
export function PhoneWorkbench(props: PhoneProps): JSX.Element {
  const [tab, setTab] = createSignal<Tab>("inbox");
  const [onAgent, setOnAgent] = createSignal(false);
  // The same count as the sidebar badge: what the server lists as waiting on a human.
  const blocked = (): number => props.rows().reduce((sum, row) => sum + row.waitingOnHuman.length, 0);
  const headline = (): string => (blocked() === 0 ? "Nothing is blocked on you." : `${blocked()} blocked on you.`);
  /*
   * The agent screen is a history entry, so the system back gesture returns
   * to the tab it came from instead of leaving the app. `pushState` without
   * a URL keeps the current one, `?layout=` and fixture parameters included.
   */
  const AGENT_ENTRY = "flock.phone-agent";
  const ownEntry = (): boolean => typeof history.state === "object" && history.state !== null && AGENT_ENTRY in history.state;
  // Whether opening the screen pushed a layout tile: closing pops only what this screen pushed.
  let pushedTile = false;
  const close = (): void => {
    if (!onAgent()) return;
    if (pushedTile) props.back();
    pushedTile = false;
    setOnAgent(false);
  };
  const onPop = (): void => close();
  window.addEventListener("popstate", onPop);
  onCleanup(() => {
    window.removeEventListener("popstate", onPop);
    // The window widened past the phone breakpoint with the agent screen open:
    // consume its history entry, so the next back is not a no-op. Any other
    // unmount (the whole app going away) leaves history to the page.
    if (props.widened() && onAgent() && ownEntry()) history.back();
  });
  const open = (id: string): void => {
    pushedTile = props.open(id);
    if (!onAgent()) history.pushState({ [AGENT_ENTRY]: id }, "");
    setOnAgent(true);
  };
  /** Leaves the agent screen through its history entry when it has one. */
  const back = (): void => {
    if (onAgent() && ownEntry()) history.back();
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
                <InboxCards cards={props.inbox} closed={props.closed} source={props.source} sender={props.sender} />
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
              <Transcript
                transport={props.transport}
                stream={`agent/${agent}`}
                source={props.source}
                onEntry={(seq, text) => props.composer.observe(agent, seq, text)}
                onCurrent={(current) => props.composer.current(agent, current)}
              />
            </div>
            <Composer agent={agent} row={props.row()} sender={props.sender} state={props.composer} />
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
