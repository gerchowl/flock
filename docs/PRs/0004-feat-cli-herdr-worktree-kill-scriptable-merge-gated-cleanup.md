---
number: 4
title: "feat(cli): herdr worktree kill — scriptable merge-gated cleanup"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-06T11:28:15Z
closed: 2026-06-06T11:28:18Z
merged: 2026-06-06T11:28:17Z
base: feat/sidebar-row-gap
head: top-prompt-float
url: https://github.com/gerchowl/herdr/pull/4
---

# feat(cli): herdr worktree kill — scriptable merge-gated cleanup

Scriptable twin of the TUI kill flow, sharing the exact Rust gate functions (SSoT). Powers a `/clean-ws` agent skill: gate dry-run → confirm → kill.

Verified e2e in a sandbox: ancestry-merged dry-run (exit 0 + evidence JSON), unique-commit branch (exit 3, no deletion offered), merged branch real kill (checkout removed + branch deleted).
