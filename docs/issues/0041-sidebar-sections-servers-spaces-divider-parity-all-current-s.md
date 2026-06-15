---
number: 41
title: "sidebar sections: servers↔spaces divider parity + all/current scope toggles (servers, spaces) like agents"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-11T07:55:38Z
closed: 2026-06-11T11:53:27Z
url: https://github.com/gerchowl/herdr/issues/41
---

# sidebar sections: servers↔spaces divider parity + all/current scope toggles (servers, spaces) like agents

## Motivation
Post-#34/#39 dogfood: the sidebar's three sections (servers / spaces / agents) have inconsistent chrome and inconsistent scoping controls.

## Scope

**1. Divider parity.** A hairline `─` divider + gap between the `servers` section and the `spaces` section — exactly the visual language of the existing spaces↔agents divider (same divider color/`divider_color()`, same gap rhythm).

**2. all/current toggle instead of collapse.** The agents panel header already has the `all`/`current` scope toggle (`agent_panel_scope`, persisted in the session snapshot, clickable in the panel header). Give the SAME control to:
- **servers**: replace `servers_collapsed` with scope — `all` = full band (home/self/snapshot/config rows); `current` = just the current machine's row (self + home row when a snapshot origin exists, since the way home must never hide).
- **spaces**: header scope toggle — `all` = today's full workspace list; `current` = only the focused workspace's space group (project). Orthogonal to per-group collapse (#17) and `auto_collapse_groups`: scope filters which groups render at all; collapse still folds members within a rendered group.

Both toggles: same render idiom as the agents header toggle, same click hit-area pattern, persisted in the session snapshot alongside `agent_panel_scope` (additive snapshot fields with serde defaults — old sessions restore unchanged).

## Pitfalls
- Hit-areas for the new header toggles must not collide with the section-collapse/scroll affordances; mirror how the agents header lays this out.
- `servers` scope=current must keep the home row visible when attached remotely (never strand).
- Snapshot additivity: defaults so existing session files restore (the LegacyWorkspaceSnapshot pattern).
- Mobile layout unaffected.

## References
agents toggle: `agent_panel_scope` / `AgentPanelScopeConfig` (config/model.rs, persisted in persist/snapshot.rs SessionSnapshot); servers band: PR #34/#39 (src/ui/sidebar.rs `render_servers_section`, `servers_collapsed`); dividers: `divider_color()` usage in sidebar.

---

## Comments

### gerchowl — 2026-06-11T07:58:27Z

## Scope additions (user)

1. **Whole-header click target**: the all/current scope toggle activates by clicking the `all/current` label AND by clicking the section title itself (`agents`, `spaces`, `servers`) — the entire header row is the toggle's hit-area, not just the small scope word.

2. **Relocate the 'menu' entry**: the `menu` item currently sitting mid-field in the sidebar header area moves to the BOTTOM of the sidebar as a standalone row, separated from the content above by the same hairline `─` divider idiom — i.e. final sidebar vertical order: servers ─ spaces ─ agents ─ … ─ ─ menu (pinned last, own row, own divider).

### gerchowl — 2026-06-11T07:59:31Z

## Scope addition (user): deprecate the sidebar 'new' entry

With the workspace-as-unit model (#25/#26: `tab_mode = "workspace"` → new_tab spawns a sibling workspace) and `branch_session` covering the fork-running-agent-into-worktree flow, the sidebar's `new` entry is workflow-redundant.

**Implementation (divergence-minimal):** hide the `new` header entry when `tab_mode = "workspace"` — the mode where it's redundant by construction (creation = prefix+c sibling / branch_session / new_worktree dialog). Default `tabs` mode keeps it (upstream-parity surface, fork tracks daily). Its hit-area/layout slot goes away with it — which composes with the menu-to-bottom relocation in this same issue: in workspace mode the header band ends up just the section titles + scope toggles.

Final sidebar in the user's daily config: servers ─ spaces ─ agents ─ … ─ menu — no mid-field 'new', whole-header scope toggles.

### gerchowl — 2026-06-11T08:16:08Z

## Scope addition (user): server-row metric layout

Two-line server rows, reformatted for the narrow sidebar:

```
<name> <ping|bat> <i/o>
<CPU> <RAM> <DISK> <GPU (if exists)>
```

- Line 1: name + latency (peers) | battery (self/laptop) + net i/o (self only — peer summaries don't carry net).
- Line 2: the metric glyphs+values, SPACE-separated — **drop the `·` separators** (the dots cost width for nothing at this density).
- **CPU & GPU always 3-digit-spaced** (right-aligned width-3: `  8%`, ` 42%`, `100%`) so columns are stable across refreshes.
- **mem: pad used to the width of total** (` 92G/512G`, ` 8G/17G`) so the slash column doesn't jitter.
- Applies to self row, snapshot rows, and config-peer rows uniformly (one formatter — keep it in the shared status helpers so the status line can adopt the same fixed-width discipline).

### gerchowl — 2026-06-11T11:53:26Z

Shipped across PR #44 + the #54 state-language integration.

