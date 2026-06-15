---
number: 81
title: "fix(sidebar): leaders render the project identity; rects only when collapsed (#78)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-12T08:52:45Z
closed: 2026-06-12T09:05:25Z
merged: 2026-06-12T09:05:25Z
base: feat/sidebar-row-gap
head: leader-identity
url: https://github.com/gerchowl/herdr/pull/81
---

# fix(sidebar): leaders render the project identity; rects only when collapsed (#78)

Fixes #78 — a live-dogfood regression against the #62 spec, found minutes after the #74 sidebar restyle deployed.

## What was mislabeled where
Section-leader rows rendered the **member** grammar (`mba22:main`), erasing the project name — two different repos both headed as `mba22:main`, distinguishable only by their children. The leader is the selectable main-checkout row, so it took the same `local_member_label` every member did.

## Fix 1 — leader label = project identity, never `server:branch`
- New `grammar::leader_label`: `owner/repo` from the project key (#27's `project_identity_label`), falling back to the workspace **display label** when the git identity hasn't resolved. Never `<server>:<branch>`.
- Sidebar render selects `leader_label` for the section head (`group_key.is_some()`), `local_member_label` for everyone else. Both label forms stay in `ui::grammar` (one place).
- The leader STAYS the selectable main-checkout row — selection / close-main / triangle / two-level-highlight semantics untouched; main's ahead/behind + PR glyph still ride the leader line.
- Remote-only **group** leaders collapse to the bare project identity too (no `· <host>:<branch>` tail). A project's **lone** remote checkout keeps `project · <host>:<branch>` so its branch stays visible (it is both the project and its only checkout, like a solo local section).

## Fix 2 — packed rects only on COLLAPSED leaders
- Expanded leaders drop the group-join `▮▮` (and the hollow `▯` no-agents marker): the member icons rendered right beneath already carry that aggregate, so the leader's rects only duplicated them.
- Collapsed leaders keep the rects + digit counts (members hidden → the aggregate is the only signal). Individual member rows keep the existing multi-class rule.

## Tests
- `ui::sidebar` (78) + full unit suite: **2131 passed, 0 failed, 1 ignored**. Clippy `-D warnings` clean.
- Updated the prior expanded-leader-carries-rects assertions to the inverse (#78), and the remote-only-leader label assertion to the bare project identity; added: leader shows `owner/repo` (resolved) and never `server:branch`, fallback to display label when unresolved, solo-remote keeps `project · member`, rects present collapsed / absent expanded for both leader kinds (local-main-headed and worktree-headed after main closes).
- `--test live_handoff`: **16 passed**. `--test peer_federation`: **7 passed** (incl. `switch_snapshot_renders_home_row_on_spoke_and_home_switches_back`, which #74 flagged pre-existing-failing — green here; the two leader-identity assertions key on the project substring my change preserves).

Do not merge.
