---
number: 92
title: "solo local workspaces render member grammar with no project identity"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-12T14:56:42Z
closed: 2026-06-12T16:08:55Z
url: https://github.com/gerchowl/herdr/issues/92
---

# solo local workspaces render member grammar with no project identity

## Gap (survives #81/#90 — screenshot in chat)

A project with ONE local workspace and no remote rows renders flat as `mba22:keyboard-shorcuts #17` — member grammar, no identity anywhere (which repo? only the PR number hints). Groups got identity leaders (#81); solos got skipped.

## Fix
Solo local rows render like solo remotes already do (#81's deliberate exception): `<icon> <owner/repo> · <server>:<branch>` — identity first, member locator second, one line, no synthetic group. When a second member appears (worktree/remote fold) it graduates to the leader+members form automatically.

## Acceptance
- `✓ gerchowl/herdr · mba22:keyboard-shorcuts #17` for the solo case; grouping unchanged otherwise; grammar stays in ui::grammar (one place).

## References
#62 (grammar spec), #78/#81 (leader identity + solo-remote precedent), #90 (stable order).
