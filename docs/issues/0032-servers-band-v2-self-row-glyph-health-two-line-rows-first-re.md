---
number: 32
title: "servers band v2: self row, glyph health, two-line rows (first real two-server dogfood)"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-10T20:40:50Z
closed: 2026-06-11T11:53:21Z
url: https://github.com/gerchowl/herdr/issues/32
---

# servers band v2: self row, glyph health, two-line rows (first real two-server dogfood)

First live two-server session (mba22+sage, post-#19) surfaced three UX gaps in the servers band:

1. **The local server isn't listed.** Only peers render — there's no 'home' anchor row. Render the local server as the FIRST row, marked as current (e.g. `● mba22 ✦`), so the band shows the whole fleet symmetrically and 'where am I' is always answered.
2. **Rows are too wide to read.** Health renders as named fields; at sidebar width it truncates into noise. Replace with the status line's existing glyph language (cpu/mem/disk glyphs from src/ui/status.rs) and split each server into TWO lines: line 1 = reachability dot + name + latency (+ current marker), line 2 = indented compact health glyphs + agent rollup.
3. **Navigation context loss** (config-side fix, g-fleet): switch-on-select onto a peer strands you when the peer doesn't list YOU — peers must be a MESH (per-host generated lists, fleet minus self). Config work happens in g-fleet; herdr-side nothing needed beyond (1) so the 'go back' row exists once meshed.

Constraints: pure render change (src/ui/sidebar.rs servers section); no protocol bump; no new Mode; the summary data needed is already in PeerStatus/SystemStats.

---

## Comments

### gerchowl — 2026-06-11T11:53:20Z

Shipped across PR #34 (v1) + #44 (metric layout) + #54/#55 (medallion join rows).

