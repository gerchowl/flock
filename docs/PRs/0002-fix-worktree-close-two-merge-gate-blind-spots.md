---
number: 2
title: "fix(worktree): close two merge-gate blind spots"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-06T11:04:19Z
closed: 2026-06-06T11:04:30Z
merged: 2026-06-06T11:04:30Z
base: feat/sidebar-row-gap
head: top-prompt-float
url: https://github.com/gerchowl/herdr/pull/2
---

# fix(worktree): close two merge-gate blind spots

Found by dogfooding `kill_worktree` on the prompt-float worktree itself: the gate said **no merge evidence** for a branch whose PR (#1) was merged.

1. `gh pr view` resolved the multi-remote checkout (origin fork + upstream) to **upstream**, where the fork's PR doesn't exist → now retried pinned `--repo <origin owner/repo>`
2. The git fallback only consults the **default branch**, but the merge landed on `feat/sidebar-row-gap` → new **remote containment** fallback: branch tip reachable from another pushed remote ref counts as evidence

2 new tests (remote-URL parsing, bare-origin containment scenario incl. own-tracking-ref exclusion); 1843/1843 suite.
