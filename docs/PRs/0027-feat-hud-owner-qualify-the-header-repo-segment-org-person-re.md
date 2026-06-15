---
number: 27
title: "feat(hud): owner-qualify the header repo segment (org|person/repo)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-10T16:24:17Z
closed: 2026-06-10T16:24:51Z
merged: 2026-06-10T16:24:51Z
base: feat/sidebar-row-gap
head: header-owner
url: https://github.com/gerchowl/herdr/pull/27
---

# feat(hud): owner-qualify the header repo segment (org|person/repo)

The pane-header context line now reads `gerchowl/herdr · <worktree> ·  <branch>` instead of bare `herdr` — the owner (org **or** person) is parsed from the space key, which is already the normalized **origin** URL (`github.com/owner/repo`), so zero additional git calls. `dir:`-fallback and origin-less repos keep the bare label; gitlab nested groups qualify with the top-level org. Unit-tested across key shapes.
