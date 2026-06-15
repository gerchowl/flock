---
number: 123
title: "worktree create: pick the base ref (main / dev / defined branch), not just HEAD"
kind: issue
state: OPEN
author: gerchowl
labels: ["enhancement"]
created: 2026-06-14T12:13:31Z
closed: 
url: https://github.com/gerchowl/herdr/issues/123
---

# worktree create: pick the base ref (main / dev / defined branch), not just HEAD

The TUI new-worktree flow always bases the new branch on the **source checkout's current HEAD** — the literal string `"HEAD"` (`src/app/worktrees.rs:654`, `src/worktree.rs:118`). There is no picker and no per-project notion of a canonical base branch.

The machinery already exists but isn't wired into the create UI:
- `detect_default_branch` (`src/worktree.rs:212`) resolves `origin/HEAD` → `main`/`master` — but is only used by the kill merge-gate.
- The CLI `herdr worktree create --base REF` (`src/cli/worktree.rs`, `src/app/api/worktrees.rs:93`) already accepts an arbitrary base, but there's no UI for it.

**Ask:**
1. A base-ref picker in the new-worktree dialog (default branch, `dev`, current HEAD, arbitrary ref).
2. An optional per-project "defined branch" config so a project can declare its canonical base (e.g. `main` or `dev`).

Part of milestone: Fleet project view + cross-machine worktrees.
