import { defaultLayout, kinds, tileIds, type Layout, type Tree } from "./model";
/** Schema changes reset the entire layout, with a visible notice (ADR-0038 §2). */
export const SCHEMA_VERSION = 1;
function validate(value: unknown): value is Layout {
  if (!value || typeof value !== "object") return false;
  const s = value as Layout;
  if (!s.tiles || typeof s.tiles !== "object" || Array.isArray(s.tiles) || !Array.isArray(s.rail) || !Array.isArray(s.history) || !Number.isSafeInteger(s.next) || s.next < 1) return false;
  const tree = (n: Tree | null, depth = 0): boolean => n === null ? depth === 0 : !!n && depth < 100 && (n.type === "stack"
    ? Array.isArray(n.tiles) && n.tiles.length > 0 && n.tiles.every(id => typeof id === "string") && n.tiles.includes(n.active)
    : n.type === "split" && ["horizontal", "vertical"].includes(n.direction) && Array.isArray(n.weights) && n.weights.length === 2 && n.weights.every(w => Number.isFinite(w) && w > 0) && Array.isArray(n.children) && n.children.length === 2 && n.children.every(c => tree(c, depth + 1)));
  if (!tree(s.tree)) return false;
  const expanded = tileIds(s.tree), ids = [...expanded, ...s.rail];
  if (new Set(ids).size !== ids.length || ids.length !== Object.keys(s.tiles).length) return false;
  if (!(s.focused === null ? expanded.length === 0 : expanded.includes(s.focused)) || !s.history.every(id => expanded.includes(id))) return false;
  return ids.every(id => {
    const t = Object.hasOwn(s.tiles, id) ? s.tiles[id] : undefined;
    return !!t && t.id === id && /^t[1-9]\d*$/.test(id) && Number(id.slice(1)) < s.next && kinds.includes(t.kind) && typeof t.subject === "string" && t.subject.length > 0
      && (expanded.includes(id) ? t.mode === "Expanded" : t.mode === "Rail") && (t.origin === undefined || typeof t.origin === "string")
      && (t.cursor === null || (Number.isSafeInteger(t.cursor) && t.cursor >= 0)) && (t.badge === undefined || typeof t.badge === "string") && Number.isFinite(t.changed);
  });
}
/**
 * Stable identity is injected. G3 #1595 supplies welcome's real identity.
 * Before welcome use a normalized configured URL, without token; fixtures use "fixture".
 * Storage access, JSON and validation errors reset atomically and report a notice.
 */
export function createLayoutStore(options: { serverKey: string; storage?: Pick<Storage, "getItem" | "setItem"> }) {
  const key = `flock.layout:${encodeURIComponent(options.serverKey)}`;
  const storage = () => options.storage ?? globalThis.localStorage;
  return {
    load(): { layout: Layout; notice: string | null } {
      try {
        const raw = storage().getItem(key);
        if (raw === null) return { layout: defaultLayout(), notice: null };
        const parsed: unknown = JSON.parse(raw);
        if (!parsed || typeof parsed !== "object" || !("version" in parsed) || parsed.version !== SCHEMA_VERSION || !("layout" in parsed) || !validate(parsed.layout)) throw new Error("Invalid or obsolete layout");
        return { layout: parsed.layout, notice: null };
      } catch { return { layout: defaultLayout(), notice: "Saved layout could not be loaded; reset to the default layout." }; }
    },
    save(layout: Layout): string | null {
      try { storage().setItem(key, JSON.stringify({ version: SCHEMA_VERSION, layout })); return null; }
      catch { return "Layout could not be saved. Changes last only for this session."; }
    },
  };
}
