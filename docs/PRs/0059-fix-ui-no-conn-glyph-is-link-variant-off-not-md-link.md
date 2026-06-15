---
number: 59
title: "fix(ui): no-conn glyph is link_variant_off, not md-link"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-11T15:44:06Z
closed: 2026-06-11T15:44:11Z
merged: 2026-06-11T15:44:11Z
base: feat/sidebar-row-gap
head: fix-noconn-glyph
url: https://github.com/gerchowl/herdr/pull/59
---

# fix(ui): no-conn glyph is link_variant_off, not md-link

User caught it live: `f0337` is **nf-md-link** (plain chain — reads as a leaf at cell size). The ghost row's latency slot now uses the actual broken chain, **nf-md-link_variant_off `f033a`** 󰌺 (verified against the nerd-fonts glyphnames registry).
