/**
 * The MVP workbench (ADR-0038 §2a, "Mission control"): a top bar, the
 * sidebar of agents, the focus pane and the inbox.
 *
 * §2's tile manager (`layout/model.ts`) is the machinery under the focus
 * pane: picking an agent opens its `transcript` tile with the device's
 * default placement, and the layout persists per server. No subject, kind
 * or placement field appears on screen.
 *
 * Post-MVP parts are shown disabled, naming what they wait for (§2a's
 * table), never as buttons that do nothing.
 *
 * @module
 */
import { For, Show, createMemo, createSignal, createUniqueId, onCleanup, type JSX } from "solid-js";

import type { AgentRow, FleetSource } from "../fleet";
import { createManager, type Layout } from "../layout/model";
import type { Transport } from "../transport";
import { Transcript, type TranscriptSource } from "../views/Transcript";
import { sortInbox, type InboxCard } from "./inbox";
import { PHASE_LABELS, StatusMark, markFor } from "./marks";
import "./workbench.css";

/** Where the workbench's data comes from, and how its layout persists. */
export type WorkbenchProps = {
  /** Where the sidebar's agent rows come from. */
  readonly fleet: FleetSource;
  readonly transport: Transport;
  readonly inbox: readonly InboxCard[];
  /** The saved layout, and where to save changes. */
  readonly layout: Layout;
  readonly saveLayout: (layout: Layout) => void;
  /** A connection status line for the top bar, if any. */
  readonly status?: () => string | null;
  /** A recorded session (fixture mode) or a live server (wire mode). */
  readonly source: TranscriptSource;
};

/** The agent id of a tile subject (`agent/<id>`), or null. */
function agentOf(subject: string | undefined): string | null {
  return subject?.startsWith("agent/") ? subject.slice("agent/".length) : null;
}

/**
 * A post-MVP action, shown but inert. What it waits for is visible text,
 * which the control names through `aria-describedby`, so phones and
 * keyboards reach it too. `reasonId` points at a reason shared with other
 * actions instead of a caption of its own.
 */
export function Waits(props: { readonly label: string } & ({ readonly waitsFor: string } | { readonly reasonId: string })): JSX.Element {
  const id = "reasonId" in props ? props.reasonId : createUniqueId();
  return (
    <span class="waits">
      <button type="button" class="action" aria-disabled="true" aria-describedby={id} onClick={(event) => event.preventDefault()}>
        {props.label}
      </button>
      {"waitsFor" in props && (
        <span id={id} class="waits__reason">
          Waits for {props.waitsFor}
        </span>
      )}
    </span>
  );
}

function AgentRowButton(props: { readonly row: AgentRow; readonly focused: boolean; readonly onFocus: () => void }): JSX.Element {
  return (
    <button type="button" class="agent-row" aria-current={props.focused ? "true" : undefined} onClick={props.onFocus} data-testid="agent-row">
      <StatusMark mark={markFor(props.row)} label={PHASE_LABELS[props.row.state]} count={props.row.waitingOnHuman.length} />
      <span class="agent-row__text">
        <span class="agent-row__name">{props.row.label}</span>
        <span class="agent-row__activity">{props.row.activity}</span>
      </span>
    </button>
  );
}

