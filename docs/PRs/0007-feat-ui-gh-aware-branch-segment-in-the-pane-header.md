---
number: 7
title: "feat(ui): gh-aware branch segment in the pane header"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-06T18:34:44Z
closed: 2026-06-06T18:53:44Z
merged: 2026-06-06T18:53:44Z
base: feat/sidebar-row-gap
head: pane-header-hud
url: https://github.com/gerchowl/herdr/pull/7
---

# feat(ui): gh-aware branch segment in the pane header

` branch ↑2 #6 ✓` — ahead/behind from the existing git cache + PR state from a slow gh poller (120s tick → shared both-loops chokepoint → off-thread `gh pr view --repo <origin>`). Mauve ✓ = merged = the kill-worktree glance signal. Omit-on-absent throughout. 7 new tests; 1856/1856.
