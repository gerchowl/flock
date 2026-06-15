---
number: 154
title: "feat(web): identity allow-list, idle timeout + session cap, --session (#147 #148 #151)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-15T02:21:02Z
closed: 2026-06-15T02:21:10Z
merged: 2026-06-15T02:21:10Z
base: master
head: feat/web-hardening
url: https://github.com/gerchowl/herdr/pull/154
---

# feat(web): identity allow-list, idle timeout + session cap, --session (#147 #148 #151)

Closes #147, #148, #151. Builds on the merged `herdr web` bridge (#131).

The three small `src/web/mod.rs` follow-ups, grouped (same file, all with tests + live verification):

## #147 — tailscale identity allow-list
`--allowed-user <login>` (repeatable) / `HERDR_WEB_ALLOWED_USERS`. When set, the WS upgrade requires a listed `Tailscale-User-Login` (case-insensitive); an **absent** identity is rejected when a list is configured. Empty list = not enforced (loopback/tailnet stays the boundary).

## #148 — idle timeout + concurrent-session cap
- `--idle-timeout <secs>` (0 = off, default off) closes a WS with no inbound frame in the window — bounds an abandoned tab without killing active output-watchers by default.
- `--max-sessions <n>` (default 16, 0 = unlimited) backstops PTY exhaustion via an RAII slot guard that releases on every exit path.

## #151 — first-class `--session <name>`
Forwarded to the spawned client as the global `--session` flag (so a launchd unit reads `--session main`, not `-- --session main`).

All flags have env equivalents. New pure helper `identity_allowed` unit-tested; **14 web tests pass**, clippy clean, default build unaffected.

**Live-verified** against the built binary: no/wrong identity → 403, listed identity → 101, over-cap → 503, idle → close.
