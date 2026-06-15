---
number: 124
title: "worktree create from an existing worktree (inherit its branch/state)"
kind: issue
state: OPEN
author: gerchowl
labels: ["enhancement"]
created: 2026-06-14T12:13:33Z
closed: 
url: https://github.com/gerchowl/herdr/issues/124
---

# worktree create from an existing worktree (inherit its branch/state)

Creating a worktree *from* another worktree row is **explicitly refused** in the TUI — "New and open worktree actions start from the repo parent workspace." (`src/app/worktrees.rs:29-50`). Only the CLI `--base` can branch off an arbitrary ref. The one related path is `branch_session` (#106), which forks the focused pane's *agent session* into a new worktree but still bases on the parent.

**Ask:** allow new-worktree from a linked-worktree workspace, basing the new branch on that worktree's current HEAD so it inherits that workspace's branch/state. Pairs naturally with the base-ref picker (sibling issue).

Part of milestone: Fleet project view + cross-machine worktrees.
