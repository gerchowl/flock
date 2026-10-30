/**
 * The sidebar's trees (ADR-0038 §1): **Fleet** (node → project → agent, with
 * node status badges) and **Projects** (project → agent), each collapsible and
 * both filtered by the scope toggle. An agent row opens its tile.
 *
 * A node row renders ADR-0043's contract: null is "unknown", never zero;
 * counts stay with their row, marked stale once the node is offline; `name`
 * is shown as sent. A node's age is `(ts_ms - last_ok) + (now - receivedAt)`:
 * the home clock's gap at the frame, plus this client's monotonic dwell; the
 * browser's wall clock is never read, and `online` never comes from the age.
 *
 * @module
 */
import { For, Show, type Accessor, type JSX } from "solid-js";

import type { AgentRow } from "../fleet";
import type { FleetNodeRow } from "../gen/FleetNodeRow";
import { nodeAgeMs, type NodeStamp } from "../store/fleet";
import type { SidebarPrefs, SidebarScope } from "../layout/sidebar";
import { AgentRowButton } from "./parts";
import type { NodeEntry, ProjectEntry, Trees } from "./trees";

/** Why "this node" shows nothing: the client does not guess which node is home. */
export const HOME_UNKNOWN = "This node is unknown: no node row is local and online, or several are.";

/** A relative age: "3m ago". A clock behind `last_ok` reads as "just now". */
export function ago(ms: number): string {
  const seconds = Math.max(0, Math.floor(ms / 1000));
  if (seconds < 5) return "just now";
  const [value, unit] = seconds < 60 ? [seconds, "s"] : seconds < 3600 ? [Math.floor(seconds / 60), "m"] : seconds < 86400 ? [Math.floor(seconds / 3600), "h"] : [Math.floor(seconds / 86400), "d"];
  return `${value}${unit} ago`;
}

/** Measurements the node supplied, and the request latency, for a tooltip; nothing is filled in. */
function details(node: FleetNodeRow): string {
  const { cpu_percent, gpu_percent, mem_used, mem_total, disk_free, thermal } = node.health ?? {};
  const parts = [
    node.latency_ms === null ? null : `latency ${node.latency_ms} ms`,
    cpu_percent == null ? null : `cpu ${cpu_percent}%`,
    gpu_percent == null ? null : `gpu ${gpu_percent}%`,
    mem_used == null || mem_total == null ? null : `mem ${mem_used}/${mem_total} B`,
    disk_free == null ? null : `disk free ${disk_free} B`,
    thermal == null ? null : `thermal ${JSON.stringify(thermal)}`,
  ];
  return parts.filter((part) => part !== null).join(" · ");
}

/** A node's compact status: reachability, how stale, its error, counts, version. */
export function NodeStatus(props: { readonly node: FleetNodeRow; readonly stamp: NodeStamp | undefined; readonly now: Accessor<number> }): JSX.Element {
  const seen = (): string => {
    if (props.node.last_ok === null) return "never observed";
    const age = nodeAgeMs(props.stamp, props.node.last_ok, props.now());
    return age === null ? "age unknown" : `seen ${ago(age)}`;
  };
  const counts = (): string => {
    const found = props.node.counts;
    if (found === null) return "agents ?";
    return `${found.total} agents · ${found.waiting_on_human ?? "?"} waiting`;
  };
  return (
    <span class="node-status" data-testid="node-status" data-online={props.node.online} title={details(props.node)}>
      <span class={`node-status__badge node-status__badge--${props.node.online ? "online" : "offline"}`}>{props.node.online ? "online" : "offline"}</span>
      <span data-testid="node-seen">{seen()}</span>
      <Show when={props.node.error}>{(error) => <span class="node-status__error" data-testid="node-error">{error().code}: {error().message}</span>}</Show>
      <span data-testid="node-counts" data-stale={props.node.online ? undefined : "true"}>{counts()}{props.node.online ? "" : " (stale)"}</span>
      <Show when={props.node.latency_ms !== null}>
        <span data-testid="node-latency" data-stale={props.node.online ? undefined : "true"}>{props.node.online ? "" : "last "}{props.node.latency_ms} ms{props.node.online ? "" : " (stale)"}</span>
      </Show>
      <Show when={props.node.version}>{(version) => <span data-testid="node-version">{props.node.online ? "" : "last "}v{version()}</span>}</Show>
    </span>
  );
}

type Collapse = { readonly is: (key: string) => boolean; readonly toggle: (key: string) => void };

/** A collapsible group: its heading is a button, so the keyboard reaches it. */
function Group(props: { readonly id: string; readonly level: "tree" | "node" | "project"; readonly title: JSX.Element; readonly collapse: Collapse; readonly children: JSX.Element }): JSX.Element {
  return (
    <section class={`group group--${props.level}`} data-testid={`group-${props.level}`}>
      <button type="button" class="group__head" aria-expanded={!props.collapse.is(props.id)} onClick={() => props.collapse.toggle(props.id)}>
        <span aria-hidden="true">{props.collapse.is(props.id) ? "▸" : "▾"}</span>
        {props.title}
      </button>
      <Show when={!props.collapse.is(props.id)}>
        <div class="group__body">{props.children}</div>
      </Show>
    </section>
  );
}

