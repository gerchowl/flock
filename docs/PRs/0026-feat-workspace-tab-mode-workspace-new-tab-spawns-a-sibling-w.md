---
number: 26
title: "feat(workspace): tab_mode=workspace — new_tab spawns a sibling workspace (#25)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-10T16:17:33Z
closed: 2026-06-10T17:17:58Z
merged: 2026-06-10T17:17:58Z
base: feat/sidebar-row-gap
head: tab-mode-workspace
url: https://github.com/gerchowl/herdr/pull/26
---

# feat(workspace): tab_mode=workspace — new_tab spawns a sibling workspace (#25)

PR 1 of 2 for spike #25 (see the [review consolidation](https://github.com/gerchowl/herdr/issues/25#issuecomment-4672014324)).

`[ui] tab_mode = "workspace"` makes `new_tab` (prefix+c) create a **sibling workspace in the active workspace's space group** instead of a tab — the workspace-as-unit model.

**Review findings baked in:**
- Branch lives inside the shared `create_tab()` → both event loops covered, zero new `request_*` fields (dual-loop trap dodged by design).
- Membership cloned **explicitly** (grouping is keyed by `WorktreeSpaceMembership`, not cwd).
- Sibling cwd pins to the membership **checkout path** (not the live root-pane cwd) → group identity survives a root-pane `cd` across restarts.
- `prompt_new_tab_name` carries over as the sibling's custom name (avoids identical same-cwd labels).
- Default `tabs`: byte-identical behavior; Tab model/persistence/API untouched (upstream divergence minimized per the fork-strategist review).

**Out of scope per consolidation:** switch_tab group-local rerouting (contested → follow-up issue); tab-bar auto-hide (upstream contributor already approved for #448).

Tests: seed-fn pin+clone, membershipless degrade, grouping-key share, config parse. 1908 green, clippy clean, gates passed on commit.
