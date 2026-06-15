---
number: 78
title: "section leaders must render the project identity, not mba22:main; rects only on collapsed leaders"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-12T08:21:47Z
closed: 2026-06-12T09:05:36Z
url: https://github.com/gerchowl/herdr/issues/78
---

# section leaders must render the project identity, not mba22:main; rects only on collapsed leaders

## Live regression vs #62 spec + one refinement (post-fd0dd50 dogfood, screenshot in chat)

1. **Section leaders render member grammar instead of the project identity.** Spec (#62): space row = the gh-origin-shared identity (`owner/repo` per #27), members = `<server>:<target>`. Shipped: leaders read `mba22:main` / `mba22:dompt` — the PROJECT NAME is gone; two different repos both head as `mba22:main`, distinguishable only by children. Fix: the leader label = project identity (owner/repo when project_key resolves; the repo/dir display label as fallback) — NEVER server:branch. The leader stays the selectable main-checkout row (selection/close semantics unchanged); ahead/behind + PR glyph for the main checkout may stay on the leader line. Members keep `<server>:<target>`.
2. **Packed rects only where they inform**: on EXPANDED leaders the group-join `▮▮` duplicates the member icons right beneath — drop them there; keep on COLLAPSED leaders (members hidden → the aggregate is the only signal) and wherever a row summarizes hidden state. Hollow ▯ no-agents marker follows the same rule.

## Acceptance
- Leader rows: `<icon> <owner/repo|label> [↑↓ main's git info] [#PR]` — no server:branch on leaders.
- `mba22:main`-style labels appear ONLY on member rows.
- Rects: collapsed leaders yes, expanded leaders no; tests for both.

## References
#62 (spec + comments), PR #74 (the rebase that intent-merged head/label), src/ui/grammar.rs, sidebar leader render.

---

## Comments

### gerchowl — 2026-06-12T08:24:42Z

## Supersede (user): retire packed rects ENTIRELY

Collapsed leaders already render the colored digit counts — which carry presence AND magnitude. So `▮▮` is redundant in every position:
- expanded leaders: member icons beneath carry the state (as filed)
- collapsed leaders: the digit counts carry it (`<icon> <owner/repo> 2 1`)

Remove the packed-rect rendering everywhere (keep/retire `packed_rects` fn per test usage; no render path uses it). The hollow ▯ no-live-agents marker is also superseded — the muted state icon already says it. One signal per fact: icon = worst state, digits = counts.

### gerchowl — 2026-06-12T09:05:36Z

Shipped in PR #81 + the supersede commit: leaders render owner/repo identity (never server:branch), and rect/hollow glyphs are retired everywhere — icon = worst state, digits = counts.

