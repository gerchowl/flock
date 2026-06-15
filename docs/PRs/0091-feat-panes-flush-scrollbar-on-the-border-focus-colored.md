---
number: 91
title: "feat(panes): flush scrollbar on the border, focus-colored"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-12T13:32:14Z
closed: 2026-06-12T13:32:19Z
merged: 2026-06-12T13:32:19Z
base: master
head: scrollbar-flush
url: https://github.com/gerchowl/herdr/pull/91
---

# feat(panes): flush scrollbar on the border, focus-colored

User: scrollbars flush to the outline, own column removed, outline colors (accent focused / grey others). The thumb now draws over the right border line (possible since #87 made every pane framed); the track IS the border; content reclaims the gutter column; colors via the pane_focus_color SSoT. 2139/0, clippy clean.
