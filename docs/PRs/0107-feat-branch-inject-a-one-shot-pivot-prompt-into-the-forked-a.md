---
number: 107
title: "feat(branch): inject a one-shot pivot prompt into the forked agent (#106)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-13T09:42:01Z
closed: 2026-06-13T09:42:05Z
merged: 2026-06-13T09:42:05Z
base: master
head: branch-pivot-inject
url: https://github.com/gerchowl/herdr/pull/107
---

# feat(branch): inject a one-shot pivot prompt into the forked agent (#106)

Spike #106 -> Option A (positional prompt, user-chosen). branch_session on a Claude pane appends a **configurable** pivot message as the fork's first user turn (the positional prompt on `claude --resume <id> --fork-session <msg>`), so the fork diverges instead of duplicating the parent.

`[worktrees] branch_pivot_message` (empty = off, `<branch>` placeholder, default template ships). **Claude only** -- codex/copilot resume take no positional. **Idempotent by construction**: the argv is built once at branch and never persisted, so later resumes re-inject nothing (the herdr-internals review's key finding). No PTY-write/readiness race (there is no input-ready signal in herdr -- which is exactly why the positional-arg approach wins).

One empirical caveat carried from the spike: that a positional prompt seeds an interactive `--resume --fork-session` session is best confirmed live (branch once, watch the fork open with the nudge as turn 1). Trivially revertible to the SessionStart-hook fallback (#106 Option B) if it doesn't. 2180 unit green (append_pivot_message red/green-style coverage: claude-fork appends, empty no-ops, codex untouched); clippy clean.
