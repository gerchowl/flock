---
number: 102
title: "slots switch drops the hub fleet snapshot -- spokes lose the servers band/home row"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-13T08:27:59Z
closed: 2026-06-13T08:59:54Z
url: https://github.com/gerchowl/herdr/issues/102
---

# slots switch drops the hub fleet snapshot -- spokes lose the servers band/home row

## Bug (live, slots-enabled): hub fleet snapshot dropped on switch -> spokes have no servers band / no home row

`[slots] enabled = true` broke hub->spoke gossip. In src/client/mod.rs the SwitchServer arm:

```rust
if let Some(manager) = slot_manager.as_mut() {
    let _ = &focus_workspace;
    let _ = &fleet;          // <-- discards the hub's freshly-generated snapshot
```

The legs path threads `fleet.as_ref()` (the SwitchServer's snapshot) into the next leg. The slots dial/flip paths instead rely on `do_handshake` -> `carried_fleet_snapshot()`, which reads the HERDR_FLEET_SNAPSHOT env -- EMPTY on the hub (the origin generates the snapshot server-side; it carries none). So the slots cold-dial/warm-flip hands the spoke `fleet: None` -> spoke renders no servers band, no home row, no way back.

## Root cause
The snapshot is SERVER-generated and delivered in SwitchServer; the legs path was the only consumer threading it through. Slots (#65/#93) bypass the launcher/env and never wired the SwitchServer fleet into the dial.

## Fix
1. **Cold dial (spawn_switch_dial)**: take `fleet: Option<FleetSnapshot>`, pass to `do_handshake` (new explicit param overriding the env read). Wire the SwitchServer `fleet` in at the call site (remove `let _ = &fleet`).
2. **Warm flip**: harder -- warm slots were pre-dialed (at startup, before any switch) with no hub snapshot. On flip to a spoke, PUSH the current snapshot to it: new additive `ClientMessage::FleetSnapshotUpdate { snapshot }` the server applies to that client's view (protocol bump). The hub client holds the latest snapshot (from its own server) to send.
3. Warm-all pre-dial: a spoke pre-dialed by the hub gets the snapshot via the push on flip, or eagerly right after the warm handshake completes.

## Acceptance
- With `[slots] enabled`, switching hub->spoke renders the spoke's servers band WITH the home row + the hub's folded spaces. (THE test that was missing -- see #103.)

## References
#65/#76 (slots), #93 (cold-dial popup), #39/#66/#73 (snapshot/home-row), src/client/mod.rs SwitchServer arm + spawn_switch_dial + do_handshake.

---

## Comments

### gerchowl — 2026-06-13T08:59:53Z

Cold-dial path fixed in PR #104 (do_handshake fleet override, threaded from the SwitchServer arm; resolve_handshake_fleet unit-tested red->green). Slots stays DISABLED until fix-2 (warm-flip snapshot push) + the full slots-client e2e (#103) land -- then re-flip behind the test, not before it.

