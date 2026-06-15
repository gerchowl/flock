---
number: 64
title: "feat(ui): band name-first layout + sidebar branch glyph + rect dedup"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-11T19:09:03Z
closed: 2026-06-11T19:09:44Z
merged: 2026-06-11T19:09:44Z
base: feat/sidebar-row-gap
head: band-polish
url: https://github.com/gerchowl/herdr/pull/64
---

# feat(ui): band name-first layout + sidebar branch glyph + rect dedup

Three dogfood-feedback UI refinements (user-specced from live screenshots).

## 1. Servers band: name-first row layout
`<name+marker> <R Y G counts> <ping | battery-glyph> <net i/o>` over a flush-left `<CPU> <RAM> <DISK> <GPU>` line.

- Name+marker leads every row; the field pads to the band-wide max **display width** (unicode-width), computed in the existing two-phase loop next to the global digit width, so the r/y/g count columns stay vertically aligned across rows.
- Counts keep everything from #58: fixed r/y/g columns, muted DIM zeros, band-global digit width, ghosted counts muted+italic.
- Self-row battery is now a **colored glyph only** (no percent text): red ≤15, peach ≤40, green above — `band_battery_style` next to the quintile/charging glyph selection in `status.rs`; the status line keeps its percent.
- Both lines flush left: the health line lost its counts-width indent; `counts_lead_width` and `SERVER_HEALTH_INDENT` are gone. Home + ghost second lines too.
- Ghost rows keep #58/#59 styling in the new order: struck muted italic name → muted counts → `\u{f033a}` + outage age in the latency slot.
- Home row (`← <origin> home`) is name-first with no counts; its name field pads the same way so columns stay aligned.
- `ServerRowBuild` became a struct splitting name / title-rest / health so the band-global padding can happen in the paint phase; the medallion config mode keeps its leading ring mark.

## 2. Sidebar branch line gets the git glyph
The workspace rows' second line now leads with the same `\u{e0a0}` branch glyph the pane header uses (styled with the line's branch color), so `main ↓15` reads as the row's git metadata instead of a phantom sibling row. Truncation budget adjusted +2 cells.

## 3. Redundant single rect dropped
Packed `▮` rects render only where they aggregate: always on group-leading rows (group join), on individual workspace/member rows only when `join.classes().len() > 1` — a lone rect just duplicated the leading circle's color. The hollow `▯` (no live agents) renders wherever it did before.

## Tests
- Full bin suite: **2058 passed, 0 failed** (`cargo test --bin herdr`).
- `--test peer_federation`: 6 passed (home/ghost band rows still match).
- fmt + clippy clean (guardrails gates passed on commit).
- Updated: all band builder unit tests, `servers_band_rows_lead_with_name_then_counts` (renamed), `band_counts_share_a_global_digit_width`, ghost test, ui.rs render tests, packed-rect group/member tests.
- New: `band_name_padding_aligns_count_columns_across_rows` (two peers, different name widths → count columns at same x), `individual_row_drops_the_single_class_rect_but_keeps_aggregates`, `band_battery_color_carries_the_level`.
