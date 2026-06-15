---
number: 132
title: "Server switch: probe SSH commands have no timeout/BatchMode → switch can hang forever"
kind: issue
state: CLOSED
author: gerchowl
labels: ["bug"]
created: 2026-06-14T13:07:17Z
closed: 2026-06-14T13:21:19Z
url: https://github.com/gerchowl/herdr/issues/132
---

# Server switch: probe SSH commands have no timeout/BatchMode → switch can hang forever

## Problem

The cold server-switch path runs several SSH probe commands against the target peer before opening the bridge. The two helpers that run them build a bare `ssh -T <target> …` with **no connection safety flags**:

- `ssh_sh_output` — `src/remote.rs:1393-1416`
- `ssh_user_shell_output` — `src/remote.rs:1418-1424`

Neither passes `BatchMode`, `ConnectTimeout`, or `StrictHostKeyChecking`. As a result, a dead/firewalled peer, a TCP black-hole, an unknown host key, or a password prompt blocks **indefinitely** (until the system TCP timeout — many minutes).

During a switch this means:
- the popup keeps counting up but the underlying `ssh` children never return;
- `[esc]` only marks the dial generation stale — the hung `ssh` processes keep running;
- on the **legacy non-slots path** (no popup at all) the alt-screen freezes with no feedback and no cancel — this is the "switch just breaks herdr" case.

Note the fire-and-forget pre-focus thread already uses the right flags (`src/app/api/peers.rs:192-203`: `BatchMode=yes`, `ConnectTimeout=5`) — the probe helpers should match.

## Fix

Add to the probe SSH invocations:
- `-o BatchMode=yes` (never prompt for password / passphrase)
- `-o ConnectTimeout=5` (bounded TCP connect)
- `-o StrictHostKeyChecking=accept-new` (no interactive host-key prompt)

Apply to `ssh_sh_output` and `ssh_user_shell_output`. Consider the same flags on the bridge ssh (`bridge_connection`, `src/remote.rs:1708`).

## Acceptance

- Switching to an unreachable peer fails within ~5s instead of hanging.
- No interactive prompts can stall a switch.

Context: branch `server-switch-slow`.
