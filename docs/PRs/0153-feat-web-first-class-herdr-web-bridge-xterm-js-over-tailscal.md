---
number: 153
title: "feat(web): first-class `herdr web` bridge (xterm.js over tailscale) (#131)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-15T02:02:24Z
closed: 2026-06-15T02:02:38Z
merged: 2026-06-15T02:02:38Z
base: master
head: web-xterm
url: https://github.com/gerchowl/herdr/pull/153
---

# feat(web): first-class `herdr web` bridge (xterm.js over tailscale) (#131)

Closes #131. Parent #109.

Ports the g-fleet herdr-web MVP into herdr as a feature-gated `herdr web` subcommand: an axum WebSocket↔PTY bridge that spawns a `herdr` client (`HERDR_RENDER_ENCODING=terminal-ansi`) per connection and pipes the server-diffed ANSI straight to xterm.js. On an always-on host the client attaches to the persistent `herdr server` daemon, so a phone over `tailscale serve` shares that node's live session and its fleet gossip view independently of the laptop.

## What's in it
- `herdr web` behind `--features web` (axum/futures-util/rust-embed/anyhow + tokio net/io-util are feature-only; **default build unaffected**). Dispatch routed through `cli::maybe_run` (single cfg site; non-web build prints how to enable).
- Frontend (index.html + vendored xterm) **embedded** via rust-embed; no `--static-dir`.
- **Security P0s** (from the spike review panel): refuse non-loopback bind, refuse to start under active `tailscale funnel`, same-origin WS check (CSWSH).
- Spawned client gets a **clean `HERDR_*` environment** (else the nested-launch guard kills every connection when run from inside herdr).
- ADR 0001 records the hosting-topology / transport / 15s-cadence decisions.

## Verification
- 11 unit tests (origin/funnel/content-type/embed); clippy clean; default + `--features web` build clean; all pre-commit gates pass.
- Live smoke tests: HTTP routes + embedded assets, cross-origin WS → 403, and an end-to-end WS test proving `HERDR_*` is stripped from the spawned child.

## Spike trail
Two fresh-agent review rounds (transport, fleet-topology, security, packaging; then correctness + scope). Follow-ups: #147 (tailscale identity), #148 (idle timeout/cap), #149 (g-fleet retirement + daemon), #150 (native transport v2 → #128/#129/#130), #151 (`--session`).

Note: adds a `.gitignore` exception for `/docs/adr/` (mirrors the existing `/docs/next/` one) so the ADR is versioned.
