---
number: 152
title: "Server switch (stage-3): idle-evict unused warm ssh bridges (large-fleet only)"
kind: issue
state: OPEN
author: gerchowl
labels: ["enhancement"]
created: 2026-06-15T02:00:49Z
closed: 
url: https://github.com/gerchowl/herdr/issues/152
---

# Server switch (stage-3): idle-evict unused warm ssh bridges (large-fleet only)

Follow-up to #139 (pre-warm config/snapshot ssh peers, landed in #146).

## Scope gate

This only matters at **large fleet size**. For a personal fleet (a handful of peers) the current behaviour is fine and this is not worth building — `#146` already bounds warm bridges by `[slots] max` (default 8) and dead bridges self-demote via `COLD_REDIAL_BACKOFF`. File/act on this **iff** someone runs a fleet big enough that holding `max` idle ssh bridges open is an actual resource problem.

## Problem (at scale)

With `prewarm_ssh_peers` on, the warm sweep stands up an `SshStdioBridge` for every reachable peer up to `[slots] max`. Each warm peer holds an open ssh connection for the whole session even if the user never switches to it. On a large fleet that is many idle ssh processes + sockets that never get reclaimed until the slot dies or the session ends.

## Idea

Add an idle-eviction policy: demote a warm (non-active) ssh slot whose bridge has been idle / unused for longer than some window, freeing the ssh connection; the next sweep (or a switch) re-warms it on demand. Considerations:
- Never evict home or the active slot.
- Eviction must tear the bridge down **off-loop** (`SshStdioBridge::Drop` joins its listener and can block) — same discipline as the #146 race guard.
- Likely a new `[slots]` knob (e.g. `warm_idle_evict_secs`, 0 = never) so small fleets keep everything warm.
- Track "last used" per slot (last flip-away time) in the registry.

## Acceptance

On a large fleet, warm ssh bridges that go unused for the configured window are reclaimed and transparently re-warmed on next need, with no impact on home / the active session.

Context: branch `server-switch-slow`. Depends on #146.
