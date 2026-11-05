/**
 * The MVP workbench (ADR-0038 §2a, "Mission control"): one toolbar, the
 * sidebar of agents, the focus pane and the inbox.
 *
 * The toolbar (ADR-0048 D7 amendment 5, #2901) has three groups: the
 * connection's status dot and server name, whose popover holds
 * *Disconnect*; the ⌘K search; and the trailing actions. It has no app-name
 * title and names the state once. A banner under it appears only for a
 * problem: a connection that needs the person, or a saved layout that could
 * not be loaded.
 *
 * §2's tile manager (`layout/model.ts`) is the machinery under the focus
 * pane: picking an agent opens its `transcript` tile with the device's
 * default placement, and the layout persists per server. No subject, kind
 * or placement field appears on screen.
 *
 * The keyboard layer (`keys.ts`, #2904): ↑/↓ move through the sidebar's
 * agents in the order it shows them, when focus is in the sidebar or on
 * nothing; `?` opens the shortcut sheet; the palette lists the layer's
 * commands beside the toolbar's.
 *
 * Parts that are not built yet are hidden (§2a as amended by #2900). With
 * the `preview` prop (a development build, `preview.ts`) they show disabled, naming what they wait
 * for, never as buttons that do nothing. There is no *Terminal* action:
 * structured agents replace terminal agents.
 *
 * @module
 */
import { For, Show, createEffect, createMemo, createSignal, onCleanup, type JSX } from "solid-js";

import type { SetBadgeCount } from "../config";
import type { FleetSource } from "../fleet";
import { createManager, type Layout } from "../layout/model";
import { Cluster, Split, SplitShell } from "../layout/primitives";
import { isPhone, type Mode } from "../layout/phone";
import { DEFAULT_SPLIT, isCompact, splitState, type SplitState } from "../layout/splits";
import { DEFAULT_SIDEBAR, type SidebarPrefs } from "../layout/sidebar";
import { fleetStore } from "../store/fleet";
import { SPACE } from "../tokens";
import type { Transport } from "../transport";
import { Transcript, type TranscriptSource } from "../views/Transcript";
import { createBadgeWriter } from "./badge";
import { attentionCards, needsYou } from "./attention";
import type { InboxCard } from "./inbox";
import { Composer, createComposerState, type Sender } from "./Composer";
import { HEALTH_LABELS, HostStatus, type HostView } from "./HostStatus";
import { SHORTCUTS, bindKeys, formatCombo, pageShortcutAvailable, runPageShortcut } from "./keys";
import { Palette, type PaletteCommand, type WaitingAction } from "./Palette";
import { createPermissionInbox } from "./permissions";
import { AgentRowButton, AgentTitle, InboxCards, Waits, createClosedIds } from "./parts";
import { PhoneWorkbench } from "./Phone";
import { ShortcutSheet } from "./ShortcutSheet";
import { HOME_UNKNOWN, NeedsYou, SidebarTrees } from "./Sidebar";
import { buildTrees, homeNodeId, inScope } from "./trees";
import { waitingOnYou } from "./waiting";
import { createWindowActive } from "./window-focus";
import "./workbench.css";

/** The toolbars' unbuilt actions, shown only in preview; each is a button and, dimmed, a palette command. */
const NEW_AGENT: WaitingAction = { label: "New agent", waitsFor: "the harness picker" };
/** Unbuilt toolbar actions, also listed by the preview palette. */
const TOOLBAR_WAITING: readonly WaitingAction[] = [NEW_AGENT];
const PANE_ACTIONS: readonly WaitingAction[] = [
  { label: "Diff", waitsFor: "the diff tile" },
  { label: "Fork", waitsFor: "forks" },
];

