---
number: 114
title: "pane geometry not re-asserted on in-session workspace/pane switch (cramped narrow render until resize)"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-13T21:36:07Z
closed: 2026-06-13T21:59:34Z
url: https://github.com/gerchowl/herdr/issues/114
---

# pane geometry not re-asserted on in-session workspace/pane switch (cramped narrow render until resize)

## Bug: pane geometry not re-asserted on workspace/pane SWITCH (sibling of #77/#82)

Switching to a workspace/pane sometimes renders text cramped to one side (stale NARROW width), as if a small window. User's own diagnosis (precise): a TWO-pane layout self-heals because dragging the split handle fires a resize that re-asserts geometry; a SINGLE pane has no such trigger, so it stays cramped until an external window resize.

## Root cause (hypothesis)
#82 re-asserts geometry on the SLOT-FLIP (server switch) path. This is the in-session WORKSPACE/PANE switch path: when a background workspace/pane becomes active (focus switch, new pane, space switch), its PTY/VT keeps a stale geometry until a resize event reaches it. The split-drag triggers a resize for multi-pane; single-pane gets nothing -> stays at the stale width.

## Fix
On activating a workspace/pane (the focus/switch chokepoint that makes a previously-background pane foreground), re-assert geometry to that pane's PTY+VT from the current terminal area -- the same re-assert #82 does on slot-flip, but on the local switch path. Likely near update_terminal_state / the workspace-activate path; mirror resize_shared_runtime_to_effective_size / the #82 Resize re-assert. The server's Resize handler then reflows.

## Acceptance
- Switch to a single-pane workspace that was last rendered at a different width -> it renders at the CURRENT width immediately, no manual resize.
- e2e: activate a background workspace whose pane has stale geometry, assert its PTY is resized to the foreground area.

## References
#77/#82 (slot-flip geometry + reflow), the workspace-activate / focus-switch path, resize_shared_runtime_to_effective_size.
