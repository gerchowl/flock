---
number: 75
title: "slots stage 2 — live status over paused slots, retire the ssh summary poller + snapshot staleness"
kind: issue
state: OPEN
author: gerchowl
labels: []
created: 2026-06-11T23:23:46Z
closed: 
url: https://github.com/gerchowl/herdr/issues/75
---

# slots stage 2 — live status over paused slots, retire the ssh summary poller + snapshot staleness

Follow-up to #65 (stage 1 landed the slots core: registry, dial/pause/flip protocol, warm-all dialing, pointer-flip switching). Tracked under #68 (federation v2).

## Stage 2: the unification payoff

Stage 1 holds one warm framed connection per fleet server, but a PAUSED slot currently carries nothing while it is not active. The locked design (#65 economics + unification comment) is that these same connections absorb the other two subsystems:

1. **Live status over paused slots.** A paused slot should carry a lightweight status stream (workspace/agent summary) instead of frames, so the sidebar reflects each peer LIVE rather than from a 15s poll. New additive `ServerMessage` (status payload) gated on the paused subscription, or a `SetFrameSubscription` mode that swaps frames for status.
2. **Retire the ssh summary poller (#66 lineage).** The hub holds persistent ssh transports purely for the 15s `peers summary` poll today. Once paused slots carry status, the poller is redundant — one connection per peer carries status while paused and frames while active. Net new infrastructure: none.
3. **Retire snapshot staleness.** The carried `FleetSnapshot` (#73, #66) is a once-at-attach handoff whose freshness only decays. With live status over the slot, the snapshot stops being a staleness source — the data goes live (#66 data).
4. **Attention-driven warming for cold/down/big-fleet tails.** When status gossip reports an agent on server X entering blocked/wants-input, pre-dial X (likeliest next leap), keyed to the existing `focus_attention` ranking (blocked > done-unseen). Warm-all fleets get it for free; this is the policy for the cold tail beyond the `[slots] max` cap.

## Notes carried from stage 1
- Stage 1 warms slots reachable as a local socket (home + peers whose ssh-stdio bridge is live). Background bridge bootstrap for not-yet-bridged peers (probe/install/forward off the active paint path) is part of making warm-all reach the full fleet and belongs here or in a sibling.
- `[slots] enabled` ships defaulting FALSE in stage 1; flip to TRUE once stage 2 makes the slots the sole status/frame/snapshot path.

Refs: #65, #66, #68.
