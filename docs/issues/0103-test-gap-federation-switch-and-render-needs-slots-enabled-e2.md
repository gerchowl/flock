---
number: 103
title: "test gap: federation switch-and-render needs slots-enabled e2e + a live smoke harness"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-13T08:28:00Z
closed: 2026-06-13T18:39:31Z
url: https://github.com/gerchowl/herdr/issues/103
---

# test gap: federation switch-and-render needs slots-enabled e2e + a live smoke harness

## Test gap: federation switch-and-render is not covered with real config flags

#102 shipped because NO test exercised "switch to a spoke WITH [slots] enabled, assert the home row renders." The existing coverage missed it on every axis:

- **peer_federation.rs e2e** spins up REAL herdr servers + a fake-ssh shim and drives a switch -- but ONLY via the legs path (HERDR_SWITCH_FILE). It never ran with `[slots] enabled`, so the entire slots switch path had zero end-to-end gossip coverage.
- **Unit tests** (#93) covered the gen-counter/popup/esc mechanics -- the CLIENT-LOCAL state machine -- never "does the snapshot reach the peer and render."
- **No config-matrix**: slots on/off is a behavior fork with no paired e2e.

## The fix (layered)
1. **CI (durable)**: extend peer_federation.rs with a slots-ENABLED variant of the switch tests -- spawn the spoke server, drive the switch with slots on, assert the rendered spoke frame contains the home row + folded hub spaces. This exact test fails on #102 and passes on its fix. Parameterize the existing switch e2e over `[slots] {disabled, enabled}`.
2. **Config-fork lint**: any behavior gated on a config flag (slots, tab_mode, server_state_mark) should have a paired e2e or a documented why-not.
3. **Pre-flip smoke (process)**: before flipping a fleet-wide config like slots-on in g-fleet, run a federation smoke (below) -- don't flip blind.
4. **Live smoke harness (tui-probe / VMs)**: a scripted real switch (mba22->vm-dev) that screenshots the spoke frame and asserts the servers band. We HAVE the VMs; wire a `just herdr-smoke` that drives it headlessly.

## Acceptance
- A red-then-green slots-enabled federation e2e committed alongside the #102 fix.
- `just herdr-smoke` drives a real cross-process switch and asserts the home row.

## References
#102, tests/peer_federation.rs, the tui-probe skill, #93/#65.

---

## Comments

### gerchowl — 2026-06-13T08:59:55Z

Partial: PR #104 added resolve_handshake_fleet red->green seam tests (the cold-dial decision point). STILL OPEN -- the harness item: a peer_federation e2e that launches a REAL slots client, drives a switch, and asserts the spoke renders the home row. That requires the harness to exercise the slot manager (today it drives raw socket handshakes, bypassing slots). Plus 'just herdr-smoke' for a live cross-process switch before any slots-on flip.

### gerchowl — 2026-06-13T18:39:31Z

Shipped: PR #113 (spawn_switch_dial dial-boundary unit test, red-proofed) + just herdr-smoke (g-fleet) -- a live two-server slots-enabled cross-process switch that asserts the spoke home row, PROVEN to fail on the pre-#104 binary and pass on the fix. This is the pre-flip gate: run it before re-enabling [slots]. The full in-harness slots-client e2e (option a) and warm-flip smoke remain noted follow-ups.

