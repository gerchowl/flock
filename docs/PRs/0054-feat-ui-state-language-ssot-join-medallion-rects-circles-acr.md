---
number: 54
title: "feat(ui): state-language SSoT — join + medallion/rects/circles across servers, spaces, workspaces (#42)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-11T11:20:03Z
closed: 2026-06-11T11:21:08Z
merged: 2026-06-11T11:21:08Z
base: feat/sidebar-row-gap
head: state-ssot
url: https://github.com/gerchowl/herdr/pull/54
---

# feat(ui): state-language SSoT — join + medallion/rects/circles across servers, spaces, workspaces (#42)

Implements #42 (body + all spec comments: leading circle + font-true digits, the design ladder, ring-join semantics, spaces adopt the join, concentric-preferred-over-bars).

## The SSoT core — `src/ui/state_signal.rs`

- **One severity→color mapping**: `StateClass` (red blocked > teal done-unseen > yellow working > green settled idle > muted none). Its derived `Ord` IS the attention priority — the former `pane_attention_priority` (workspace/aggregate.rs) and `workspace_attention_priority` (sidebar.rs) duplicates consolidate into it, as do `state_dot`, `state_label_color`, `agent_icon`, and `remote_status_dot` (now pure consumers).
- **One join fn**: `join_states` → severity-sorted top-3 multiset. Repetition meaningful (`r·r·y` = two blocked + one working); <3 live states → shorter join; empty → none.

## Three renderings of the join

- **Servers band (2-line rows)**: the `ring_medallion` is now the leading mark — rings = the row's join outer→inner (self row joins all local pane states; peer rows join their workspace statuses), `base_bg` = the row's actual fill (highlight-aware, asserted in tests). Empty join on a reachable row renders a single muted ring ("muted none" presence). Unreachable peers keep the compact `unreachable {age}` form with a **muted hollow dot** (shape = reachability). The circled-count rollup chips are gone. New `[ui] medallion_style = "sextant" | "quadrant"` (sextant default; live-reloadable; documented in configuration.mdx).
- **Single-line rows**: packed rects `▮▮▮` of the join — **group join on space header rows, including the expanded primary row** (the "main row has no traffic light" fix), own join on member/standalone workspace rows; hollow `▯` muted = no live agents. Leading circles keep today's seen/unseen shape semantics (`●`/`○`/`·`, unseen-done stays teal), colored via the mapping.
- **Counts**: collapsed-group traffic lights migrate from `❶❷` dingbats to plain colored digits in the terminal font (`2 1` red/yellow); `circled_count` deleted outright — a buffer-scan test asserts no U+2776..=U+277F renders anywhere.

## PR/gh status on rows (issue item 1)

- Rows with cached PR state render the compact `#N ⊙/◐/✓/✗` glyph (extracted from the pane-header HUD into the shared module; panes.rs consumes it): on the branch line after ahead/behind for two-line rows, inline after the label for one-line member rows.
- **Poll scope**: `PrStatePollDue` was not focused-only but it WAS linked-worktrees-only — primary checkouts and standalone repos never got PR state. Widened to all workspaces with a branch (worktree membership or live git metadata for repo root), **deduped to one `gh` call per (repo, branch)** with the result fanned out to every workspace on it — same 120s cadence, same single off-thread worker, so the budget grows only by the number of distinct branches.
- Documented silence-means-synced at the ahead/behind site (issue item 1a) rather than adding a synced chip — noise at sidebar density.

## Agents panel (item 6)

Keeps its richer ✓/spinner/◉ icon set; colors now flow from the mapping (asserted for blocked/working/done/idle).

## Judgment calls

- **Done-unseen outranks working** in the severity sort, preserving the codebase's existing aggregate semantics (`aggregate_state_done_unseen_beats_working` test) over the coarse r>y>g ladder wording in the comments — the comments never addressed teal, and the spec says "keep seen/unseen semantics where they exist today".
- Remote (peer-workspace) sidebar rows keep just the leading circle, no trailing rects: peer summaries carry one status per workspace, so their join is degenerate; the peer's multiset renders on its server-band medallion instead.
- Collapsed group rows show rects (presence) AND plain-digit counts (exact counts matter when members are hidden); expanded primary shows rects only.
- Home row (way back to origin) keeps its `←` form — it is a navigation row, not a server-state row.

## Tests

2027 passed, 0 failed, 1 ignored (the pre-existing medallion eyeball demo); fmt + clippy clean. New coverage: join multiset sort/top-3 cap/empty/none-filter; medallion ring colors + bg compose on rendered band rows (sextant + quadrant config); packed rects on collapsed/expanded-primary/member/standalone rows incl. hollow; plain digits + no-dingbat buffer scan; PR glyph on two-line and one-line rows; agent icon/dot/label color consolidation; `pr_poll_targets` linked/primary/standalone coverage + dedupe.
