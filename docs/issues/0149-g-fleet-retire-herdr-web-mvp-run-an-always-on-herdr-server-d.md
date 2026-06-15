---
number: 149
title: "g-fleet: retire herdr-web MVP, run an always-on `herdr server` daemon + `herdr web` on sage"
kind: issue
state: OPEN
author: gerchowl
labels: []
created: 2026-06-15T00:34:12Z
closed: 
url: https://github.com/gerchowl/herdr/issues/149
---

# g-fleet: retire herdr-web MVP, run an always-on `herdr server` daemon + `herdr web` on sage

Follow-up to #131. The herdr side has landed (`herdr web`, feature-gated, assets embedded). This is the g-fleet (~/dotfiles) side to close #131 acceptance "g-fleet module shrinks to enable + tailscale serve".

## Deltas (from the scope review)
- Delete `pkgs/herdr-web/` entirely (Cargo, src, static, vendor, package.nix) — now in-tree.
- Build the herdr flake input/package with `--features web` (else the binary prints "built without the web feature" and exits 2).
- Rewrite `modules/herdr-web.nix`: drop `herdrWebPkg`/callPackage and `--static-dir`; `exec herdr web --bind 127.0.0.1:PORT`. Keep the `tailscale serve` oneshot.
- Add a sister daemon that runs `herdr server` (always-on) as the same user, so the bridge attaches to ONE persistent gossiping server (the chosen topology) instead of auto-spawning an ephemeral one per first connection. TUI + phone both attach as clients.
- NixOS branch can drop its stub: `herdr web` is cross-platform; a systemd user unit mirrors the launchd one.

## Acceptance
- [ ] `pkgs/herdr-web/` removed; herdr pin built with `--features web`
- [ ] sage runs a persistent `herdr server` daemon; `herdr web` attaches as a client
- [ ] `modules/herdr-web.nix` is `enable + tailscale serve` only

Refs #131, #109.
