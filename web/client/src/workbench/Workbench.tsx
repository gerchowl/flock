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
import { Show, createEffect, createMemo, createSignal, onCleanup, type JSX } from "solid-js";

import type { FleetSource } from "../fleet";
import { createManager, type Layout } from "../layout/model";
import { isPhone, type Mode } from "../layout/phone";
import { DEFAULT_SIDEBAR, type SidebarPrefs } from "../layout/sidebar";
import { fleetStore } from "../store/fleet";
import type { Transport } from "../transport";
import { Transcript, type TranscriptSource } from "../views/Transcript";
import type { InboxCard } from "./inbox";
import { Composer, createComposerState, type Sender } from "./Composer";
import { Palette } from "./Palette";
import { createPermissionInbox } from "./permissions";
import { AgentTitle, InboxCards, Waits, createClosedIds } from "./parts";
import { PhoneWorkbench } from "./Phone";
import { HOME_UNKNOWN, SidebarTrees } from "./Sidebar";
import { buildTrees, homeNodeId, inScope } from "./trees";
import "./workbench.css";

/** Where the workbench's data comes from, and how its layout persists. */
export type WorkbenchProps = {
  /** Where the sidebar's agent rows come from. */
  readonly fleet: FleetSource;
  readonly transport: Transport;
  readonly inbox: readonly InboxCard[];
  /** Sends `agent.send` through the outbox; `null` in fixture mode. */
  readonly sender: Sender | null;
  /** The saved layout, and where to save changes. */
  readonly layout: Layout;
  readonly saveLayout: (layout: Layout) => void;
  /** The sidebar's saved scope and collapsed groups, and where to save changes; stored beside the layout. */
  readonly sidebar?: SidebarPrefs | undefined;
  readonly saveSidebar?: ((prefs: SidebarPrefs) => void) | undefined;
  /** A monotonic clock in ms that node age is read against; it must be reactive. Defaults to `performance.now()`, re-read every 30 s and when a node row changes. */
  readonly now?: (() => number) | undefined;
  /** A connection status line for the top bar, if any. */
  readonly status?: () => string | null;
  /** A recorded session (fixture mode) or a live server (wire mode). */
  readonly source: TranscriptSource;
  /** Phone or desktop; `auto` (the default) follows the viewport width. */
  readonly mode?: Mode;
};

/** The agent id of a tile subject (`agent/<id>`), or null. */
function agentOf(subject: string | undefined): string | null {
  return subject?.startsWith("agent/") ? subject.slice("agent/".length) : null;
}

