---
number: 131
title: "port herdr-web bridge into herdr as a first-class option (g-fleet keeps only the tailscale-serve exposure)"
kind: issue
state: CLOSED
author: gerchowl
labels: ["enhancement"]
created: 2026-06-14T12:41:42Z
closed: 2026-06-15T02:02:39Z
url: https://github.com/gerchowl/herdr/issues/131
---

# port herdr-web bridge into herdr as a first-class option (g-fleet keeps only the tailscale-serve exposure)

The web bridge currently lives in **g-fleet** (`pkgs/herdr-web/`: a 367-line axum + portable-pty + tokio WS server, plus `static/index.html` and vendored xterm). It was built there for #109-MVP speed (sage-only, Darwin-only launchd, vendored) — but it's a small, self-contained app that is *entirely coupled to herdr's own `HERDR_RENDER_ENCODING=terminal-ansi` output*. It belongs in the product, not in personal dotfiles.

## Architecture (unchanged)
```
browser (xterm.js) <--WS--> herdr-web <--PTY--> herdr (terminal-ansi)
```
The bridge spawns a herdr **client** in a PTY per WS connection and pumps bytes both ways (`pkgs/herdr-web/src/main.rs:211`).

## Proposed boundary
| Concern | Owner |
|---|---|
| WS↔PTY bridge, embedded frontend (xterm + index.html), enable flag/config | **herdr** (first-class) |
| Which host enables it + the `tailscale serve` rule (loopback→tailnet) | **g-fleet** (topology only) |

## Design choices
1. **`herdr web --bind 127.0.0.1:PORT` subcommand** (separate in-tree binary/crate or cargo feature) vs. baking a listener into `herdr server`. Recommend the subcommand so the web deps (axum/tower/portable-pty) stay behind a feature and a default `herdr` build stays lean. herdr already depends on tokio.
2. **Embed assets** (include_dir / rust-embed) — drop `--static-dir`, ship a self-contained binary. Fixes #128/#129/#130 in the product instead of in dotfiles.
3. **Config surface**: a `[web]` block / flags (bind addr/port, session args). g-fleet's module shrinks to `enable + tailscale serve`.

