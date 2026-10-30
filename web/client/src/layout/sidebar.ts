/**
 * The sidebar's per-device state (ADR-0038 §1 scope toggle; §2 "per-device
 * persistence"): which scope the trees filter on, and which groups are
 * collapsed.
 *
 * It is stored beside the layout record, under its own key and version, not
 * inside it: the layout's schema change resets the whole tile tree with a
 * notice (`persistence.ts`), and a toggle added later must not cost a person
 * their tiles. An unreadable record falls back to the defaults silently, as a
 * lost preference has nothing to report.
 *
 * @module
 */

/** "This node" or "all nodes" (ADR-0038 §1). */
export type SidebarScope = "node" | "all";

/** What the sidebar remembers. `collapsed` holds the keys of collapsed trees and groups. */
export type SidebarPrefs = { readonly scope: SidebarScope; readonly collapsed: readonly string[] };

/** The defaults: all nodes, everything expanded. */
export const DEFAULT_SIDEBAR: SidebarPrefs = { scope: "all", collapsed: [] };

const SIDEBAR_VERSION = 1;

/** The sidebar record of one server, keyed like the layout (`flock.layout:<serverKey>`). */
export function createSidebarStore(options: { serverKey: string; storage?: Pick<Storage, "getItem" | "setItem"> }) {
  const key = `flock.sidebar:${encodeURIComponent(options.serverKey)}`;
  const storage = () => options.storage ?? globalThis.localStorage;
  return {
    load(): SidebarPrefs {
      try {
        const raw = storage().getItem(key);
        const parsed: unknown = raw === null ? null : JSON.parse(raw);
        const prefs = (parsed as { version?: unknown; prefs?: Partial<SidebarPrefs> } | null);
        if (prefs?.version !== SIDEBAR_VERSION || prefs.prefs === undefined) return DEFAULT_SIDEBAR;
        const { scope, collapsed } = prefs.prefs;
        if ((scope !== "node" && scope !== "all") || !Array.isArray(collapsed) || !collapsed.every((each) => typeof each === "string")) return DEFAULT_SIDEBAR;
        return { scope, collapsed };
      } catch {
        return DEFAULT_SIDEBAR;
      }
    },
    save(prefs: SidebarPrefs): void {
      try {
        storage().setItem(key, JSON.stringify({ version: SIDEBAR_VERSION, prefs }));
      } catch {
        // Storage is unavailable: the choice lasts for this session only.
      }
    },
  };
}
