---
number: 100
title: "release pipeline: atlas-built GH release binaries + harmonia cache (transport v2)"
kind: issue
state: OPEN
author: gerchowl
labels: []
created: 2026-06-12T21:57:04Z
closed: 
url: https://github.com/gerchowl/herdr/issues/100
---

# release pipeline: atlas-built GH release binaries + harmonia cache (transport v2)

## Follow-on to the atlas-builder wiring (g-fleet)

Landed today: atlas signs its builds (secret-key-files), hopper trusts the key, `just herdr-prebuild` builds the pinned rev on atlas and nix-copies the signed path over user ssh. Linux already builds once (kiln via --build-host; bastion consumes).

Remaining for the full vision:
1. **GH releases**: `just herdr-release` — atlas (aarch64-darwin) + kiln-dev (linux) build the pinned rev, upload binaries to a `fork-<rev7>` release on gerchowl/herdr (human/offline consumption; nix keeps consuming via the cache, not GH).
2. **Transport v2 — harmonia on atlas**: an always-on HTTP cache over tailscale replaces the manual nix copy; macs add it to extra-substituters and every apply substitutes automatically (no pre-step). Watch: atlas runs Lix with nix.enable=false — harmonia as a plain launchd daemon.
3. **Drv-identity check**: confirm the dotfiles-consumed herdr drv == herdr's own flake build (input follows could fork them); if they differ, align the inputs or prebuild via the dotfiles flake eval instead.

## References
g-fleet commit 'atlas as signing builder', #75 (client-anchor arc), ADR-027.
