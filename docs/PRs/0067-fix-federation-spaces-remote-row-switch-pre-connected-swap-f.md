---
number: 67
title: "fix(federation): spaces remote-row switch + pre-connected swap + failure notice (#63)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-11T20:29:03Z
closed: 2026-06-11T20:29:54Z
merged: 2026-06-11T20:29:54Z
base: feat/sidebar-row-gap
head: switch-fix
url: https://github.com/gerchowl/herdr/pull/67
---

# fix(federation): spaces remote-row switch + pre-connected swap + failure notice (#63)

Fixes #63.

## Part 1 — the live bug (spaces remote-row switch)

Traced the spaces remote-row click end to end: `mouse.rs` (`remote_card_areas` hit-test, line ~576) → `RemotePeerRef::switch_request` → `request_peer_switch` → headless loop → `prepare_switch_server` → `ServerMessage::SwitchServer`.

**Finding:** on this tip (post-#64) the path is structurally sound — a FOLDED config-peer row (indented under a local project block) emits `SwitchServer` correctly at both unit and e2e levels. #64 (band-polish) did not touch the spaces hit-test, and the spaces path was unchanged since #46. The diagnosis built a faithful reproduction (servers A+B sharing one repo origin so B's row folds INTO A's local block as an indented member) and confirmed the click yields `SwitchServer { target: peerb }`. The reported "blip to terminal and land wrong" with "no switch events" is consistent with the switch firing but the **leg failing invisibly** (parts 2/3) rather than the click never emitting.

**Locked in** with regression tests so any future regression in the folded spaces hit-path is caught:
- `folded_remote_member_row_click_switches_server` (e2e, `tests/peer_federation.rs`) — clicks the indented folded member row, asserts `SwitchServer`.
- `folded_config_peer_row_click_requests_switch` (unit, `mouse.rs`) — config-peer row folded under a matching local project asserts `ConfigPeer` emission.

## Part 2 — pre-connected / seamless swap (no blip)

The switching client now **holds the alternate screen + raw mode** on exit (`SWITCH_HANDOFF_PENDING`): the last frame stays frozen while the launcher establishes the next leg (ssh + handshake + first frame in `run_remote`), so the host shell never flashes. The next leg's `ratatui::init()` re-enters (idempotent) and paints over the frozen frame. Each leg clears the flag on start; the launcher `force_restore_host_terminal()`s if a held chain ultimately dies with nothing left to reclaim the screen. Least-invasive — no change to the subprocess/leg model, and the #52 retry window is untouched.

## Part 3 — failure surfaces top-right (never strand at a shell)

Proto **v16** adds `Hello.notice`. On a switch leg that fails to establish, the launcher re-attaches the **previous** leg carrying `switch to <name> failed: <reason>` via `HERDR_SWITCH_NOTICE`; the client lifts it into the Hello (one-shot, cleared so a #38 handshake retry doesn't repeat it), and the server raises it as the existing top-right `action_notice`. The leg-loop decision is extracted into a pure, unit-tested `decide_next_leg`.

Tests: `app_client_attach_notice_surfaces_as_action_notice`, `terminal_attach_client_ignores_notice`, `take_attach_notice_is_one_shot`/`_ignores_empty`, `decide_next_leg_*`, `switch_failure_*`. All Hello encoders + handshake version asserts bumped 15→16.

## Test results
Full suite green (`--test-threads=4`), `--test peer_federation` 7/7, `--test live_handoff` 16/16; fmt + clippy clean. (The known flaky `app::tests`/`headless` config/startup family fails only under parallel load and passes single-threaded — pre-existing, unrelated.)
