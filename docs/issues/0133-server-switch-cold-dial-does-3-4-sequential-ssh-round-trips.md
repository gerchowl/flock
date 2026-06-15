---
number: 133
title: "Server switch: cold dial does 3–4 sequential SSH round-trips → slow switches"
kind: issue
state: CLOSED
author: gerchowl
labels: ["enhancement"]
created: 2026-06-14T13:07:42Z
closed: 2026-06-14T13:27:55Z
url: https://github.com/gerchowl/herdr/issues/133
---

# Server switch: cold dial does 3–4 sequential SSH round-trips → slow switches

## Problem

A cold (non-warm) server switch runs the remote-herdr discovery as a series of **separate, sequential** SSH commands before the bridge is even started:

1. `detect_remote_platform` → `uname -s; uname -m` (`src/remote.rs:618`)
2. `remote_binary_on_path_any` → `command -v herdr` (`src/remote.rs:637`)
3. `remote_binary_matches` → `test -x … && … --version && … status client --json` (`src/remote.rs:662`) — run **once or twice** (PATH binary, then default location)

Each is a fresh `ssh` child (`start_switch_bridge_noninteractive`, `src/remote.rs:1585-1612`; same chain in `prepare_remote_herdr`, `src/remote.rs:548-616`). Without SSH ControlMaster multiplexing that is 3–4 full SSH handshakes back-to-back. On a slow link this is several seconds of latency before the popup sees any result — the popup’s 10s "host not responding" beat is realistic here.

Config-peer and snapshot-peer switches cannot currently be pre-warmed, so they almost always hit this cold path (see #DEPENDS).

## Fix

Collapse discovery into a **single** `/bin/sh -s` round-trip that returns everything the caller needs to decide locally:
- `uname -s` / `uname -m`
- `command -v herdr` (PATH binary, if any)
- for the PATH binary and the default `~/.local/bin/herdr`: `--version` + `status client --json`

Parse the combined output in Rust and pick the compatible binary with no further round-trips. Keep the interactive install path (`prepare_remote_herdr`) behaviour identical; this is purely about merging the read-only probes.

## Acceptance

- A cold switch to a reachable peer issues **one** probe SSH command, not 3–4.
- Selected remote binary / platform identical to today on macOS + Linux peers.

Context: branch `server-switch-slow`.

---

## Comments

### gerchowl — 2026-06-14T13:07:57Z

Related: depends on #132 (ssh timeouts), and #134 covers the pre-warm gap that makes config/snapshot peers hit this cold path.

