---
number: 13
title: "feat(api): peer-PID pane resolution + hostname in status line"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-06T20:23:21Z
closed: 2026-06-06T20:23:35Z
merged: 2026-06-06T20:23:35Z
base: feat/sidebar-row-gap
head: pane-header-hud
url: https://github.com/gerchowl/herdr/pull/13
---

# feat(api): peer-PID pane resolution + hostname in status line

Heals every pane orphaned by pre-chaining handoffs: socket peer PID → process-tree ancestry → pane child PID. Env ids become advisory. Hostname (accent, bold) now leads the machine status line. 1858/1858, clippy clean.
