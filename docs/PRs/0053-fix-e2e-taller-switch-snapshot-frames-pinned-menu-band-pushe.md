---
number: 53
title: "fix(e2e): taller switch-snapshot frames — pinned menu band pushed the folded remote row below the 30-row fold"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-11T10:54:58Z
closed: 2026-06-11T11:14:33Z
merged: 2026-06-11T11:14:33Z
base: feat/sidebar-row-gap
head: basefix
url: https://github.com/gerchowl/herdr/pull/53
---

# fix(e2e): taller switch-snapshot frames — pinned menu band pushed the folded remote row below the 30-row fold

## Regression

`switch_snapshot_renders_home_row_on_spoke_and_home_switches_back` (tests/peer_federation.rs) times out on the integration tip waiting for the hub's folded remote workspace row (`"proj · "`) at 90×30.

## Root cause: 866ad0b (PR #44), pure frame geometry — not a rendering bug

The pinned 2-row menu band carves `SIDEBAR_MENU_BAND_ROWS` off the sections **before** the spaces/agents split (`expanded_sidebar_sections`). At height 30 that turns the math from "fits exactly" into "one row short":

| | pre-#44 | tip |
|---|---|---|
| sections height | 30 | 28 (menu band −2) |
| ws section (0.5 split) | 15 | 14 |
| servers band (3 slots × 2 lines + header + divider = 8, clamped to half) | 7 | 7 |
| spaces list / body (−2 header, −1 `new` footer) | 8 / **5** | 7 / **4** |
| content: alpha card (2) + row gap (1) + remote row (1) + trailing gap (1) | 5 ✔ | 5 ✘ |

This test (unlike its passing sibling) has **two** config peers (`peerb` + down `ghost`), so the servers band wants 3 two-line slots and hits the half-height clamp. The trailing remote-only `proj` row lands below the fold; the timeout screen dump shows exactly that: the alpha card, blank gap rows, and a scrollbar — the row is scrollable into view, not wrongly hidden. `fold_remote_entries`, `remote_peers()` (#51) and the scope/server filters (#44) all behave correctly; folding is unit-covered.

(#50 and #51 are exonerated: the test, added in a6c28b0/#36, breaks at 866ad0b's geometry alone.)

## Fix

Test-only: grow this test's hub and spoke frames from 90×30 to 90×45 so the grown sidebar chrome plus the spaces list fit on screen (the spoke phase needs it too — at 30 rows the clamped band also cuts the third server row, `ghost`). Comment in the test records the geometry reasoning.

## Verification

- `cargo test --test peer_federation` — 6/6 pass (failing test now 15.8s, was 58s timeout)
- `cargo test --test live_handoff`, `--test server_headless` — pass
- unit suite (`--lib --bins`) — pass
