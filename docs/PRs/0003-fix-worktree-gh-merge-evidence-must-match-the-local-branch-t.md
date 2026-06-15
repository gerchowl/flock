---
number: 3
title: "fix(worktree): gh merge evidence must match the local branch tip"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-06T11:17:57Z
closed: 2026-06-06T11:20:48Z
merged: 2026-06-06T11:20:48Z
base: feat/sidebar-row-gap
head: top-prompt-float
url: https://github.com/gerchowl/herdr/pull/3
---

# fix(worktree): gh merge evidence must match the local branch tip

Follow-up hardening from reviewing the dirty-checkout behavior: `gh pr view` matches by branch **name**, so commits added locally after the PR merged were still blessed by stale evidence. gh evidence now requires `headRefOid == local tip`; otherwise the gate falls through to the tip-exact git checks, which correctly reject unrecorded work. 1843/1843.
