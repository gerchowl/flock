---
number: 1
title: "feat(pane): float the last submitted prompt over agent panes"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-06T10:46:09Z
closed: 2026-06-06T10:46:18Z
merged: 2026-06-06T10:46:18Z
base: feat/sidebar-row-gap
head: top-prompt-float
url: https://github.com/gerchowl/herdr/pull/1
---

# feat(pane): float the last submitted prompt over agent panes

Agent panes pin the **last submitted prompt** to their top rows so long output/thinking never hides what the agent was asked.

- `[ui] prompt_float_lines` (default 3, 0 disables, never covers more than half the pane)
- Prompts taller than the limit collapse in the **middle** (`⋯ +N lines ⋯`) — start and end always survive; overlong single lines are middle-truncated the same way
- Source: Claude `UserPromptSubmit` hook → new `prompt` action in the managed hook script → `pane.report_prompt` RPC → `TerminalState.last_prompt`; ANSI-stripped + length-capped server-side
- Claude integration v5 → v6 (installed hooks surface as *update available*)
- Fixes two environment-dependent branch-session tests that break when the test process runs inside a linked worktree checkout

Verified: 13 new unit tests, 1841/1841 suite on the merged tree, clippy clean, sandboxed tui-probe e2e (multi-line collapse + single-line truncation + prompt replacement).
