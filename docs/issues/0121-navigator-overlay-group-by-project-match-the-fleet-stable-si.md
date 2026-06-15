---
number: 121
title: "navigator overlay: group by project (match the fleet-stable sidebar)"
kind: issue
state: OPEN
author: gerchowl
labels: ["enhancement"]
created: 2026-06-14T12:13:29Z
closed: 
url: https://github.com/gerchowl/herdr/issues/121
---

# navigator overlay: group by project (match the fleet-stable sidebar)

The `/` navigator overlay lists workspaces in **raw storage order** with no project grouping or fleet-stable sort — it just iterates `self.workspaces` and expands tab/pane children (`src/app/actions.rs:297` `navigator_rows`, rendered by `src/ui/navigator.rs`).

The **sidebar** already does the right thing: it folds checkouts by `project_key`, renders the main checkout as the section head with linked worktrees + peer rows indented, and sorts sections by machine-independent `project_key` so the list reads identically on every server (#85 — `src/app/actions.rs:945` `project_section_sort_ids`, `src/ui/sidebar.rs:584`).

**Ask:** make the navigator adopt the same project grouping + fleet-stable ordering as the sidebar, so the grouped `<server>:<branch>`-under-`owner/repo` model is consistent in both list UIs.

Part of milestone: Fleet project view + cross-machine worktrees.
