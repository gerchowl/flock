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
Flock records the owning screen’s erase generation in each runtime anchor.
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
DECCOLM (private mode 3 when mode 40 permits it) calls `Terminal.deccolm`
through `Handler.setMode`, resizing internally and then calling
`Terminal.eraseDisplay(.complete)`. That bypasses flock's
resize hook. Invalidate on private mode 3 set/reset/restore (`h`, `l`, `r`)
without mirroring mode 40; this intentionally refuses even when mode 40
would suppress the operation. DECALN (`ESC # 8`) calls `Terminal.decaln`,
which clears all active rows in place and fills them with E. Recognize its single `#` intermediate across
fragmented writes and invalidate before resolving either endpoint. Both
operations invalidate the screen being erased. This adopts the
full-screen-clear refusal policy for #2151; the broader partial/in-place
erase policy, including ED 0 after cursor-home, remains tracked by #2143.
Ordinary retained-cell rewrites still follow the copy policy in §2.

Full reset (`ESC c`) invalidates both screen generations. Screen mode
1047/1049 transitions conservatively invalidate alternate identity, including XTRESTORE (`CSI ? Pm
r`) regardless of the saved value; mode 47 restore is conservatively treated
as alternate loss too. None of these alternate facts invalidate primary cells.
`TrackedSelection::points` compares only its captured screen’s generation and
requires that screen to be active before resolving endpoints.

`SelectionEraseTracker` owns per-screen loss-on-resize facts. Primary loses
history on resize only when configured with zero scrollback. Alternate is
always created with zero scrollback by native `Terminal.switchScreen`.
`Terminal::resize` therefore invalidates alternate identity even while it is
inactive, and primary identity only for primary loss. Alternate history or
past allocation never vetoes a primary anchor. This covers modes 47, 1047 and
1049, saved restores and teardown without a bridge or C-API change (#2146).

`SelectionEraseTracker::dispatch_csi` identifies screen-switch boundaries.
`Terminal::write` flushes through each boundary and uses `synchronize_screen`
to read the native active screen before tracking the following bytes. This
preserves native parser state across segments; alternate enter/output/erase/exit
in one PTY read cannot leak alternate erase facts into primary. Saved mode bits
are not mirrored, and no successful allocation or restored value is guessed.
A failed native query leaves the active screen unknown. An erase while that
own-screen fact is unknown, including a later mode inside the same CSI,
invalidates both generations, failing safe. DECCOLM also applies per-screen
resize-loss facts before invalidating its active erase.

Audit: the stream dispatcher sends XTSAVE to `save_mode` / `modes.save`,
which only stores bits; XTRESTORE dispatches `restore_mode` through
`modes.restore` and `Handler.setMode`. Only modes 47, 1047 and 1049 call
`Terminal.switchScreenMode` in that setter; mode 1048 saves/restores the cursor
only. DECRQM calls `Handler.requestMode` / `getReport` and sends a report
without setting modes or switching screens. Native-terminal tests cover saved
true and unsaved false for all three screen modes and confirm XTSAVE/DECRQM
preserve content. Real-PTY tests cover destructive restore paths, primary preservation after
full-screen apps and unsaved restore, primary loss without scrollback, and
active/inactive alternate loss or teardown in all three screen modes. Ordinary
primary resize/reflow with scrollback remains supported after alternate use.
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
