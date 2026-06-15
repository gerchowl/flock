---
number: 6
title: "docs(agents): codify the fork fleet PR flow"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-06T18:30:46Z
closed: 2026-06-06T18:30:57Z
merged: 2026-06-06T18:30:57Z
base: feat/sidebar-row-gap
head: chore/fork-pr-flow
url: https://github.com/gerchowl/herdr/pull/6
---

# docs(agents): codify the fork fleet PR flow

Locks in the workflow the fleet converged on (PRs #1–#5): worktree isolation → PR into feat/sidebar-row-gap → merge → verified flake pin → live-handoff → merge-gated cleanup. Direct pushes caused three pin races today; this ends them. Doc-only change, no deploy needed.
