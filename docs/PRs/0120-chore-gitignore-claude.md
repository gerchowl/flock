---
number: 120
title: "chore: gitignore .claude/"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-14T12:08:20Z
closed: 2026-06-14T12:08:26Z
merged: 2026-06-14T12:08:26Z
base: master
head: chore/gitignore-claude
url: https://github.com/gerchowl/herdr/pull/120
---

# chore: gitignore .claude/

Claude Code creates a per-project `.claude/` for local state (scheduled tasks, worktree metadata). It is machine-local — never useful to track. Stops it showing as dirty in `git status`.
