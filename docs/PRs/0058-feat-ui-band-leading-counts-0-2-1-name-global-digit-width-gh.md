---
number: 58
title: "feat(ui): band leading counts — 0 2 1 <name>, global digit width, ghost rows (#42)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-11T14:07:45Z
closed: 2026-06-11T14:07:50Z
merged: 2026-06-11T14:07:50Z
base: feat/sidebar-row-gap
head: band-leading-counts
url: https://github.com/gerchowl/herdr/pull/58
---

# feat(ui): band leading counts — 0 2 1 <name>, global digit width, ghost rows (#42)

User-final band rendering after the medallion eyeball: **leading count columns** `0 2 1 herdr` — fixed r/y/g (blocked/working/calm), zeros muted, **band-global digit width** (one server hitting 10 right-aligns every row to two digits). `[ui] medallion_style` → `server_state_mark = counts (default) | medallion_sextant | medallion_quadrant` — the rectangle medallion survives as a config escape.

**Unreachable rows ghost** (user spec): hollow ○, struck-through name, `󰌷`-off broken-link icon + outage age in the latency slot, last-known stats AND counts kept visible but muted+italic.

New `StateTally` (uncapped counts) beside `StateJoin`; band loop two-phases for the global width. 2055 tests green, clippy clean, docs updated.
