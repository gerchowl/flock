---
number: 72
title: "fix(client): unconditional terminal restore + visible progress on the held screen (#69)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-11T22:22:06Z
closed: 2026-06-11T22:29:25Z
merged: 2026-06-11T22:29:25Z
base: feat/sidebar-row-gap
head: hold-fix
url: https://github.com/gerchowl/herdr/pull/72
---

# fix(client): unconditional terminal restore + visible progress on the held screen (#69)

## #69: reconnect after handoff — idle hang behind frozen frame + unusable shell

After a live-handoff reattach, a client that inherited a **held** host terminal (alt-screen + raw mode, a frozen frame from the previous leg's seamless switch) could:
1. sit in the #52 retry window with **no visible progress** behind the frozen frame (perceived hang), and
2. be left **unusable** if the user ctrl-c'd the "hung" client — the held-terminal exit path missed restore.

### Fix
- **Unconditional restore (outcome 2).** A `HeldRestoreGuard` covers the held-terminal window of a leg, armed when the leg holds the terminal itself (`SWITCH_HANDOFF_PENDING`) or inherited a hold from a previous leg (new `HERDR_TERMINAL_HELD` env, set by the launcher in `run_attach_legs`). Any abnormal exit — error return, ctrl-c/SIGTERM/SIGHUP, panic, or the `std::process::exit` paths that skip `Drop` — reclaims the host terminal (leave alt-screen, raw off, pop kitty flags, reset modifyOtherKeys). A clean handoff into the next leg disarms it so the no-blip switch (#63/#67) is preserved.
- **Visible progress (outcome 1).** The live-handoff retry status is painted as a cursor-parked bottom-row overlay **on the held screen** (`\x1b7\x1b[9999;1H…\x1b8`) with a live elapsed-seconds counter, so the wait reads as progress, not frozen silence. Plain in-leg reconnects keep the in-place stderr line.
- **Refusal not masked by a dying server.** `set_read_timeout(None)` after a Welcome is in hand can EINVAL when a mid-handoff server half-closes the socket right after the refusal; that failure used to surface as a bare `ConnectionFailed`, defeating the retry classification. It is now ignored so the refusal drives the retry.

### Tests
- New unit tests: hold-flag tracking, `HeldRestoreGuard` drop reclaims an inherited hold, a disarmed guard keeps the hold for the next leg, and the elapsed-seconds status counter.
- New e2e `killing_a_client_mid_held_handoff_restores_the_terminal`: a refusing stand-in server drives a real `herdr client` (with an inherited hold) into the retry window; asserts (a) visible held-screen progress overlay with the elapsed counter, (b) full terminal restore sequence on SIGINT. The stand-in holds refused connections open to mirror a real mid-handoff server.
- Green: `client_mode` (17), `live_handoff` (16), `peer_federation` (7), and the full `--bin herdr` unit suite (2075). #67 no-blip and #52 retry semantics preserved.

Built/tested entirely in the devShell (sccache); commit passed guardrails gates.
