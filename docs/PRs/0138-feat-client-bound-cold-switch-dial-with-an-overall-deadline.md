---
number: 138
title: "feat(client): bound cold-switch dial with an overall deadline"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-14T13:29:44Z
closed: 2026-06-14T13:32:43Z
merged: 2026-06-14T13:32:43Z
base: master
head: feat/switch-dial-deadline
url: https://github.com/gerchowl/herdr/pull/138
---

# feat(client): bound cold-switch dial with an overall deadline

Closes #134.

## What

The slots cold-switch dial ran on a detached thread with **no overall time limit**. #132 bounds each individual `ssh` with `ConnectTimeout=5`, but a peer that accepts the TCP connection and then stalls (auth, exec, or a bridge `accept` that never completes) could outlast the popup’s retry window — the switch popup spins with only `[esc]` to escape.

## Change

Race the blocking dial against `SWITCH_DIAL_DEADLINE` (28s — just past the popup’s "retry window ending soon" hint at 25s): the dial runs on an inner thread feeding an mpsc channel, and the worker `recv_timeout`s on it. On timeout the worker surfaces `SlotDialFailed("host did not respond within 28s")`, so the loop returns the user to the previous server. A late-arriving success is dropped (receiver gone) and its stream + bridge clean up on `Drop`. This is the existing `SlotDialFailed` path, so no new loop handling is needed.

The bulk of the diff is re-indentation from wrapping the dial closure in the inner thread; the logic is unchanged.

## Scope note (re: #134)

This lands **option 1** from the issue — the timeout-bounded fallback for the slots path. The legacy non-slots path is already bounded by #132 (every probe ssh now has `ConnectTimeout` + `BatchMode`, so it can no longer hang on a dead host or a prompt). **Option 2 — background pre-warming of config/snapshot SSH peers (stage-2)** — is the larger remaining work and is split into a follow-up issue rather than bundled here.

## Verification

- `cargo build`, `cargo clippy --all-targets -D warnings`, `cargo fmt` green.
- New test `switch_dial_deadline_outlasts_popup_retry_hint` locks the deadline > retry-hint invariant.

Note: CI `check` job has pre-existing flaky integration tests (`cross_area`, `cli_wrapper`) unrelated to this diff; `check-contributor` is the upstream fork gate.
