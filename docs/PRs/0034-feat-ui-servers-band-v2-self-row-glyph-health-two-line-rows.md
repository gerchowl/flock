---
number: 34
title: "feat(ui): servers band v2 — self row, glyph health, two-line rows (#32)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-10T20:59:24Z
closed: 2026-06-10T21:00:10Z
merged: 2026-06-10T21:00:10Z
base: feat/sidebar-row-gap
head: servers-band-v2
url: https://github.com/gerchowl/herdr/pull/34
---

# feat(ui): servers band v2 — self row, glyph health, two-line rows (#32)

Implements #32: the first two-server dogfood showed the band hides the local server and truncates peer health into noise at sidebar width. Pure render change in the sidebar's servers section — no protocol bump, no new Mode, no AppState shape changes.

## What changed

**Self row first (`src/ui/sidebar.rs`)**
- The local server renders as the FIRST band row, marked current: accent dot + `short_host_name()` + `✦`. Health comes from the same local `SystemStats` sample the status line shows; the agent rollup counts local pane states.
- Slot 0 deliberately gets **no** `ServerCardArea`, so clicking yourself can never request a `SwitchServer`.

**Two-line rows + shared glyph language**
- Every server (self + peers) renders as two lines: `<dot> <name> <latency-if-peer>` over indented compact health.
- The status line's glyph/threshold helpers (`utilization_style`, `push_metric`, new `mem_percent`) are extracted from `render_status_line` into shared `pub(super)` fns in `src/ui/status.rs` and reused — line 2 reads `cpu 42% · mem 13G/16G · disk 213G` with the same 60/85% color shifts.
- Agent rollup uses the collapsed-group traffic-light convention: `circled_count` glyphs, blocked (red) then working (yellow), zeros omitted.
- Unreachable peers stay compact: red `○` on line 1, dim `unreachable {age}` on line 2.

**Layout + hit-areas**
- `servers_section_height` / `compute_server_section_areas` learned the two-line slot math (`SERVER_ROW_LINES`, `server_slot_rect`); rows only render when their full two lines fit under the band's half-section cap.
- Peer cards are now two-line rects; the mouse matcher (`src/app/input/mouse.rs`) hits anywhere inside a card instead of only its first line.

## Tests
- Updated: band height math, hit-area layout, peer row content, unreachable presentation (now with outage age).
- New: `self_server_rows_show_local_identity_and_glyph_health`, full-frame `servers_band_renders_self_row_first_with_two_line_peers` (TestBackend buffer assertions), and mouse-level `server_band_click_hits_both_peer_lines_but_never_the_self_row`.
- `cargo test --bin herdr`: **1927 passed, 0 failed**; `peer_federation` e2e (incl. click-switches-server) green; fmt + clippy `--all-targets` clean.

## Deviations from the brief
- Line 2 drops the status line's `" free"` suffix on disk (`disk 213G` instead of `disk 213G free`) to fit sidebar width; the status line itself is unchanged.
- The two-line hit-area required widening the mouse matcher from `row == rect.y` to row-in-rect — a one-expression change in `src/app/input/mouse.rs`, covered by the new mouse test.

Closes #32
