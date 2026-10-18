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
Flock also records a terminal erase generation in each runtime anchor.
ED 3 (`CSI 3 J`) calls `Screen.eraseHistory` → `PageList.eraseRows` /
`erasePage`. Those operations relocate tracked pins into a surviving row
without marking them garbage. A generation mismatch therefore refuses in
`resolve_content_selection` before either copying or highlighting. Tracking
runs at `Terminal.write`, across fragmented PTY reads, and leaves vendored
libghostty-vt unchanged. Its CSI parameters follow the vendored parser's
24-slot limit, saturating 16-bit values, saved empty slots and non-empty
final-slot rule. A trailing separator saves the preceding value without
adding a final empty value: `CSI 3 ; J` is ED 3 and `CSI 2 ; J` is ED 2,
while `CSI 3 ; ; J` has two parameters and is ignored. Empty saved values
are zero; no parameters select ED 0. Colons suppress non-SGR dispatch.
A table test checks these forms against the vendored terminal's actual
screen/history effects, not a second constructed parser model.

ED 2 (`CSI 2 J`) normally clears the main screen's active cells **in place**
via `clearRows`; at a semantic shell prompt it may first scroll them into
history. Neither path reliably invalidates pins. Treat ED 2 as a destructive
boundary too. ED 22 scrolls active rows into history and retains the anchors.
Full reset (`ESC c`), mode 1047/1049 screen clears and no-scrollback resize
also invalidate the generation. The latter calls `eraseHistory` in
`Screen.resize`. `Terminal.resize` resizes an allocated alternate screen
even while primary is active, so the tracker remembers its allocation
across `?47l` and invalidates on resize until full reset removes it.
XTRESTORE (`CSI ? Pm r`) also reaches `setMode` and `switchScreenMode`:
restoring false for 1047 clears the departing alternate screen, and restoring
true for 1049 clears the entered alternate screen. Every private restore
naming 47, 1047 or 1049 therefore invalidates and conservatively marks
alternate allocation possible, regardless of the saved value. Saved mode
bits are not mirrored. Full reset clears both the saved modes and allocation;
a subsequent restore marks possible allocation again, even when unsaved
(false). Resizes conservatively invalidate primary-screen anchors too once
alternate allocation is known or possible.

Audit: `stream.zig` dispatches XTSAVE to `save_mode`, which only stores bits
(`stream_terminal.zig:191`); XTRESTORE dispatches `restore_mode` through
`modes.restore` and `setMode` (`stream_terminal.zig:192–194`). Only modes
47, 1047 and 1049 call `switchScreenMode` in that setter
(`stream_terminal.zig:490–492`); mode 1048 saves/restores the cursor only.
DECRQM calls `requestMode` / `getReport` and sends a report without setting
modes or switching screens (`stream_terminal.zig:423–426`). Native-terminal
tests cover saved true and unsaved false for all three screen modes and
confirm XTSAVE/DECRQM preserve content. Real-PTY tests cover both destructive
restore paths, plus capture after reset/restore followed by resize. Ordinary primary resize/reflow before alternate allocation
with scrollback remains supported.
The other `erasePage` path trims blank trailing resize rows only after
checking that no tracked pin occupies them; normal history pruning already
marks evicted pins garbage. Flock does not expose the full application's
`Termio.clearScreen` / `eraseActive` API.

Highlighting resolves the same endpoints before painting. Each terminal retains
at most one pair of pins until its next selection or destruction. Soft wraps
are unwrapped on copy, so reflow does not insert new line breaks.

## 2. Limits and copy policy

This anchors terminal positions, not immutable text snapshots: a program that
rewrites retained cells changes what a later copy reads. Copying while another
terminal screen is active refuses; returning to a surviving original screen
can resolve again if no erase boundary occurred. Generation invalidation is
conservative: an erase anywhere invalidates the terminal's selection, even
if its selected rows survived (for example ED 2 over a history selection).
An exhausted generation counter refuses all subsequent anchors. Evicted,
erased or reset content cannot be recovered. A missing runtime refuses with
the distinct `pane_closed` reason and a “Selected pane closed” toast. Runtime
anchors do not survive process restart or live handoff. Prompt-history panel
selection remains separate from the terminal grid. Existing release-to-copy
behavior is unchanged; copy_on_select configuration is tracked by #2127.
