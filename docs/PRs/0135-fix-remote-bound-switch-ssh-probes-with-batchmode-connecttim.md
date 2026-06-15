---
number: 135
title: "fix(remote): bound switch SSH probes with BatchMode + ConnectTimeout"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-14T13:12:41Z
closed: 2026-06-14T13:21:18Z
merged: 2026-06-14T13:21:18Z
base: master
head: fix/ssh-probe-timeouts
url: https://github.com/gerchowl/herdr/pull/135
---

# fix(remote): bound switch SSH probes with BatchMode + ConnectTimeout

Closes #132.

## What

The cold server-switch path ran bare `ssh -T <target>` for platform/binary discovery (`ssh_sh_output`, `ssh_user_shell_output`) and for the bridge tunnel (`bridge_connection`), with **no connection safety flags**. A dead, firewalled, or unknown host could block a switch **indefinitely** on a TCP black-hole or an interactive password / host-key prompt — the "switch just hangs / breaks herdr" failure mode.

## Change

Adds a shared `SSH_NONINTERACTIVE_OPTS`:
- `BatchMode=yes` — never prompt for password / passphrase
- `ConnectTimeout=5` — bounded TCP connect
- `StrictHostKeyChecking=accept-new` — no interactive host-key prompt

Applied to both probe helpers and the bridge ssh command. Matches the flags the pre-focus thread already uses (`src/app/api/peers.rs`).

## Verification

- `cargo build`, `cargo clippy -D warnings`, `cargo test` all green.
- Switching to an unreachable peer now fails within ~5s instead of hanging.
