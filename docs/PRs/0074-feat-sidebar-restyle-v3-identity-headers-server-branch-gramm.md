---
number: 74
title: "feat(sidebar): restyle v3 — identity headers, server:branch grammar, agent icons, single-row agents (#62)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-11T22:59:52Z
closed: 2026-06-12T00:10:24Z
merged: 2026-06-12T00:10:24Z
base: feat/sidebar-row-gap
head: restyle-v3
url: https://github.com/gerchowl/herdr/pull/74
---

# feat(sidebar): restyle v3 — identity headers, server:branch grammar, agent icons, single-row agents (#62)

Implements #62 (sidebar restyle v3). Body + all comments are the locked spec.

## Stages

**1–3 · Identity headers, uniform `<server>:<target>` grammar, agent icons** (commit f9f02bb, from the prior worktree; verified + carried forward)
- Space row = project identity (`owner/repo` per #27); members render uniformly as `<server>:<target>` (local and remote), two-line name+branch row collapses to one line with ahead/behind + PR glyph inline.
- Agent-style icons (done / spinner / blocked / muted) replace the circles on space + member rows, driven by each row's join head. One label formatter lives in `ui::grammar` so the in-flight origin-gossip merge is trivial.

**4 · Agents panel → single rows + remote agents**
- Each entry is one line: `<icon> <agent> <server> <proj> <workspace|branch>`. Status text, live-activity, custom-status, and header-field chips drop from the panel (they remain in the header/nav/member rows). Location truncates right-to-left via `grammar::agent_location_label`.
- Remote agents fold into the all-scope panel from the same peer summaries that feed the spaces list, scope-respecting; selecting one requests the same peer switch its workspace row would (`AgentDetailTarget`). Row stride dropped two-line → one across visible-count + hit-test.

**5 · `switch_space` indexed action**
- New indexed action (`BindingConfig::empty` default, `keys.switch_space`) jumps to the Nth project *section*. The dispatch arm is a thin shell; section resolution lives in `AppState::space_switch_target` (active member if in-section, else the section head — main when present). Section heads show their 1-based index when bound. Existing dispatch arms unchanged.

**6 · Close-main keeps the space; close-whole-space on the space row**
- `close_selected_workspace` now closes only the selected workspace. Closing main no longer tears down the group — it survives on members' own membership keys. `collapsible_space_keys` drops the has-parent requirement; `space_head_idx` derives the section head (main, else first member) in one place for grouping + triangle + selection.
- New `close_selected_space` (close every member) is wired to the space row's `Close group` context-menu item (including on a worktree head once main is gone), behind an explicit `confirm_close_whole_space` flag. `kill_worktree` already derives its root from each member's own membership — no change. Restore-from-members works for free (grouping reads persisted per-workspace membership).

## Judgment calls
- **"Most recently active member" fallback**: no per-workspace MRU timestamp exists; used the active workspace when it belongs to the section, else the first member in workspace order (deterministic). Adding a timestamp was out of scope.
- **Section-index display**: shown only when `switch_space` is bound, so the unbound default stays uncluttered (mirrors collapsed-sidebar numbering).
- **`live_activity` field** removed from `AgentPanelEntry` — no renderer reads it after the single-row change (clippy `-D warnings` would block otherwise).
- **Input files**: touched `input/sidebar.rs` + `input/mouse.rs` minimally (row-stride + remote routing for the agents panel) since the single-row layout makes the old two-row hit-test math wrong; kept member-label rendering in one `ui::grammar` function for a trivial origin-gossip merge.
- A `top_drop_slot` drag test premise was a two-line-layout artifact; reframed to the stable card→insert-index invariants.

## Tests
- Full unit suite: **2023 passed, 0 failed, 1 ignored**. Clippy `-D warnings` clean.
- `--test live_handoff`: **16 passed**.
- `--test peer_federation`: **5 passed**; updated two assertions to the new owner/repo identity on remote leader rows. One failure (`switch_snapshot_renders_home_row_on_spoke_and_home_switches_back`, the carried-fleet "ghost" row) is **pre-existing and unrelated** — it fails identically at the true base `fa7e9d0` before any restyle work; it lives in the concurrent origin-gossip/snapshot area.

Do not merge.
