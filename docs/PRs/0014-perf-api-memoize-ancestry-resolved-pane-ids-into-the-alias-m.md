---
number: 14
title: "perf(api): memoize ancestry-resolved pane ids into the alias map"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-06T21:38:59Z
closed: 2026-06-06T21:39:14Z
merged: 2026-06-06T21:39:13Z
base: feat/sidebar-row-gap
head: pane-header-hud
url: https://github.com/gerchowl/herdr/pull/14
---

# perf(api): memoize ancestry-resolved pane ids into the alias map

Answering 'is this lazy/efficient?': the walk was correct-but-recomputed per stale report. Now it memoizes into `pane_id_aliases` (the registry that already has shadow-eviction + persistence + handoff chaining) and pane creation GCs dead-target entries. First stale report walks; everything after is a HashMap hit. 1860/1860.
