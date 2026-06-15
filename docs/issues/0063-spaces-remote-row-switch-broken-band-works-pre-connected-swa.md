---
number: 63
title: "spaces remote-row switch broken (band works); pre-connected swap + top-right failure toast"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-11T18:46:56Z
closed: 2026-06-12T15:10:45Z
url: https://github.com/gerchowl/herdr/issues/63
---

# spaces remote-row switch broken (band works); pre-connected swap + top-right failure toast

## Bug (live, post-e82dea1 deploy)

Clicking a **folded remote workspace row in the spaces list** (`sage:main` under a project) does not switch — the screen "blips to the terminal" and returns/lands wrong. Clicking the **server row in the band works** (switch + attach to sage succeeds), so transport/protocol/legs are healthy.

Diagnostics so far:
- Both machines `0.6.8-fork.e82dea1` proto 15.
- The local server log shows **no switch/peer events** for the remote-row clicks → the click likely never emits the SwitchServer request: suspect the spaces remote-row hit path after #51 (RemotePeerRef + ws_idx targeting) and/or #56's row restructure (entry/hit-area mapping for remote member rows). The band path (#39) emits fine.
- The "blip" = the client exit→relaunch leg becoming user-visible (and on failure it bounces back or strands at the shell).

## Fixes wanted

1. **The bug**: remote workspace rows in the spaces list must emit the same switch the band rows do (carrying the target workspace for post-attach focus). Add a test that clicks a folded remote row and asserts the SwitchServer emission (the band has one; the spaces path apparently doesn't or it regressed).
2. **Seamless switch (no blip)** — pre-connected swap: establish the new leg (ssh + handshake + first frame) IN THE BACKGROUND while the current view keeps rendering; only swap the painter when the new server's first frame arrives. (User: "can't we rayon it" — concurrency at the launcher/client level, not literally rayon.)
3. **Failure surfaces top-right**: when a switch fails (leg dies, handshake rejected, timeout), NEVER strand at the terminal — return to the previous server view and render the failure as the existing top-right notice/toast (action_notice machinery): `switch to sage failed: <reason>`.

## Sequencing
Bug fix (1) is dispatchable immediately after the in-flight band-polish PR merges (same sidebar/mouse surfaces). (2)+(3) are the switch-UX hardening — same PR or follow-up, they touch main.rs legs + client, disjoint from the sidebar.

## References
#39 (SwitchServer legs), #51 (snapshot folding + RemotePeerRef), #56 (row restructure), #38/PR #52 (attach retry — the failure-path plumbing to reuse).

---

## Comments

### gerchowl — 2026-06-12T15:10:44Z

Superseded/shipped piecewise: the click-path regression tests + frozen-frame + failure notice landed in PR #67; the pre-connected swap was replaced by the strictly better #65 connection slots (PR #76); the cancel story completes via the #93 popup (spike consolidated, implementation in flight).

