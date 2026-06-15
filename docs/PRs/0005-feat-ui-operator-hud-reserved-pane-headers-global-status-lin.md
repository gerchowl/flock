---
number: 5
title: "feat(ui): operator HUD — reserved pane headers + global status line"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-06T17:14:05Z
closed: 2026-06-06T17:14:18Z
merged: 2026-06-06T17:14:18Z
base: feat/sidebar-row-gap
head: pane-header-hud
url: https://github.com/gerchowl/herdr/pull/5
---

# feat(ui): operator HUD — reserved pane headers + global status line

One glance, knowing what's up:

```
cpu 62% · mem 14G/16G · disk 126G free · ⚡100% · net ▼▲ · gpu   ← global status line
 repo ·  top-prompt-float ·  branch                            ← per-pane context
 ❯ review the HUD branch ⋯ +2 lines ⋯ ship it when green         ← last prompt
 <pane content — PTY shrunk, never covered>
```

- Reserved rows replace the prompt-float overlay (latched per pane, no resize churn, tiny panes opt out)
- Status line: sysinfo sampler thread, 2s cadence, omit-on-unreadable (GPU via ioreg IOAccelerator, battery via pmset)
- 5 new tests (carve math, gpu parse, human-bytes); 1849/1849; probe-verified end-to-end