type Stamps = (nodeId: string) => NodeStamp | undefined;

type Opener = { readonly focused: Accessor<string | null>; readonly open: (id: string) => void };

/** The Projects tree's rows carry their own test id, so the Fleet tree's rows stay "every agent, once". */
const Agents = (props: { readonly rows: readonly AgentRow[]; readonly testid?: string | undefined } & Opener): JSX.Element => (
  <For each={props.rows}>{(row) => <AgentRowButton row={row} focused={row.id === props.focused()} onFocus={() => props.open(row.id)} testid={props.testid} />}</For>
);

function ProjectGroup(props: { readonly entry: ProjectEntry; readonly title: string; readonly tree: "fleet" | "projects"; readonly collapse: Collapse } & Opener): JSX.Element {
  return (
    <Group id={`${props.tree}-project:${props.entry.key}`} level="project" title={<span title={props.entry.project.id}>{props.title}</span>} collapse={props.collapse}>
      <Agents rows={props.entry.agents} focused={props.focused} open={props.open} testid={props.tree === "projects" ? "project-agent-row" : undefined} />
    </Group>
  );
}

function NodeGroup(props: { readonly entry: NodeEntry; readonly stamp: Stamps; readonly now: Accessor<number>; readonly collapse: Collapse } & Opener): JSX.Element {
  return (
    <Group
      id={props.entry.key}
      level="node"
      collapse={props.collapse}
      title={
        <>
          <span class="group__name">{props.entry.node?.name ?? props.entry.nodeId}</span>
          <Show when={props.entry.node} fallback={<span class="node-status">no status row</span>}>
            {(node) => <NodeStatus node={node()} stamp={props.stamp(node().node_id)} now={props.now} />}
          </Show>
        </>
      }
    >
      <For each={props.entry.projects}>{(entry) => <ProjectGroup entry={entry} title={entry.project.name} tree="fleet" collapse={props.collapse} focused={props.focused} open={props.open} />}</For>
      <Show when={props.entry.unfiled.length > 0}>
        <Group id={`${props.entry.key}:unfiled`} level="project" title="No project" collapse={props.collapse}>
          <Agents rows={props.entry.unfiled} focused={props.focused} open={props.open} />
        </Group>
      </Show>
    </Group>
  );
}

/** The scope toggle, the Fleet tree and the Projects tree. */
export function SidebarTrees(props: {
  readonly trees: Accessor<Trees>;
  readonly home: Accessor<string | null>;
  readonly prefs: Accessor<SidebarPrefs>;
  readonly setPrefs: (prefs: SidebarPrefs) => void;
  readonly stamp: Stamps;
  readonly now: Accessor<number>;
} & Opener): JSX.Element {
  const collapse: Collapse = {
    is: (key) => props.prefs().collapsed.includes(key),
    toggle: (key) => {
      const { collapsed } = props.prefs();
      props.setPrefs({ ...props.prefs(), collapsed: collapsed.includes(key) ? collapsed.filter((each) => each !== key) : [...collapsed, key] });
    },
  };
  const scopeButton = (scope: SidebarScope, label: string): JSX.Element => (
    <button type="button" class="action" aria-pressed={props.prefs().scope === scope} onClick={() => props.setPrefs({ ...props.prefs(), scope })}>
      {label}
    </button>
  );
  return (
    <>
      <div class="scope" role="group" aria-label="Scope">
        {scopeButton("node", "This node")}
        {scopeButton("all", "All nodes")}
      </div>
      <Show when={props.prefs().scope === "node" && props.home() === null}>
        <p class="sidebar__note" data-testid="home-unknown">{HOME_UNKNOWN}</p>
      </Show>
      <Group id="tree:fleet" level="tree" title="Fleet" collapse={collapse}>
        <For each={props.trees().fleet.nodes}>{(entry) => <NodeGroup entry={entry} stamp={props.stamp} now={props.now} collapse={collapse} focused={props.focused} open={props.open} />}</For>
        <Show when={props.trees().fleet.unbound.length > 0}>
          <Group id="group:unbound" level="node" title="Not bound to a node" collapse={collapse}>
            <Agents rows={props.trees().fleet.unbound} focused={props.focused} open={props.open} />
          </Group>
        </Show>
      </Group>
      <Group id="tree:projects" level="tree" title="Projects" collapse={collapse}>
        <For each={props.trees().projects.projects}>{(entry) => <ProjectGroup entry={entry} title={entry.label} tree="projects" collapse={collapse} focused={props.focused} open={props.open} />}</For>
        <Show when={props.trees().projects.unfiled.length > 0}>
          <Group id="group:no-project" level="project" title="No project" collapse={collapse}>
            <Agents rows={props.trees().projects.unfiled} focused={props.focused} open={props.open} testid="project-agent-row" />
          </Group>
        </Show>
      </Group>
    </>
  );
}
