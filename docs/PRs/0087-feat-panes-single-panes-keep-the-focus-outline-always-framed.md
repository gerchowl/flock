---
number: 87
title: "feat(panes): single panes keep the focus outline (always framed)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-12T11:34:49Z
closed: 2026-06-12T11:34:54Z
merged: 2026-06-12T11:34:54Z
base: master
head: single-pane-outline
url: https://github.com/gerchowl/herdr/pull/87
---

# feat(panes): single panes keep the focus outline (always framed)

User: the single-pane window lacked the outline. The `multi_pane` gate skipped borders for lone/zoomed panes; now always-framed — same #43 focus language at every pane count. 2132/0, clippy clean.
