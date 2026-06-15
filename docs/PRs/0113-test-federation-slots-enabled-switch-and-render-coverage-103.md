---
number: 113
title: "test(federation): slots-enabled switch-and-render coverage (#103)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-13T18:38:26Z
closed: 2026-06-13T18:39:26Z
merged: 2026-06-13T18:39:26Z
base: master
head: slots-federation-test
url: https://github.com/gerchowl/herdr/pull/113
---

# test(federation): slots-enabled switch-and-render coverage (#103)

## Summary

- Adds a dial-boundary unit test (`slots_switch_dial_threads_hub_fleet_into_spoke_handshake`) that exercises `spawn_switch_dial` against a fake spoke listener, decodes the bincode `Hello` it sends, and asserts the fleet origin matches the hub. Catches regressions of the `let _ = &fleet;` pattern PR #104 fixed -- verified RED then GREEN by temporarily reverting the `fleet.as_ref()` arg.

- Companion `just herdr-smoke` recipe lives in `g-fleet` (gerchowl/dotfiles@4147459): drives a REAL cross-process slots switch and asserts the spoke renders the home row + folded hub workspace. Catches the same regression at the system level before any `[slots] enabled` flip in the fleet config.

## Honest scope

The issue's deliverable (a) -- an in-repo e2e that launches a real `herdr client` binary with slots enabled, drives the switch, and reads the spoke frame -- was infeasible in this PR's time budget without an ANSI-parsing TUI observer to read the slots client's PTY output. The unit test exercises `spawn_switch_dial` directly (covering the Path A `slot_socket_path(Home)` handshake call, which shares the same `do_handshake` invocation with the Path B SSH peer dial), proving the fleet survives to the wire.

The system-level e2e is delivered as `just herdr-smoke` in g-fleet -- the catch-before-deploy hook the issue called for. It is what should run BEFORE a `[slots] enabled = true` flip in the fleet config.

## What this CANNOT catch

A regression that drops `fleet.clone()` at the `spawn_switch_dial` call site in the `SwitchServer` ARM (instead of inside `spawn_switch_dial` itself) would not be caught by this unit test, because the test calls `spawn_switch_dial` directly. The `just herdr-smoke` recipe catches that.

## Red-proof

Reverted PR #104's `fleet.as_ref()` (Path A) to `None`:

```
thread 'client::tests::slots_switch_dial_threads_hub_fleet_into_spoke_handshake' panicked at src/client/mod.rs:3487:30:
spoke must receive Some(fleet) from the slots switch dial -- reintroducing `let _ = &fleet;` regresses #102 here
test client::tests::slots_switch_dial_threads_hub_fleet_into_spoke_handshake ... FAILED
```

Restored: GREEN.

## Test plan

- [x] `cargo test --bin herdr -- --test-threads=2` (2186 passed)
- [x] `cargo test --test peer_federation --test live_handoff -- --test-threads=2` (23 passed)
- [x] `cargo fmt --check`
- [x] `cargo clippy --bins --tests -- -D warnings`
- [x] Manual red/green confirmation by reverting the Path A fleet arg
