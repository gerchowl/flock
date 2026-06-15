---
number: 119
title: "feat(float): drag-select and copy text in float panes"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-14T10:58:37Z
closed: 2026-06-14T10:58:43Z
merged: 2026-06-14T10:58:43Z
base: master
head: feat/float-text-selection
url: https://github.com/gerchowl/herdr/pull/119
---

# feat(float): drag-select and copy text in float panes

## What

Float overlays now support host-side mouse text selection just like tiled/layout panes: drag to select, release to copy.

- **`handle_float_mouse`** makes the same forward-or-select decision a layout pane does. A left-press the float's program doesn't claim (mouse-reporting off, or Shift held) anchors a host selection on the float's pane; a press the program *does* claim is forwarded. An in-progress float selection owns the following drag/release even off-overlay (drag clamps to the inner rect, tracks the edge); release copies, a bare click clears.
- **`copy_selection`** resolves a float selection's runtime by the float's `terminal_id` (floats live outside the workspace pane tree), otherwise the layout-pane path.
- **`render_float_overlay`** paints the selection highlight over the float with the same style layout panes use.

## Tests

Adds `drag_inside_float_without_mouse_reporting_selects_and_copies` and `plain_click_in_float_anchors_then_clears_without_copying`. Full suite green (2194 passed).

## Notes

`PaneId` is globally unique (atomic alloc), so float vs layout pane ids never collide. Also includes a separate chore commit untracking the accidentally-committed `.direnv/` devshell cache.
