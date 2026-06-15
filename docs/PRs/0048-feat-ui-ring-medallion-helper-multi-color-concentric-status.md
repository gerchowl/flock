---
number: 48
title: "feat(ui): ring-medallion helper — multi-color concentric status rings in sub-cell blocks (#42)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-11T09:32:20Z
closed: 2026-06-11T09:42:56Z
merged: 2026-06-11T09:42:56Z
base: feat/sidebar-row-gap
head: ring-medallion
url: https://github.com/gerchowl/herdr/pull/48
---

# feat(ui): ring-medallion helper — multi-color concentric status rings in sub-cell blocks (#42)

## Summary

Pure rendering helper for #42's "leading state circle" concentric-target variant: `ring_medallion(rings, base_bg, style)` in the new `src/ui/medallion.rs` rasterizes up to three severity-sorted ring colors (outer → inner) into a 2-cell-wide × 2-line-tall block of sub-cell mosaic glyphs, returning ready-to-place `[Vec<Span>; 2]`.

- **Sextant mode** (`MedallionStyle::Sextant`): U+1FB00..=U+1FB3B Symbols for Legacy Computing, 2×3 sub-blocks per cell → 4×6 grid.
- **Quadrant fallback** (`MedallionStyle::Quadrant`): U+2580..=U+259F quadrant blocks, 2×2 per cell → 4×4 grid, for fonts without legacy-computing coverage. Caller picks.
- **Compose guarantee**: every cell's bg is `base_bg` and its glyph leaves the grid-corner sub-block unlit (rounded shape), so the medallion composes with row highlight/selection fills.

## The documented approximation

A terminal cell carries one fg + one bg, and every cell of the 2×2 block touches a grid corner (pinned to `base_bg`), the outer ring, *and* the core — three colors, two slots. A faithful bullseye is not expressible, so each cell elects one ring color, walking the severity-sorted ring list along the TL → TR/BL → BR diagonal: the outermost (worst) color anchors the top-left, the innermost the bottom-right.

```text
ideal raster (4×6)     elected fg per cell     rendered (sextant / quadrant)
  . o o .              3 rings: [r0][r1]          🬻🬺        ▟▙
  o m m o                       [r1][r2]          🬬🬝        ▜▛
  o c c o              2 rings: [r0][r0]
  o c c o                       [r1][r1]
  o m m o
  . o o .
```

Edge cases: 1 ring → solid rounded dot in a `base_bg` field; empty rings → all-`base_bg` blank; duplicate colors well-formed; >3 rings → inner extras ignored.

## Scope

New file + one `mod medallion;` line in `src/ui.rs` only — **no render-path integration** (that lands with #42, after PR #44). Conflict-trivial with the other in-flight sidebar work.

## Testing

12 in-module tests render the spans into a 2×2 `TestBackend` buffer and assert: corner cells carry `bg == base_bg` with corner-open glyphs (both styles), `rings[0]` on edge cells, innermost color on the core-reaching cell, charset confinement (quadrant: U+2580–259F; sextant: U+1FB00–1FB3B + space/full block), solid-dot/blank/duplicate/truncation edge cases, determinism, and the sub-pixel grid geometry. Full suite: **1958 passed, 0 failed**; fmt + clippy clean.