/** Where the workbench's data comes from, and how its layout persists. */
export type WorkbenchProps = {
  /** Where the sidebar's agent rows come from. */
  readonly fleet: FleetSource;
  /** Desktop shell badge capability; absent in the browser. */
  readonly setBadgeCount?: SetBadgeCount | undefined;
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
  /** The live connection, for the toolbar's status dot and popover; none in fixture mode. */
  readonly host?: HostView | undefined;
  /** The connection's problem banner, shown under the toolbar (wire mode). */
  readonly banner?: (() => JSX.Element) | undefined;
  /** A problem with the saved layout, if any, shown as a banner. */
  readonly notice?: string | null | undefined;
  /** A recorded session (fixture mode) or a live server (wire mode). */
  readonly source: TranscriptSource;
  /** Phone or desktop; `auto` (the default) follows the viewport width. */
  readonly mode?: Mode;
  /** Shows the unbuilt controls, disabled, with what they wait for  (development builds only, `preview.ts`); hidden by default. */
  readonly preview?: boolean | undefined;
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
  const windowActive = createWindowActive();
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
  const attention = createMemo(() => rows().filter(needsYou));
  const remaining = createMemo(() => rows().filter((row) => !needsYou(row)));
  const waiting = createMemo(() => waitingOnYou(fleet.nodes.items()));
  const attentionLabel = (): string => waiting().text;
  const attentionCommand = (): string => `First agent needing you (${attentionLabel()}${waiting().description === attentionLabel() ? "" : `; ${waiting().description}`})`;
  const badge = createBadgeWriter(props.setBadgeCount);
  createEffect(() => {
    document.title = waiting().title;
    badge(waiting().total, waiting().badgeLabel);
  });
  onCleanup(() => {
    document.title = "flock";
    badge(0);
  });
  const trees = createMemo(() => buildTrees(remaining(), fleet.nodes.items(), fleet.projects.items(), prefs().scope, home()));
  const fleetNote = createMemo((): string | null => {
    const progress = fleet.agents.progress();
    if (progress.phase === "failed") return progress.message;
    if (progress.phase === "loading") return "Loading the agent list…";
    return rows().length === 0 ? "No agents on this server yet." : null;
  });
  // Live, the inbox is the open permission prompts the fleet lists; fixture mode keeps its samples.
  onCleanup(() => permissions?.close());
  const cards = (): readonly InboxCard[] => [...(permissions?.cards() ?? props.inbox), ...attentionCards(rows())];
  const closed = createClosedIds(cards);
  const [layout, setLayout] = createSignal<Layout>(structuredClone(props.layout));
  const manager = createManager(structuredClone(props.layout));
  const publish = (): void => {
    const snapshot = structuredClone(manager.state);
    setLayout(snapshot);
    props.saveLayout(snapshot);
  };
  const regionState = (region: "sidebar" | "inspector"): SplitState => splitState(layout().regions?.[region]);
  const changeRegion = (region: "sidebar" | "inspector", value: SplitState): void => {
    manager.state.regions = { sidebar: DEFAULT_SPLIT, inspector: DEFAULT_SPLIT, ...manager.state.regions, [region]: value };
    publish();
  };
  const toggleRegion = (region: "sidebar" | "inspector"): void => changeRegion(region, { ...regionState(region), collapsed: !regionState(region).collapsed });
  onCleanup(bindKeys(document.body, { "sidebar.toggle": () => toggleRegion("sidebar"), "inspector.toggle": () => toggleRegion("inspector") }));
  const toggleButton = (region: "sidebar" | "inspector"): JSX.Element => <button type="button" class="action" aria-label={`Toggle ${region}`} aria-expanded={!regionState(region).collapsed} aria-controls={`split-${region}`} onClick={() => toggleRegion(region)}>{region === "sidebar" ? "Sidebar" : "Inspector"}</button>;
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
  /**
   * Opens the agent `step` rows from the reference in the sidebar's order: the focused sidebar
   * control, whose focus then follows; else the open agent's first row, counting each agent once
   * (the two trees list it twice). With neither, ↓ opens the first and ↑ the last. It stops at the ends.
   */
  let nav: HTMLElement | undefined;
  let compactNav: HTMLElement | undefined;
  const stepAgent = (step: 1 | -1): void => {
    const navigation = isCompact(width()) ? compactNav : nav;
    if (navigation === undefined) return;
    const all = [...navigation.querySelectorAll<HTMLElement>("[data-agent-id]")];
    const active = document.activeElement;
    const inside = active instanceof HTMLElement && navigation.contains(active);
    const rows = inside ? all : all.filter((row, index) => all.findIndex((other) => other.dataset["agentId"] === row.dataset["agentId"]) === index);
    const from = inside ? active : rows.find((row) => row.dataset["agentId"] === focusedAgent());
    const ahead = (row: HTMLElement): boolean => from === undefined || (from.compareDocumentPosition(row) & (step > 0 ? Node.DOCUMENT_POSITION_FOLLOWING : Node.DOCUMENT_POSITION_PRECEDING)) !== 0;
    const next = step > 0 ? rows.find(ahead) : rows.filter(ahead).at(-1);
    const id = next?.dataset["agentId"];
    if (next === undefined || id === undefined) return;
    openAgent(id);
    if (inside) next.focus();
  };
  const firstAttention = (): void => {
    const first = attention()[0];
    if (first !== undefined) openAgent(first.id);
  };
  const moves = { "attention.first": { run: firstAttention, available: () => attention().length > 0 && waiting().token !== "" }, "agent.previous": () => stepAgent(-1), "agent.next": () => stepAgent(1) };
  onCleanup(bindKeys(document.body, moves));
  /** The layer's commands, each run as its shortcut would be from the page. */
  const keyCommands = createMemo((): PaletteCommand[] => SHORTCUTS.filter((entry) => entry.command).map((entry) => ({ label: entry.id === "attention.first" ? attentionCommand() : entry.label, hint: formatCombo(entry.combo), ...(entry.id === "attention.first" ? { available: () => pageShortcutAvailable(entry.id) } : {}), run: () => void runPageShortcut(entry.id) })));
  const preview = (): boolean => props.preview === true;
  // The server's name: the home node's as the fleet sends it, else the connection's own.
  const hostName = (host: HostView): string => fleet.nodes.items().find((node) => node.node_id === home())?.name ?? host.name;
  const [hostOpen, setHostOpen] = createSignal(false);
  /** The toolbar's connection actions as ⌘K commands: every toolbar action is one (web/DESIGN.md rule 9). */
  const hostCommands = (host: HostView): PaletteCommand[] => [
    { label: `Connection to ${hostName(host)}`, hint: HEALTH_LABELS[host.health()], run: () => setHostOpen(true) },
    host.connected() ? { label: "Disconnect", hint: host.detail(), run: host.disconnect } : { label: "Connect", hint: host.detail(), run: host.connect },
  ];
  const hostStatus = (): JSX.Element => <Show when={props.host}>{(host) => <HostStatus view={host()} name={hostName(host())} open={hostOpen} setOpen={setHostOpen} />}</Show>;
  const banners = (): JSX.Element => (
    <div class="banners">
      <Show when={props.notice}>
        {(notice) => (
          <p class="banner" role="status" data-testid="layout-notice">
            {notice()}
          </p>
        )}
      </Show>
      {props.banner?.()}
    </div>
  );
  const waits = (action: WaitingAction): JSX.Element => <Waits label={action.label} waitsFor={action.waitsFor} />;
  const desktop = (): JSX.Element => (
    <SplitShell active={windowActive()}>
      <Cluster as="header" class="topbar" justify="space-between" space={SPACE[4]}>
        <Cluster space={SPACE[2]}>
          {toggleButton("sidebar")}
          {hostStatus()}
        </Cluster>
        <Palette
          rows={scoped}
          onOpen={openAgent}
          commands={[...(props.host === undefined ? [] : hostCommands(props.host)), ...keyCommands()]}
          waiting={preview() ? [...TOOLBAR_WAITING, ...(focusedAgent() === null ? [] : PANE_ACTIONS)] : []}
        />
        <Cluster space={SPACE[2]}>
          <button type="button" class="action" data-testid="needs-you-count" aria-label={attentionCommand()} disabled={!pageShortcutAvailable("attention.first")} onClick={() => void runPageShortcut("attention.first")}>
            {attentionLabel()}
          </button>
          <Show when={preview()}>
            <For each={TOOLBAR_WAITING}>{waits}</For>
          </Show>
          {toggleButton("inspector")}
        </Cluster>
      </Cluster>
      {banners()}
      <Split region="sidebar" value={regionState("sidebar")} change={value => changeRegion("sidebar", value)} rail={
        <nav class="sidebar" aria-label="Agents" data-testid="compact-rail" ref={element => ((compactNav = element), onCleanup(bindKeys(element, moves)))}>
          <ul class="agent-list"><For each={[...attention(), ...scoped().filter(row => !needsYou(row))]}>{row => <li><AgentRowButton row={row} focused={focusedAgent() === row.id} onFocus={() => openAgent(row.id)} testid="compact-agent-row" /></li>}</For></ul>
        </nav>
      }>
        <nav ref={(element) => ((nav = element), onCleanup(bindKeys(element, moves)))} class="sidebar" aria-label="Agents" data-testid="sidebar">
          <NeedsYou rows={attention()} focused={focusedAgent} open={openAgent} />
          <Show when={fleetNote()}>{(note) => <p class="sidebar__note">{note()}</p>}</Show>
          <Show when={rows().length > 0 || fleet.nodes.items().length > 0 || fleet.projects.items().length > 0}>
            <SidebarTrees trees={trees} home={home} prefs={prefs} setPrefs={setPrefs} stamp={fleet.nodes.stamp} now={now} focused={focusedAgent} open={openAgent} />
          </Show>
        </nav>
      </Split>
      <main class="focus" aria-label="Focused agent" data-testid="focus-pane">
        <Show when={focusedAgent()} keyed fallback={<p class="focus__empty">Pick an agent on the left.</p>}>
          {(agent) => (
            <>
              <header class="focus__header">
                <AgentTitle agent={agent} row={focusedRow()} />
                <Show when={preview()}>
                  <span class="focus__actions">
                    <For each={PANE_ACTIONS}>{(action) => <Waits label={action.label} waitsFor={action.waitsFor} />}</For>
                  </span>
                </Show>
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
      <Split region="inspector" value={regionState("inspector")} change={value => changeRegion("inspector", value)}>
        <aside class="inbox" aria-label="Inbox" data-testid="inbox">
          <h2 class="inbox__title">Inbox</h2>
          <InboxCards cards={cards()} closed={closed} source={props.source} sender={props.sender} preview={preview()} />
        </aside>
      </Split>
      <ShortcutSheet />
    </SplitShell>
  );
  return (
    <Show when={phone()} fallback={desktop()}>
      {banners()}
      <PhoneWorkbench
        host={hostStatus}
        rows={scoped}
        waiting={() => waiting().description}
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
        preview={preview()}
      />
    </Show>
  );
}