## Wins
- Cross-platform for free (the NixOS side is a stub today only because it's hand-rolled). Any host can opt in; g-fleet just picks which.
- Web-view fixes (#128 badge overlap, #129 key bar, #130 mouse forwarding) land in herdr, versioned with it.
- One release artifact; no vendoring drift in g-fleet.

Parent: #109. Subsumes the home for #128 / #129 / #130.

---

# Spike addendum (2026-06-14): hosting topology, bridge transport, gossip freshness

This expands #131 from "move the bridge into herdr" into the architectural decision behind it: **the web bridge should be a fleet *client* attached to a headless herdr node that gossips independently of any interactive machine** — so a phone can see the whole fleet via an always-on host (e.g. sage) even when the MacBook is off. Parent #109.

## Hosting topology — the headless node is the fleet member, the web bridge is just a client

Verified against the code:

- A **headless `herdr server`** (`src/server/headless.rs:3272`) participates *fully* in fleet gossip with **no TUI and no client attached**. The peer-poll loop lives in `App` (`src/app/mod.rs:304`), which lives in the headless server — **not** in the interface client. Clients never gossip.
- The **interface client** (`src/client/`) is a thin paint-only client; with `HERDR_RENDER_ENCODING=terminal-ansi` the server pre-diffs and ships raw ANSI and the client is a stdout passthrough (`src/client/mod.rs:1697`). That byte stream is exactly what xterm.js consumes.
- ⇒ **sage runs a headless server that gossips with the fleet regardless of the laptop. The web bridge is *another client* attaching to sage's local server. The phone sees the whole fleet** (sidebar peer rows, reachability, agent-attention rollup) because *sage's server* is the poller — the laptop being off is irrelevant to what the phone sees.

The MVP almost does this but spawns a fresh default `herdr` per WS connection (each attaches/auto-spawns a server) rather than deliberately attaching to a **persistent, always-on headless server**. Decision below.

## Gossip freshness — two paths, do not conflate (the "15s cadence" question)

| State the phone is viewing | Path | Latency |
|---|---|---|
| The **attached server's own** sessions/panes/agents (sage) | direct render-frame stream, pushed every render (`src/server/render_stream.rs`) | **instant** |
| **Other fleet members'** state (MacBook reachability, peer sidebar dots, cross-host agent rollup) | **15s pull-poll**: `peer-summary-tick` → `ssh <peer> herdr peers summary --json` (`src/app/mod.rs:309`, `src/app/api.rs:162`, `src/peers.rs:201`) | **up to ~15s** + staleness (`PEER_STALE_AFTER_SECS = 60`) |

**There is no push / broadcast / notify-on-change between servers.** A local change on one host is *not* immediately gossiped; peers learn it on their next 15s poll. The only non-poll propagation is `FleetSnapshot`, a one-shot ride-along stamped only on a client server-switch (`src/app/api/peers.rs:154`) — not continuous. The phone attaching to sage does not change this: sage is the poller, so cross-fleet rows on the phone are 15s-cadence regardless.

**Decision to make:** for the phone-glance case, is 15s-stale cross-fleet state acceptable (likely yes — it's a glance), or does this case warrant a faster poll / push-on-change gossip? The latter is a **fleet-wide** change larger than the web bridge and should be split into its own issue if pursued.

## Spike decisions (to be settled by the review panel + owner)

1. **Bridge transport.** (a) Spawn `herdr client` subprocess in a PTY (MVP — reuses everything, opaque byte pump) vs (b) speak the `herdr-client.sock` bincode protocol natively in the bridge (`src/protocol/wire.rs` `ClientMessage`/`ServerMessage`; mirrors `remote::run_client_process`). (b) drops the PTY + subprocess, enables structured input (mouse #130, key bar #129) and a real "connected" signal (#128), but reimplements the client handshake/encoding loop.
2. **Server attach model.** Persistent shared headless server (sessions survive WS disconnect; one server gossips; multiple clients incl. the TUI view the same live session) vs ephemeral per-WS server (MVP). Recommend **persistent**, matching the always-on-sage intent.
3. **Packaging.** `herdr web --bind 127.0.0.1:PORT` subcommand behind a cargo feature (axum/tower/portable-pty gated; lean default build) vs baking a listener into `herdr server`. Embed assets (rust-embed/include_dir), drop `--static-dir`.
4. **Auth boundary.** v1 = bind 127.0.0.1 + `tailscale serve` (tailnet identity). Decide whether to map the tailscale identity header → herdr user, and whether multi-client write access needs any guard (a web client can drive kill/worktree/branch actions).

## Pitfalls

- **Conflating the two freshness paths** (instant local render vs 15s peer poll) → wrong expectations about what the phone sees in real time.
- **Ephemeral-per-WS server** silently forks fleet state / loses sessions on disconnect; defeats the always-on intent.
- **Subprocess transport** makes #128/#129/#130 (status badge, key bar, mouse forwarding) harder — they want structured frames, not an opaque PTY byte pump.
- **Feature-gating leak**: pulling axum/tower/portable-pty into the default build bloats every `herdr`. Must stay behind the feature.
- **Auth**: binding anything but loopback, or trusting `tailscale serve` without verifying the funnel-vs-serve distinction, exposes a full shell to the tailnet (or worse).
- **15s poll under a large fleet** brushing ARG_MAX on leg spawn (`src/peers.rs:121`) / SSH fan-out cost if the poll is sped up for the phone case.

## Acceptance criteria

- [ ] `herdr web` (or chosen entry) ships in-tree, feature-gated, default build unaffected.
- [ ] Web bridge attaches to a **persistent** local headless server; sessions survive WS reconnect.
- [ ] Phone over `tailscale serve` sees the live local session **and** the fleet sidebar (peer rows/reachability) sourced from that node's gossip.
- [ ] Assets embedded; no `--static-dir` needed.
- [ ] g-fleet module shrinks to `enable + tailscale serve`.
- [ ] Decision recorded (ADR) on transport, server-attach model, and the 15s-cadence acceptability.

## References

- Headless server: `src/server/headless.rs:3272`; gossip loop: `src/app/mod.rs:304`, `src/app/api.rs:162`; peers: `src/peers.rs`, `src/app/api/peers.rs`, `src/cli/peers.rs`.
- Render encoding: `src/protocol/wire.rs:78` (`RenderEncoding`), `src/client/mod.rs:1697` (ANSI passthrough), `src/server/render_stream.rs`.
- Transport/sockets: `src/server/socket_paths.rs`, `src/remote.rs:1794` (`run_client_process`), `src/protocol/wire.rs` (`ClientMessage`/`ServerMessage`).
- MVP: g-fleet `pkgs/herdr-web/src/main.rs`, `modules/herdr-web.nix`.
- Related: #109 (parent), #128, #129, #130.

---

## Comments

### gerchowl — 2026-06-15T00:36:28Z

## Implemented + reviewed (spike round 2)

Landed on branch `web-xterm` (commits eaaaafd, 9b9f1f4, 1fabada):

- `herdr web` subcommand, feature-gated (`--features web`); default build unaffected (verified: `cargo tree --no-default-features` has no axum/rust-embed/etc).
- Spawns a `herdr` client (`terminal-ansi`) per WS → attaches to the local persistent server (the always-on-daemon topology). Assets embedded via rust-embed (no `--static-dir`).
- Security P0s: refuse non-loopback bind, refuse under active `tailscale funnel`, same-origin WS check (CSWSH). 11 unit tests; live HTTP + CSWSH smoke test green.
- Round-2 review fixes: strip inherited `HERDR_*` before spawn (was tripping the nested-launch guard — every connection flash-exited when run from inside herdr; verified fixed end-to-end), abort the WS→PTY task on exit (thread/fd leak), case-insensitive Origin/Host.
- Decision recorded: `docs/adr/0001-web-bridge-hosting-and-transport.md`.

### Acceptance status
- [x] `herdr web` in-tree, feature-gated, default build unaffected
- [x] Assets embedded; no `--static-dir`
- [x] Phone sees fleet sidebar from the attached node's gossip
- [~] Persistent-server attach — works via default-launch; explicit `--session` flag is #151
- [ ] g-fleet shrinks to `enable + tailscale serve` — #149 (separate repo)
- [x] Decision recorded (ADR)

### Follow-ups filed
- #147 — tailscale identity allow-list (P1)
- #148 — idle WS timeout + session cap (P1)
- #149 — g-fleet: retire MVP, run `herdr server` daemon + `herdr web` (P1)
- #150 — native/stdio transport v2 (unblocks #128/#129/#130)
- #151 — first-class `--session` flag

The existing #128 (status badge), #129 (key bar), #130 (mouse forwarding) now apply to the in-tree `assets/web/index.html` and are best done atop #150.

