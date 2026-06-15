---
number: 105
title: "workspace creation UX: new-blank-from-home + 'new' on spaces header + right-click->branch on any row"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-13T09:00:48Z
closed: 2026-06-13T09:53:05Z
url: https://github.com/gerchowl/herdr/issues/105
---

# workspace creation UX: new-blank-from-home + 'new' on spaces header + right-click->branch on any row

## Workspace creation UX (3 user asks, one surface)

1. **New blank workspace starts from ~/**: a fresh top-level workspace (not a branched/sibling one) should cwd to $HOME, not inherit a repo/space dir. Branching/sibling creation keeps the space's checkout_path (unchanged); only the "new blank" path defaults to home.
2. **'new' affordance on the spaces header**: the `spaces  all` header gets a `new` (right-aligned or beside the scope toggle) that creates a new blank workspace (from ~/, per #1). This was deprecated under tab_mode=workspace (#44) because "new tab = new sibling"; reinstate it as the BLANK-workspace creator (distinct from prefix+c sibling-in-space). Click = new workspace at $HOME.
3. **Right-click any workspace/space row -> 'branch'**: a context-menu entry on ANY row (not just the focused agent) that runs branch_session against that row's agent -- fork its session into a new worktree. Composes with the existing Mode::ContextMenu machinery (servers band already has one #51; extend to workspace rows).

## Pitfalls
- tab_mode=workspace: distinguish "new blank workspace" (home, top-level, its own project section) from "new sibling tab" (prefix+c, same space). The header 'new' is the former.
- Selection/focus after creating a blank workspace: focus it.
- Right-click branch on a NON-agent row (a plain shell workspace): branch_session needs a resumable agent session -- gray out / no-op with a toast if none (the #1 stale-alias lesson).

## References
#44 (new deprecation), #25/#28 (tab_mode + sibling creation), branch_session, Mode::ContextMenu, #51 (server right-click precedent).

---

## Comments

### gerchowl — 2026-06-13T09:53:04Z

Shipped in PR #110: blank workspace from $HOME (loops-shared request_new_workspace_cwd seam), 'new' on the spaces header in both tab modes, right-click -> Branch session (hidden when no resumable agent). 2184 green.