/** The workbench: three regions on desktop, inbox-first tabs on a phone. */
export function Workbench(props: WorkbenchProps): JSX.Element {
  const [width, setWidth] = createSignal(window.innerWidth);
  const onResize = (): void => void setWidth(window.innerWidth);
  window.addEventListener("resize", onResize);
  onCleanup(() => window.removeEventListener("resize", onResize));
  const phone = createMemo(() => isPhone(props.mode ?? "auto", width()));
  const composer = createComposerState();
  // The fleet store owns the agents (ADR-0038 §3): the sidebar, palette and permission tracker read its one record.
  const permissions = props.source === "live" ? createPermissionInbox(props.transport) : null;
  // One monotonic clock stamps a node row's receipt (the store) and reads its age (the view).
  const clock = props.now ?? ((): number => performance.now());
  const fleet = fleetStore(props.fleet, clock).read();
  const rows = fleet.agents.items;
  onCleanup(fleet.release);
  createEffect(() => permissions?.track(rows()));
  const [prefs, setPrefsSignal] = createSignal<SidebarPrefs>(props.sidebar ?? DEFAULT_SIDEBAR);
  const setPrefs = (next: SidebarPrefs): void => {
    setPrefsSignal(next);
    props.saveSidebar?.(next);
  };
  const [tick, setTick] = createSignal(0);
  if (props.now === undefined) {
    const timer = setInterval(() => setTick((count) => count + 1), 30_000);
    onCleanup(() => clearInterval(timer));
  }
  // Re-read on a tick and whenever a node row changes, so a row received between ticks is never aged against an older clock reading.
  const now = (): number => (tick(), fleet.nodes.items(), clock());
  const home = createMemo(() => homeNodeId(fleet.nodes.items()));
  // Agents in scope: the summary, the palette and the phone list count these; the trees group them.
  const scoped = createMemo(() => rows().filter((row) => inScope(prefs().scope, home(), row.nodeId)));
  const trees = createMemo(() => buildTrees(rows(), fleet.nodes.items(), fleet.projects.items(), prefs().scope, home()));
  const fleetNote = createMemo((): string | null => {
    const progress = fleet.agents.progress();
    if (progress.phase === "failed") return progress.message;
    if (progress.phase === "loading") return "Loading the agent list…";
    return rows().length === 0 ? "No agents on this server yet." : null;
  });
  // Live, the inbox is the open permission prompts the fleet lists; fixture mode keeps its samples.
  onCleanup(() => permissions?.close());
  const cards = (): readonly InboxCard[] => permissions?.cards() ?? props.inbox;
  const closed = createClosedIds(cards);
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
  /** Focuses `id`'s tile; true when that pushed a tile, which `back` may then pop. */
  const openAgent = (id: string): boolean => {
    if (focusedAgent() === id) return false;
    manager.open(`agent/${id}`, phone() ? "push" : "replace");
    publish();
    return true;
  };
  const back = (): void => {
    manager.back();
    publish();
  };
  const working = createMemo(() => scoped().filter((row) => row.state === "working").length);
  const desktop = (): JSX.Element => (
    <div class="workbench" data-testid="workbench">
      <header class="topbar">
        <span class="topbar__mark">flock</span>
        <Palette rows={scoped} onOpen={openAgent} />
        <span class="topbar__summary" data-testid="agent-summary">
          {scoped().length} agents · {working()} working
        </span>
        <Show when={props.status?.()}>{(status) => <span class="topbar__status" role="status">{status()}</span>}</Show>
        <Waits label="New agent" waitsFor="the harness picker (ADR-0036 S17)" />
      </header>
      <nav class="sidebar" aria-label="Agents" data-testid="sidebar">
        <Show when={fleetNote()}>{(note) => <p class="sidebar__note">{note()}</p>}</Show>
        <Show when={rows().length > 0 || fleet.nodes.items().length > 0 || fleet.projects.items().length > 0}>
          <SidebarTrees trees={trees} home={home} prefs={prefs} setPrefs={setPrefs} stamp={fleet.nodes.stamp} now={now} focused={focusedAgent} open={openAgent} />
        </Show>
      </nav>
      <main class="focus" aria-label="Focused agent" data-testid="focus-pane">
        <Show when={focusedAgent()} keyed fallback={<p class="focus__empty">Pick an agent on the left.</p>}>
          {(agent) => (
            <>
              <header class="focus__header">
                <AgentTitle agent={agent} row={focusedRow()} />
                <span class="focus__actions">
                  <Waits label="Terminal" waitsFor="the terminal tile" />
                  <Waits label="Diff" waitsFor="the diff tile" />
                  <Waits label="Fork" waitsFor="forks (ADR-0036 S12)" />
                </span>
              </header>
              <div class="focus__body">
                <Transcript
                  transport={props.transport}
                  stream={`agent/${agent}`}
                  source={props.source}
                  onEntry={(seq, text) => composer.observe(agent, seq, text)}
                  onCurrent={(current) => composer.current(agent, current)}
                />
              </div>
              <Composer agent={agent} row={focusedRow()} sender={props.sender} state={composer} />
            </>
          )}
        </Show>
      </main>
      <aside class="inbox" aria-label="Inbox" data-testid="inbox">
        <h2 class="inbox__title">Inbox</h2>
        <InboxCards cards={cards()} closed={closed} source={props.source} sender={props.sender} />
      </aside>
    </div>
  );
  return (
    <Show when={phone()} fallback={desktop()}>
      <PhoneWorkbench
        rows={scoped}
        fleetNote={() => fleetNote() ?? (prefs().scope === "node" && home() === null ? HOME_UNKNOWN : null)}
        agent={focusedAgent}
        row={focusedRow}
        open={openAgent}
        back={back}
        widened={() => !phone()}
        transport={props.transport}
        source={props.source}
        inbox={cards()}
        closed={closed}
        sender={props.sender}
        composer={composer}
      />
    </Show>
  );
}
