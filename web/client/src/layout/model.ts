/** Client presentation only (ADR-0038 §2). G3 #1595 maps wire EntityRef onto SubjectRef. */
export type SubjectRef = string;
/** Reserved kinds remain labelled empty views until their slices ship. */
export const kinds = ["transcript", "terminal", "artifact", "queue", "inbox", "goal_tree", "work_graph", "node", "trace", "logs", "audit", "diff", "context"] as const;
/** Catalogue of tile body kinds. */
export type Kind = typeof kinds[number];
/** One independent view, with a G3-supplied cursor and optional live badge. */
export type Tile = { id: string; kind: Kind; subject: SubjectRef; mode: "Expanded" | "Collapsed" | "Rail"; origin?: string; cursor: number | null; badge?: string; changed: number };
/** Binary weighted splits terminate in stacks with exactly one active tab. */
export type Tree = { type: "stack"; tiles: string[]; active: string } | { type: "split"; direction: "horizontal" | "vertical"; weights: [number, number]; children: [Tree, Tree] };
/** Persisted presentation state; history is phone navigation, not session history. */
export type Layout = { tree: Tree | null; tiles: Record<string, Tile>; rail: string[]; focused: string | null; next: number; history: string[] };
/** Explicit open location; beside names an expanded origin. */
export type Placement = "replace" | "tab" | "split_right" | "split_down" | "push" | { beside: string };
/** Callbacks reconcile shared stream ownership; cursors are supplied by G3, never derived here. */
export type Callbacks = { onSubscribe(subject: SubjectRef, cursor: number | null): void; onUnsubscribe(subject: SubjectRef): void; onFilter(subject: SubjectRef | null): void };
const noop: Callbacks = { onSubscribe() {}, onUnsubscribe() {}, onFilter() {} };
const stack = (id: string): Tree => ({ type: "stack", tiles: [id], active: id });
/** The fixture default is one transcript and no rail chips. */
export function defaultLayout(): Layout {
  return { tree: stack("t1"), tiles: { t1: { id: "t1", kind: "transcript", subject: "agent/fixture", mode: "Expanded", cursor: null, changed: 0 } }, rail: [], focused: "t1", next: 2, history: [] };
}
/** All expanded tile IDs, in keyboard traversal order. */
export function tileIds(tree: Tree | null): string[] {
  return tree === null ? [] : tree.type === "stack" ? tree.tiles : tree.children.flatMap(tileIds);
}
function edit(tree: Tree | null, id: string, change: (leaf: Extract<Tree, { type: "stack" }>) => Tree | null): Tree | null {
  if (!tree) return null;
  if (tree.type === "stack") return tree.tiles.includes(id) ? change(tree) : tree;
  const a = edit(tree.children[0], id, change), b = edit(tree.children[1], id, change);
  return a && b ? { ...tree, children: [a, b] } : a ?? b;
}
/**
 * Mutates presentation data only. Callers publish a snapshot after each operation.
 * G3 owns subscriptions for the initial state and supplies cursor/badge updates.
 * Callbacks here report ownership changes, rather than opening real subscriptions.
 */
export function createManager(state: Layout, callbacks: Callbacks = noop, phone = false) {
  const live = (tile: Tile) => tile.mode === "Expanded" || tile.badge !== undefined;
  const owners = (subject: string) => Object.values(state.tiles).filter(t => t.subject === subject && live(t));
  const detach = (id: string) => {
    state.tree = edit(state.tree, id, leaf => {
      const tiles = leaf.tiles.filter(t => t !== id);
      return tiles.length ? { ...leaf, tiles, active: leaf.active === id ? tiles[0]! : leaf.active } : null;
    });
    state.rail = state.rail.filter(t => t !== id);
  };
  const focus = (id: string) => {
    const tile = state.tiles[id];
    if (!tile || tile.mode !== "Expanded") return;
    state.focused = id;
    state.tree = edit(state.tree, id, leaf => ({ ...leaf, active: id }));
    callbacks.onFilter(tile.subject);
  };
  const refocus = () => {
    if (state.focused && state.tiles[state.focused]?.mode === "Expanded") return;
    state.focused = tileIds(state.tree)[0] ?? null;
    if (state.focused) focus(state.focused); else callbacks.onFilter(null);
  };
  const close = (id: string) => {
    const tile = state.tiles[id];
    if (!tile) return;
    const wasLive = live(tile);
    detach(id);
    delete state.tiles[id];
    state.history = state.history.filter(t => t !== id);
    if (wasLive && !owners(tile.subject).length) callbacks.onUnsubscribe(tile.subject);
    refocus();
  };
  const open = (subject: SubjectRef, placement: Placement = phone ? "push" : "replace", kind: Kind = "transcript", origin?: string) => {
    const anchor = typeof placement === "object" ? placement.beside : state.focused;
    if (typeof placement === "object" && !tileIds(state.tree).includes(placement.beside)) throw new Error("Origin is not expanded");
    const shared = owners(subject)[0];
    const id = `t${state.next++}`;
    state.tiles[id] = { id, subject, kind, mode: "Expanded", cursor: shared?.cursor ?? null, changed: Date.now(), ...(origin ? { origin } : {}) };
    if (!shared) callbacks.onSubscribe(subject, null);
    if (placement === "push" && anchor) state.history.push(anchor);
    if (!state.tree || !anchor) state.tree = stack(id);
    else state.tree = edit(state.tree, anchor, leaf => {
      if (placement === "tab" || placement === "push") return { ...leaf, tiles: [...leaf.tiles, id], active: id };
      if (placement === "replace") return { ...leaf, tiles: leaf.tiles.map(t => t === anchor ? id : t), active: id };
      return { type: "split", direction: placement === "split_down" ? "vertical" : "horizontal", weights: [1, 1], children: [leaf, stack(id)] };
    });
    focus(id);
    if (placement === "replace" && anchor) close(anchor);
    return id;
  };
  return { state, open, close, focus,
    expandIntoSecond: (subject: SubjectRef, origin: string, kind: Kind = "artifact") => {
      if (!tileIds(state.tree).includes(origin)) throw new Error("Origin is not expanded");
      focus(origin);
      return open(subject, phone ? "push" : { beside: origin }, kind, origin);
    },
    collapse: (id: string) => {
      const tile = state.tiles[id];
      if (!tile || tile.mode !== "Expanded") return;
      detach(id); tile.mode = "Rail"; state.rail.push(id);
      if (!owners(tile.subject).length) callbacks.onUnsubscribe(tile.subject);
      refocus();
    },
    restore: (id: string) => {
      const tile = state.tiles[id];
      if (!tile || tile.mode === "Expanded") return;
      const subscribed = owners(tile.subject).length > 0;
      detach(id); tile.mode = "Expanded";
      state.tree = state.tree && state.focused ? edit(state.tree, state.focused, leaf => ({ ...leaf, tiles: [...leaf.tiles, id], active: id })) : stack(id);
      if (!subscribed) callbacks.onSubscribe(tile.subject, tile.cursor);
      focus(id);
    },
    back: () => { let id; while ((id = state.history.pop())) { if (state.tiles[id]?.mode === "Expanded") { focus(id); return; } } },
  };
}
