---
number: 68
title: "tracking: federation v2 — spoke parity, restyle v3, connection slots"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-11T20:34:08Z
closed: 2026-06-12T00:18:46Z
url: https://github.com/gerchowl/herdr/issues/68
---

# tracking: federation v2 — spoke parity, restyle v3, connection slots

Tracking issue for the federation-v2 arc — LANDED 2026-06-12.

## Sequence — all shipped

- [x] #63 — switch UX: frozen-frame handoff, failure→top-right notice, click regression tests (PR #67, proto 16)
- [x] #66 — origin summary in the down-gossip: spokes see the hub's spaces, home-sentinel + focus_workspace targeting (PR #73, proto 17; changeset recovered from a stalled agent's transcript, 46/46 ops)
- [x] #62 — restyle v3: identity headers, `<server>:<target>` grammar, agent icons replace circles, single-row agents incl. remote, switch_space(N), close-main-keeps-the-space (PR #74; rebase over six trains caught + fixed a byte-slice truncation panic that zero-framed spokes)
- [x] #65 — connection slots stage 1: multi-connection client, warm flips without relaunch, SetFrameSubscription pause/resume (PR #76, proto 18, `[slots] enabled=false` opt-in; architecture review BLOCKED v1 with three real wiring bugs — slot-tagged events, apply-time stale-frame drop, explicit bridge-socket plumbing — all fixed before merge)

Shipped en route: #69→PR #72 (unconditional terminal restore + refusal-masking root cause), #70→PR #71 (numpad KP codes), hotfix pin bfde089.

## Follow-ups
- #75 — slots stage 2: live status over paused slots, retire the ssh summary poller + snapshot staleness, full cold-fleet bridging
- #57 — strip overflow test + section-key memoization nits

Final pin: fd0dd50 (proto 18). Deploy = lockstep apply + live-handoff on both Macs; flip `[slots] enabled = true` to dogfood the pointer-flip switching.

---

## Comments

### gerchowl — 2026-06-12T00:18:45Z

Arc complete — all four constituents merged (PRs #67, #73, #74, #76), final pin fd0dd50 (proto 18). The review gates earned their keep twice over: the architecture round blocked three real wiring bugs in the slots client, and the #74 rebase surfaced a spoke-render panic. Stage 2 lives in #75.

