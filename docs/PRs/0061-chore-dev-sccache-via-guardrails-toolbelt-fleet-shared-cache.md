---
number: 61
title: "chore(dev): sccache via guardrails toolbelt (fleet-shared cache)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-11T16:03:09Z
closed: 2026-06-11T16:03:14Z
merged: 2026-06-11T16:03:14Z
base: feat/sidebar-row-gap
head: dedupe-sccache
url: https://github.com/gerchowl/herdr/pull/61
---

# chore(dev): sccache via guardrails toolbelt (fleet-shared cache)

guardrails#15 lifted sccache to the cross-repo mkDevShell (the right home — every consuming repo + worktree inherits one fleet-shared `~/.cache/sccache`, shared deps compile once across projects). Drops herdr's local copy from #60 + its repo-private cache dir. Verified: devShell exports RUSTC_WRAPPER=sccache from the toolbelt.
