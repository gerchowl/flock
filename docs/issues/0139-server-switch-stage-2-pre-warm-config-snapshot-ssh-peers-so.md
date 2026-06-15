---
number: 139
title: "Server switch (stage-2): pre-warm config/snapshot SSH peers so their switches are instant"
kind: issue
state: CLOSED
author: gerchowl
labels: ["enhancement"]
created: 2026-06-14T13:30:00Z
closed: 2026-06-15T00:20:53Z
url: https://github.com/gerchowl/herdr/issues/139
---

# Server switch (stage-2): pre-warm config/snapshot SSH peers so their switches are instant

Follow-up to #134 (option 2). The timeout-bounded fallback (option 1) landed in #138; this tracks the larger remaining work.

## Problem

The background warm sweep (`src/client/mod.rs`) only warms slots that have a `slot_socket_path` — in stage-1 that is just Home + the active leg. **Config-peer and snapshot-peer slots are never pre-warmable**, so switching to them always pays the cold-dial cost (one probe SSH + bridge dial, even after #133’s collapse). A warm flip, by contrast, is instant and in-process.

## Idea

Extend the warm sweep so config/snapshot SSH peers can be background-dialed like Home: stand up their `SshStdioBridge` ahead of time (subject to a concurrency/cost budget and the same `SWITCH_DIAL_DEADLINE` bounding), so `flip_to` returns a live stream and the switch is instant.

## Considerations

- Cost: each warm SSH peer holds an open ssh bridge — needs a cap and probably an idle-eviction policy.
- Reuse the ssh ControlMaster/keepalive config already generated for bridges.
- Must not stall the 2s sweep tick — pre-warm dials run detached, same as `spawn_switch_dial`.

Context: branch `server-switch-slow`. Depends on the now-merged #132 / #133 / #138.

---

## Comments

### gerchowl — 2026-06-15T02:00:50Z

Deferred idle-eviction of unused warm ssh bridges (large-fleet-only concern) tracked in https://github.com/gerchowl/herdr/issues/152.

