---
number: 17
title: "feat(sidebar): collapse-all keybind + auto-collapse + traffic-light counts"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-09T08:14:37Z
closed: 2026-06-09T23:00:01Z
merged: 2026-06-09T23:00:01Z
base: feat/sidebar-row-gap
head: keyboard-shorcuts
url: https://github.com/gerchowl/herdr/pull/17
---

# feat(sidebar): collapse-all keybind + auto-collapse + traffic-light counts

## What

Sidebar workspace collapse controls + collapsed-group status summary.

- **`toggle_collapse_all` keybind** — collapses every worktree group at once; pressed again (when all are collapsed) expands them all. Unbound by default; set `keys.toggle_collapse_all` to enable.
- **`auto_collapse_groups` UI setting** — when enabled, keeps only the focused workspace's group expanded and collapses the rest as focus moves. Default `false`.
- **Traffic-light counts on collapsed rows** — a collapsed group's hidden members are summarized inline as per-state pane counts (blocked → done → working → idle, attention-priority order), drawn as filled circled digits (❶–❿, then `●N`) colored by state.

## How

- `set_active_workspace()` becomes the single chokepoint for focus changes and triggers auto-collapse; all 7 active-assignment sites route through it.
- `collapsible_space_keys()` centralizes group detection (≥2 members sharing a space key + one non-linked parent), replacing the inline sidebar computation.
- `pane_states()` extracted onto `Workspace`; `aggregate_state()` reuses it; `space_state_counts()` buckets per key for the traffic light.

## Tests

9 new unit tests: collapse toggle round-trip, group-detection edge cases (no parent / lone parent), auto-collapse on/off, `space_state_counts` bucketing, `circled_count` glyph range. Full suite green (1876).

## Follow-up (not in this PR)

Add the binding + setting to the user config (`~/dotfiles/herdr/config.toml`) once merged.
