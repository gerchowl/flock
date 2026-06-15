---
number: 43
title: "feat(ui): pane header divider speaks the border focus language (SSoT)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-11T08:19:57Z
closed: 2026-06-11T08:20:02Z
merged: 2026-06-11T08:20:02Z
base: feat/sidebar-row-gap
head: pane-focus-divider
url: https://github.com/gerchowl/herdr/pull/43
---

# feat(ui): pane header divider speaks the border focus language (SSoT)

User request: promote the per-pane header divider to the same color semantics as pane borders — accent (blueish) for the focused pane, overlay (whitish) for the rest — and make a lone pane always read focused.

Implementation is the SSoT version: `pane_focus_color()` is now the single mapping for pane-focus coloring, consumed by both the border styling and the header hairline. Single panes are `is_focused` by construction so 'always highlighted when alone' falls out with zero special-casing. Border thickness (THICK when terminal-active) unchanged. 23 pane/header tests green, full build clean.
