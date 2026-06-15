---
number: 55
title: "feat(ui): medallion v2 — 6x6 canvas, nested rectangular bands (#42)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-11T11:27:48Z
closed: 2026-06-11T11:52:09Z
merged: 2026-06-11T11:52:09Z
base: feat/sidebar-row-gap
head: medallion-6x6
url: https://github.com/gerchowl/herdr/pull/55
---

# feat(ui): medallion v2 — 6x6 canvas, nested rectangular bands (#42)

User-final medallion geometry: **3 cells × 2 lines** (sextant 6×6 / quadrant 6×4 fallback), **rectangular nested bands** widths 1/1/2 (corners in the outer band — no rounding, fully opaque), duplicate ring colors merge **by color** into solid regions (`r·r·r` = one solid red rectangle). Center-column cells electing innermost-fg over outer-bg is the one documented approximation. `MEDALLION_WIDTH` 2→3 — #54's band integration picks the width up via the const (full suite green on the rebase: 2021/0). Replaces the rounded 2×2 election design after the eyeball round.
