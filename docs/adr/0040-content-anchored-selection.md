# ADR 0040 — Content-anchored pane selections

- Status: Accepted
- Date: 2026-10-18
- Decision owner: architect, binding decision on #475
- Issues: #475; copy policy follow-up #2127

## 1. Content identity and runtime ownership

A pane selection captures a content revision and an inclusive row/column
range in scrollback coordinates. AppState remains pure data: it stores the
revision and coordinates, while the pane runtime owns two tracked grid
references from the vendored terminal model. Capture and drag resolution run
under the same terminal mutex as PTY parsing. Output never clears a selection.

At copy time, resolve both tracked endpoints and re-read the selected rows.
The terminal model follows scrolling and resize/reflow and invalidates a
reference whose content was pruned or reset. An unavailable endpoint, wrong
revision or different active screen refuses the copy with a typed attention
toast; do not substitute the rows now occupying the old coordinates.
Highlighting resolves the same endpoints before painting. Each terminal retains
at most one pair of pins until its next selection or destruction. Soft wraps
are unwrapped on copy, so reflow does not insert new line breaks.

## 2. Limits and copy policy

This anchors terminal positions, not immutable text snapshots: a program that
rewrites retained cells changes what a later copy reads. Copying while another
terminal screen is active refuses; returning to a surviving original screen
can resolve again. Evicted or reset content cannot be recovered. Runtime
anchors do not survive process restart or live handoff. Prompt-history panel
selection remains separate from the terminal grid. Existing release-to-copy
behavior is unchanged; copy_on_select configuration is tracked by #2127.
