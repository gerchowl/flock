---
number: 8
title: "feat(agents): workspace-scoped attention cycling"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-06T18:52:50Z
closed: 2026-06-06T18:53:03Z
merged: 2026-06-06T18:53:03Z
base: feat/sidebar-row-gap
head: feat/scoped-attention
url: https://github.com/gerchowl/herdr/pull/8
---

# feat(agents): workspace-scoped attention cycling

ctrl+shift+a's little sibling: same blocked-oldest→done-unseen queue filtered to the current space; empty queue cycles the space's panes instead of chiming (scoped all-clear would lie about other workspaces). 1854/1854 tests, clippy clean. New optional keys: focus_attention_workspace(_previous).
