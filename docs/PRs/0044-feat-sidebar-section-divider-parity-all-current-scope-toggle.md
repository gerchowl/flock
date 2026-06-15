---
number: 44
title: "feat(sidebar): section divider parity + all/current scope toggles (#41)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-11T08:38:44Z
closed: 2026-06-11T09:19:41Z
merged: 2026-06-11T09:19:41Z
base: feat/sidebar-row-gap
head: sidebar-sections
url: https://github.com/gerchowl/herdr/pull/44
---

# feat(sidebar): section divider parity + all/current scope toggles (#41)

Implements #41: consistent chrome and scoping controls across the sidebar's three sections (servers / spaces / agents).

## What

**1. Divider parity** — the servers band now ends in a hairline `─` row in `divider_color()`, so the servers↔spaces boundary speaks the same visual language (divider → header → gap → body) as the existing spaces↔agents divider. Only present when the band itself renders.

**2. servers all/current toggle** — replaces the whole-header collapse click with the agents-style right-aligned `all`/`current` label in the header. Scope `current` renders only the local server row, **plus the pinned home row whenever a fleet snapshot origin exists** — the way home never hides (slot order puts home first, so even band clipping can't strand it). `servers_collapsed` is removed; it was never persisted in the session snapshot, so old session files restore unaffected.

**3. spaces all/current toggle** — same header idiom. Scope `current` pins the workspace list to the focused workspace's space group (`collapsible_space_keys` grouping; a focused ungrouped workspace renders alone). The filter lives in `workspace_list_entries` — the single source of the rendered list — so hit-areas, scroll, and keyboard selection (`visible_workspace_order`) clamp to what's on screen. Orthogonal to per-group collapse / `auto_collapse_groups`: those still fold members *within* the rendered group. Remote rows spliced into the focused project keep rendering; remote-only trailing projects hide.

## Plumbing

- `PanelScopeConfig` (generalized from `AgentPanelScopeConfig`), `ui.servers_panel_scope` / `ui.spaces_panel_scope`, both default `"all"` → zero behavior change unconfigured. Documented in the config template + website configuration docs.
- Toggles persist like the agents one: config write on change + additive serde-default `SessionSnapshot` fields (`servers_panel_scope`, `spaces_panel_scope`) — pre-#41 snapshots restore to `All`; capture now takes the scopes bundled as `PanelScopes` (kept `capture` under the clippy arg cap).
- Mobile untouched: mobile mouse routing short-circuits before the desktop sidebar branch and the mobile switcher does not consume `workspace_list_entries`.

## Tests (1959 passing, fmt + clippy --all-targets clean)

- header toggle click flips scope + persists to snapshot (servers, spaces — mirrors the agents toggle test); header click outside the label is a no-op
- servers `current` keeps the home row + hit-area when a snapshot is present
- spaces `current`: only the focused group renders; ungrouped-focused renders alone; collapse stays orthogonal; focused project's remote rows kept, remote-only projects hidden; keyboard selection clamps to visible entries
- old snapshot without the new fields restores to defaults; capture round-trips both scopes
- render test: divider row between servers band and ` spaces` header; scope labels render right-aligned

## Notes / deviations

- `servers_collapsed` had no snapshot field, so there is nothing to migrate — removal is clean (verified against `persist/`).
- The servers divider counts toward the band's half-section cap, so on very tight sidebars one fewer peer row fits before the cap than pre-#41.
- Any config reload now also resets `workspace_scroll` (mirrors the existing `agent_panel_scroll` reset in the same block).

## Scope additions (issue comments, second commit)

**4. Whole-header click** — the entire servers/spaces/agents header row (title word included) is the all/current toggle hit-area; the right-aligned label keeps working. The agents toggle is unified onto the same idiom.

**5. menu → bottom** — the `menu` entry leaves the spaces footer and becomes a standalone row pinned to the sidebar's last row, with its own hairline `─` divider above (`divider_color()`): servers ─ spaces ─ agents ─ … ─ menu. The whole row is the launcher hit-area; the sections split the rows above the 2-row band (`SIDEBAR_MENU_BAND_ROWS`), and the section-split drag math follows. At `pane_gap = 0` the collapse toggle `«` shares the bottom row — its cell keeps click priority.

**6. `new` hidden in workspace tab-mode** — with `ui.tab_mode = "workspace"` the sidebar's `new` entry does not render and has no hit-area (`AppState::sidebar_new_entry_visible`); the spaces list reclaims the footer row (body rect, scroll metrics, drop indicator bounds all follow). Default tabs mode unchanged.

**7. Server-row metric layout** — line 1: `<name> <latency(peers)|battery(self)> <net i/o (self only)>` (battery quintile glyph + net `\u{f06f3}` move onto the self row's title). Line 2: `<CPU> <RAM> <DISK> <GPU(if exists)>`, space-separated — the `·` separators are gone from the band (`push_band_metric` variant; the status line keeps its dots via the shared `push_metric_with_sep` core). CPU/GPU right-aligned width-3 (`  8%` / ` 42%` / `100%`, `format_percent3`), mem used padded to the width of total (` 92G/512G`, `format_mem_ratio`) — one formatter in the shared status helpers, applied uniformly to self/snapshot/config-peer rows (peer summaries don't carry gpu/net/battery, so those simply omit).

### Tests (now 1970 passing, fmt + clippy -D warnings clean)

- whole-header clicks toggle all three scopes (+ divider row / server row / label still behave)
- menu row pinned to the last sidebar row with divider above; launcher click opens the menu; collapse toggle wins over the menu row
- workspace mode: `new` rect/footer gone, old click position a no-op, list body reclaims the row, render shows no `new` but keeps `menu`; tabs mode unchanged
- fixed-width formatting incl. `100%` fill and mem padding; band joins with spaces while the status line keeps `·`; battery/net on the self title line; gpu width-3 on line 2
