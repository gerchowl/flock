---
number: 146
title: "feat(client): pre-warm cold ssh peers so their switches flip instantly"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-15T00:17:25Z
closed: 2026-06-15T00:20:52Z
merged: 2026-06-15T00:20:52Z
base: master
head: feat/prewarm-ssh-peers
url: https://github.com/gerchowl/herdr/pull/146
---

# feat(client): pre-warm cold ssh peers so their switches flip instantly

Closes #139. Stage-2 of the slots warm-all design (completes #134 / the server-switch hardening series #132/#133/#138).

## Problem

The background warm sweep only dialed slots that already had a reachable socket — home and the active leg. **Config peers (`[[peers]]`) and snapshot peers (carried fleet)** had no bridge yet, so they were skipped and stayed cold. Switching to them always paid the cold-dial cost (one probe SSH + bridge build), while a warm slot flips instantly in-process.

## Change

The sweep now pre-warms cold ssh peers by **building their `SshStdioBridge` in the background** (`spawn_warm_bridge_dial`, which reuses the existing deadline-bounded bridge dialer). A later switch then hits the warm-flip path (the `SwitchServer` handler already tries `flip_to` first) and is instant.

- **Both peer kinds covered:** config and snapshot peers are both `SlotTarget::Ssh` in `warm_all_targets`, so the single sweep change warms both.
- **Bounded:** the existing `[slots] max` cap is the cost budget (each warm peer holds one ssh bridge); failed dials ghost with the registry’s existing `COLD_REDIAL_BACKOFF`.
- **Gated:** new `[slots] prewarm_ssh_peers` (default `true`, matching the original warm-all design documented in `slots.rs`). Set `false` to keep ssh peers cold-by-design (the old stage-1 behaviour) for a very large fleet or flaky link.
- **Race guard:** `SlotManager::has_connection` lets the loop drop a redundant pre-warm/switch dial **off-loop** (bridge `Drop` can block) instead of overwriting — and orphaning a duplicate session on — a live connection when a sweep dial and a switch dial race for the same peer.

## Why this is safe / small

The registry + manager already modeled warm ssh bridges (the `bridge` field flows `SlotWarmed` → `add_warm`), and `pending_dials` already emits `Dial(Ssh(…))`. The only gate was the loop skipping bridge-less targets. This PR removes that gate for ssh peers and hardens the dial-race.

## Verification

- `cargo build`, `cargo clippy --all-targets -D warnings`, `cargo fmt --check` green.
- New unit test `has_connection_reflects_active_and_warm_only`; existing slots/registry tests pass (185 unit tests green).
- No integration test enables `[slots]`, so `peer_federation` e2e is unaffected (slot_manager is None there).

## Deferred

Active idle-eviction of unused warm bridges (the issue’s “probably an idle-eviction policy” consideration) is **not** included — the `max` cap bounds growth and dead bridges self-demote. Can be a follow-up if a large fleet wants tighter resource control.

Note: CI `check` job has pre-existing flaky integration tests (`cross_area`, `cli_wrapper`) unrelated to this diff; `check-contributor` is the upstream fork gate.
