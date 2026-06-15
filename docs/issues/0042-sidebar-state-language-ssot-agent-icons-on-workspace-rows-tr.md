---
number: 42
title: "sidebar state-language SSoT: agent icons on workspace rows, traffic light on primary rows, PR/gh status everywhere"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-11T08:18:39Z
closed: 2026-06-11T11:53:29Z
url: https://github.com/gerchowl/herdr/issues/42
---

# sidebar state-language SSoT: agent icons on workspace rows, traffic light on primary rows, PR/gh status everywhere

## Motivation (live dogfood screenshot)

Workspace rows speak a poorer state language than the agents panel:

1. **gh status is inconsistent across rows** — some show `↓N` ahead/behind, most show nothing. Two causes to untangle: (a) rows that ARE in sync render nothing (correct but reads as "no data" — needs an explicit subtle synced state or tolerable silence), (b) **PR state (`#N ⊙/◐/✓/✗`) renders only in the pane header HUD, never on sidebar rows** — rows with an open PR should carry the PR glyph.
2. **Primary/main rows lack the traffic light** — the group's aggregate state (the #17 traffic-light counts) renders only on COLLAPSED group headers; the expanded primary row shows just a plain dot. The primary row should always carry the aggregate state of its group.
3. **State iconography is duplicated, not shared** — the agents panel has the rich set (green checkmark = done, yellow animated spinner = working, red escalation = blocked); workspace rows use plain `○/●` dots. **SSoT**: one state→symbol/animation mapping (the agents' set) consumed by BOTH the agents panel and the workspace rows:
   - ✓ green — done/idle-unseen
   - animated spinner ("snake") yellow — working
   - red circle/escalation — blocked

## Scope
- Extract the agent-panel state icon/animation mapping into one shared fn (the existing `state_dot`/agent icon code paths converge).
- Workspace rows (parent + members) adopt it; primary rows show group-aggregate state.
- PR-state glyph on any row whose workspace has cached PR state (data already polled per the 120s PrStatePollDue cycle — verify scope: focused-only vs all-workspaces; widen to all if needed, keeping the off-thread gh calls budgeted).
- ahead/behind arrows stay; consider a minimal synced indicator (or document silence-means-synced).

## Sequencing
After #41 (chrome rework, same file in flight); natural companion to #33's row restructure (two-level highlight + member tab-strip) — could land as its first commit.

## References
Screenshot in chat 2026-06-11; #17 traffic lights (`space_state_counts`/`circled_count`), agents panel icons (src/ui/sidebar.rs agent panel + src/ui/status.rs `state_label`/agent icon), PR poll (`PrStatePollDue`, api.rs chokepoint), #33/#41.

---

## Comments

### gerchowl — 2026-06-11T08:49:59Z

## Scope addition (user): server-row leading state circle + font-true counts

- The agent-state rollup moves INTO the leading circle before the server name (no trailing traffic chips): shape = reachability (`○` unreachable / `●` reachable), color = worst agent state present (red blocked > yellow working > green done/idle > muted none).
- The `❶❷` dingbat circled digits are replaced — they render from a fallback font. Counts become PLAIN DIGITS in the terminal font, colored per state class, rendered only for non-zero classes: `● herdr 2 1` (red 2, yellow 1).
- Alternative considered (user's 'target'): three adjacent dots severity-sorted outer→inner, green-padded (`⏺⏺⏺` = presence-only encoding, no counts) — terminal cells are single-color so concentric-in-one-glyph is impossible; keep as fallback aesthetic if digit counts prove noisy.
- Same mapping feeds workspace rows (this issue's SSoT core) — circle shape there = seen/unseen where applicable, color = state; circled_count dingbats retire everywhere (collapsed-group traffic lights migrate to colored plain digits too).

Sequencing unchanged: after PR #44 merges (same rows in flight).

### gerchowl — 2026-06-11T08:55:16Z

## State-indicator design ladder (user, final)

**Primary — concentric medallion** ('concentric is nicer'): the ring-medallion helper (PR in flight from branch `ring-medallion`) renders 1–3 concentric rings, severity-sorted OUTER→INNER, in sub-cell blocks across the two-line row (2 cells × 2 lines). Unreachable = hollow outline ring in muted color, no fill.

**Fallback — packed rectangles** (simple, single-line capable): one to three closely-packed `▮` rectangles for whatever state classes exist (severity-sorted), e.g. blocked+working+done = red▮yellow▮green▮, working+done only = yellow▮green▮. **Hollow `▯` = no connection** (single hollow rect replaces the group). 1–3 cells, pure terminal font, zero rasterization.

Both encode presence (not counts); exact counts stay available as the colored plain digits after the name (earlier comment). Integration picks medallion where the two-line row exists (servers band), packed-rects where rows are single-line (workspace rows), keeping one severity→color mapping (the SSoT core of this issue) feeding all three renderings: medallion rings, packed rects, leading circle.

### gerchowl — 2026-06-11T09:19:16Z

## Ring semantics (user, refines the ladder comment)

Rings = the **top-3 of the severity-sorted MULTISET (join) of the server's individual agent states**, outer→inner — NOT presence-padded. Repetition is meaningful:

- `r·r·r` = ≥3 blocked · `r·g·g` = one blocked among done · `y·y·y` = all working · `y·g·g` = top-3 of [y,g,g,…]
- fewer than 3 agents → fewer rings (2 rings / single dot) — ring count itself signals scale, capped at 3
- unreachable = hollow outline (unchanged)

Same join feeds the packed-rect fallback (top-3 rects) and the leading-circle color (= the join's head). One severity-sort, three renderings.

### gerchowl — 2026-06-11T09:32:21Z

## Confirmed (user): the join signal applies to SPACES too — one system, all levels

server (2-line row) → medallion · space/primary/collapsed-group + workspace rows (1-line) → packed-rects ▮▮▮ of the same top-3 join · hollow ▯ = unreachable (servers) / no live agents (spaces, workspaces). Repetition meaningful at every level (r·r·g on a space = two blocked members). Dingbat circled counts retire everywhere; colored plain digits remain where exact counts matter (collapsed groups). One severity→color mapping + one join fn feeding medallion, rects, and leading circles — that's the SSoT deliverable of this issue.

### gerchowl — 2026-06-11T10:25:37Z

## Refinements (user)

**Count badges — final form**: counts LEAD the name in fixed severity columns (r y g), zeros included:
`0 2 1 herdr <ping>` — zeros grey/muted, non-zero counts as **plain colored text** (user considered colored-bg badge chips with contrast text, then settled: just colored text). Unreachable: no-conn marker in the ping slot + the whole line muted/italic.

**Medallion geometry — RECTANGULAR, not rounded**: nested rectangle borders, NO corner-rounding (corners belong to the outer band):
```
r r r r r r
r y y y y r
r y g g y r
r y g g y r
r y y y y r
r r r r r r
```
Band widths outer→core = 1 1 2 1 1 (sums to 6 — exact fit on the 6×6 canvas): outer perimeter width 1, middle width 1, core 2×2. This REPLACES the corner-base/rounded spec: the medallion is a solid 3-cell×2-line rectangle; no compose-with-row-fill corner pixels needed. (Simplifies rasterization: pure chebyshev nested squares; only the vertical-center column cells still cross 3 colors → election, innermost wins fg.)

### gerchowl — 2026-06-11T11:53:28Z

Shipped in PR #54 + #55 (medallion v2 rectangles).

