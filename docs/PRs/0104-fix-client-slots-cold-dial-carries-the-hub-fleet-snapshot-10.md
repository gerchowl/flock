---
number: 104
title: "fix(client): slots cold dial carries the hub fleet snapshot (#102)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-13T08:59:22Z
closed: 2026-06-13T08:59:27Z
merged: 2026-06-13T08:59:27Z
base: master
head: slots-fleet-carry
url: https://github.com/gerchowl/herdr/pull/104
---

# fix(client): slots cold dial carries the hub fleet snapshot (#102)

**The gossip break behind `[slots] enabled`.** The slots SwitchServer arm did `let _ = &fleet` -- discarding the hub's server-generated snapshot -- and `do_handshake` fell back to an env carry that is **empty on the hub** (the origin generates the snapshot, carries none). So a slots switch handed the spoke `fleet: None` -> no servers band, no home row, no way back.

Fix: `do_handshake` takes an explicit `fleet_override`; the slots cold dial (`spawn_switch_dial`) threads the `SwitchServer` fleet through; initial attach + warm pre-dial keep the env carry unchanged. `resolve_handshake_fleet` centralizes the decision and is unit-tested **red->green** (reintroducing `let _ = fleet_override` fails `resolve_handshake_fleet_prefers_the_explicit_override`).

**Scope/deferral:** warm-flip to a *genuinely-warm* spoke would need a post-flip snapshot push (new ClientMessage) -- deferred, because warm-all only warms home + the active bridge (#76), so spokes are always cold-dialed today. The full slots-client federation e2e is the larger harness item in #103.

2179 unit / peer_federation 7 / client_mode 19 -- all green; clippy clean.
