/**
 * The sidebar's Fleet and Projects trees (ADR-0038 §1), grouped from the fleet
 * store's rows and filtered by the scope toggle.
 *
 * "The client never invents a relation": an agent sits under the node its
 * `node_id` names and the project `(node_id, project)`. An agent with no
 * `node_id` goes in `unbound`; one whose project is absent, or not in the
 * store, goes in its node's `unfiled`. Nothing matches by label or basename.
 *
 * The Projects tree is project → agent. Its worktree level waits for #2798,
 * as the fleet stream carries no worktree entity yet.
 *
 * @module
 */
import type { AgentRow } from "../fleet";
import type { SidebarScope } from "../layout/sidebar";
import type { FleetNodeRow } from "../gen/FleetNodeRow";
import type { FleetProjectRow } from "../gen/FleetProjectRow";

/** One project and the agents that name it. `label` carries its node, for the Projects tree. */
export type ProjectEntry = { readonly key: string; readonly project: FleetProjectRow; readonly label: string; readonly agents: readonly AgentRow[] };

/** One node of the Fleet tree. `node` is null for a `node_id` agents name that has no row. */
export type NodeEntry = { readonly key: string; readonly nodeId: string; readonly node: FleetNodeRow | null; readonly projects: readonly ProjectEntry[]; readonly unfiled: readonly AgentRow[] };

/** The two trees in one scope. */
export type Trees = {
  readonly fleet: { readonly nodes: readonly NodeEntry[]; readonly unbound: readonly AgentRow[] };
  readonly projects: { readonly projects: readonly ProjectEntry[]; readonly unfiled: readonly AgentRow[] };
};

const projectKey = (nodeId: string, id: string): string => JSON.stringify([nodeId, id]);

/**
 * The home node: the one row with no route that is online (ADR-0043: "Null
 * plus online distinguishes local from unavailable"). No such row, or more
 * than one, leaves it unknown (null).
 */
export function homeNodeId(nodes: readonly FleetNodeRow[]): string | null {
  const local = nodes.filter((node) => node.route === null && node.online);
  return local.length === 1 ? local[0]!.node_id : null;
}

/** Whether an agent counts in `scope`. "This node" never guesses: with an unknown home it holds nothing. */
export function inScope(scope: SidebarScope, home: string | null, nodeId: string | undefined): boolean {
  return scope === "all" || (home !== null && nodeId === home);
}

/** Groups `agents`, `nodes` and `projects` into both trees, in the order the server introduced them. */
export function buildTrees(agents: readonly AgentRow[], nodes: readonly FleetNodeRow[], projects: readonly FleetProjectRow[], scope: SidebarScope, home: string | null): Trees {
  const visible = (nodeId: string | undefined): boolean => inScope(scope, home, nodeId);
  const known = new Set(projects.map((project) => projectKey(project.node_id, project.id)));
  const byProject = new Map<string, AgentRow[]>();
  const byNode = new Map<string, AgentRow[]>();
  const unbound: AgentRow[] = [];
  const push = (map: Map<string, AgentRow[]>, key: string, row: AgentRow): void => void map.set(key, [...(map.get(key) ?? []), row]);
  for (const row of agents.filter((each) => visible(each.nodeId))) {
    if (row.nodeId === undefined) unbound.push(row);
    else if (row.project !== undefined && known.has(projectKey(row.nodeId, row.project))) push(byProject, projectKey(row.nodeId, row.project), row);
    else push(byNode, row.nodeId, row);
  }
  const nodeName = (nodeId: string): string => {
    const named = nodes.filter((node) => node.node_id === nodeId);
    // Two nodes may share a name; the id then tells them apart.
    return named.length === 0 ? nodeId : nodes.filter((node) => node.name === named[0]!.name).length > 1 ? `${named[0]!.name} (${nodeId})` : named[0]!.name;
  };
  const entry = (project: FleetProjectRow): ProjectEntry => {
    const key = projectKey(project.node_id, project.id);
    return { key, project, label: `${project.name} · ${nodeName(project.node_id)}`, agents: byProject.get(key) ?? [] };
  };
  const shown = projects.filter((project) => visible(project.node_id)).map(entry);
  const rows = nodes.filter((node) => visible(node.node_id));
  const listed = new Set(rows.map((node) => node.node_id));
  const unlisted = [...byNode.keys(), ...shown.map((each) => each.project.node_id)].filter((id, at, all) => !listed.has(id) && all.indexOf(id) === at);
  const nodeEntry = (nodeId: string, node: FleetNodeRow | null): NodeEntry => ({
    key: `node:${nodeId}`,
    nodeId,
    node,
    projects: shown.filter((each) => each.project.node_id === nodeId),
    unfiled: byNode.get(nodeId) ?? [],
  });
  return {
    fleet: { nodes: [...rows.map((node) => nodeEntry(node.node_id, node)), ...unlisted.map((id) => nodeEntry(id, null))], unbound },
    projects: { projects: shown, unfiled: [...unbound, ...[...byNode.values()].flat()] },
  };
}
