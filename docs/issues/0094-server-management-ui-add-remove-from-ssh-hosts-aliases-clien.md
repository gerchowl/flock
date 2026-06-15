---
number: 94
title: "server management UI: add/remove from ssh hosts, aliases, client-side overlay over nix peers"
kind: issue
state: OPEN
author: gerchowl
labels: []
created: 2026-06-12T15:10:16Z
closed: 
url: https://github.com/gerchowl/herdr/issues/94
---

# server management UI: add/remove from ssh hosts, aliases, client-side overlay over nix peers

## Feature (user): in-app server management from ssh hosts + aliases

1. **Discovery**: parse the local ~/.ssh/config Host entries; the servers band's context menu / a settings surface offers add/remove of fleet servers from that list. Empty-state/help text: "add hosts to ~/.ssh/config to see them here".
2. **Aliases**: short display names for the band/grammar (e.g. `gerchowl-mba22-1.tail…` → `mba22`) — used everywhere the server name renders (<server>:<branch>, band rows, agents grammar).
3. **Refresh on open**: the list re-reads ssh config when the interface opens (+ a manual refresh in the menu).
4. **Where it runs — resolved by the client-anchor model**: discovery is CLIENT-side (the ssh config that matters is the one on the machine you're typing at). Under #65/#79 the client owns fleet targets (slots) and viewer state — the server list becomes client state handed to whichever server renders. No bus round-trip needed; works identically while attached to a remote.
5. **Write target**: the nix-managed config.toml is a READ-ONLY symlink (HM) — in-app add/remove writes a separate overlay (e.g. ~/.local/state/herdr/servers.toml: additions, removals/hides, aliases) merged over config [[peers]] at load. The nix-generated hub list stays the base; the overlay is the user's live edits.

## Sequencing
Client-anchor arc: #65 (slots, landed) → #79 (viewer profile — the overlay file naturally lives beside it) → this. Composes with #80 (visibility selection) — alias + visibility may share the per-server overlay entry.

## References
~/.ssh/config parsing precedent (ssh_config crate or hand parse Host lines), #39/#76 (SlotTarget keys), #79/#80, g-fleet common.nix peer generation (the read-only base).