/** The inbox's cards, blocking first. */
export function InboxCards(props: { readonly cards: readonly InboxCard[] }): JSX.Element {
  return (
    <>
      <p class="inbox__note">{props.cards.length === 0 ? "The live inbox arrives with G5 (#1597)." : "Sample cards: the live inbox arrives with G5 (#1597)."}</p>
      <For each={sortInbox(props.cards)}>
        {(card) => {
          const reason = createUniqueId();
          return (
          <article class="card" classList={{ "card--blocking": card.blocking }} data-testid="inbox-card">
            <p class="card__meta">
              {card.kind === "permission" ? "Permission" : "Question"} · {card.agent}
            </p>
            <h3 class="card__title">{card.title}</h3>
            <p class="card__detail">{card.detail}</p>
            <div class="card__choices">
              <For each={card.choices}>{(choice) => <Waits label={choice} reasonId={reason} />}</For>
              <Waits label="Edit…" reasonId={reason} />
            </div>
            <p id={reason} class="waits__reason">
              Answering waits for the inbox stream (G5 #1597).
            </p>
          </article>
          );
        }}
      </For>
    </>
  );
}

/** The desktop workbench: top bar and three regions. */
export function Workbench(props: WorkbenchProps): JSX.Element {
  const [rows, setRows] = createSignal<readonly AgentRow[]>([]);
  const [fleetNote, setFleetNote] = createSignal<string | null>("Loading the agent list…");
  onCleanup(
    props.fleet.subscribe({
      rows: (next) => {
        setRows(next);
        setFleetNote(next.length === 0 ? "No agents on this server yet." : null);
      },
      failed: (message) => {
        setRows([]);
        setFleetNote(message);
      },
    }),
  );
  const [layout, setLayout] = createSignal<Layout>(structuredClone(props.layout));
  const manager = createManager(structuredClone(props.layout));
  const publish = (): void => {
    const snapshot = structuredClone(manager.state);
    setLayout(snapshot);
    props.saveLayout(snapshot);
  };
  const focusedAgent = createMemo(() => {
    const current = layout();
    return agentOf(current.focused === null ? undefined : current.tiles[current.focused]?.subject);
  });
  const focusedRow = createMemo(() => rows().find((row) => row.id === focusedAgent()));
  const openAgent = (id: string): void => {
    if (focusedAgent() === id) return;
    manager.open(`agent/${id}`);
    publish();
  };
  const working = createMemo(() => rows().filter((row) => row.state === "working").length);
  return (
    <div class="workbench" data-testid="workbench">
      <header class="topbar">
        <span class="topbar__mark">flock</span>
        <span class="topbar__summary" data-testid="agent-summary">
          {rows().length} agents · {working()} working
        </span>
        <Show when={props.status?.()}>{(status) => <span class="topbar__status" role="status">{status()}</span>}</Show>
        <Waits label="New agent" waitsFor="the harness picker (ADR-0036 S17)" />
      </header>
      <nav class="sidebar" aria-label="Agents" data-testid="sidebar">
        <Show when={fleetNote()}>{(note) => <p class="sidebar__note">{note()}</p>}</Show>
        <For each={rows()}>{(row) => <AgentRowButton row={row} focused={row.id === focusedAgent()} onFocus={() => openAgent(row.id)} />}</For>
      </nav>
      <main class="focus" aria-label="Focused agent" data-testid="focus-pane">
        <Show when={focusedAgent()} keyed fallback={<p class="focus__empty">Pick an agent on the left.</p>}>
          {(agent) => (
            <>
              <header class="focus__header">
                <h1 class="focus__name">{focusedRow()?.label ?? agent}</h1>
                <Show when={focusedRow()}>
                  {(row) => (
                    <span class="chip">
                      <StatusMark mark={markFor(row())} label={PHASE_LABELS[row().state]} />
                      {PHASE_LABELS[row().state]}
                    </span>
                  )}
                </Show>
                <span class="focus__actions">
                  <Waits label="Terminal" waitsFor="the terminal tile" />
                  <Waits label="Diff" waitsFor="the diff tile" />
                  <Waits label="Fork" waitsFor="forks (ADR-0036 S12)" />
                </span>
              </header>
              <div class="focus__body">
                <Transcript transport={props.transport} stream={`agent/${agent}`} source={props.source} />
              </div>
            </>
          )}
        </Show>
      </main>
      <aside class="inbox" aria-label="Inbox" data-testid="inbox">
        <h2 class="inbox__title">Inbox</h2>
        <InboxCards cards={props.inbox} />
      </aside>
    </div>
  );
}
