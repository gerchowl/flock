---
number: 66
title: "down-gossip the hub's OWN workspaces (origin summary) — spokes can't see mba22's spaces"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-11T20:00:11Z
closed: 2026-06-11T22:20:40Z
url: https://github.com/gerchowl/herdr/issues/66
---

# down-gossip the hub's OWN workspaces (origin summary) — spokes can't see mba22's spaces

## Gap (live dogfood on sage, follows #39/#51)

On a spoke, the carried fleet shows the OTHER peers (anvil/ksb ghosts fold into spaces per #51) — but **the hub's own workspaces are missing**: the FleetSnapshot carries the hub's peer_summaries, and the hub is not its own peer. The origin is only a label on the home row. Net effect: standing on sage, mba22's spaces/worktrees are invisible.

## Fix

1. **The snapshot carries an origin summary**: the hub synthesizes its own PeerSummaryState (the self-summary already exists — it's what `herdr peers summary` returns about the local server, src/app/api/peers.rs) and embeds it as the snapshot's origin entry (workspaces + system + version).
2. **The spoke folds origin workspaces into the spaces sections** exactly like carried peers (#51 machinery) — labeled `mba22:<branch>` per the #62 grammar.
3. **Switch target for origin rows = the HOME sentinel** (`"<home>"` → AttachLeg::Local), NOT an ssh dial — spokes have no route/auth to the hub (hub-and-spoke; inbound keys removed deliberately). Carry the focused-workspace target through the switch file so selecting `mba22:keyboard-shorcuts` on sage lands home WITH that workspace focused (the post-attach focus plumbing may need a small addition — the switch file gains an optional focus target).
4. Staleness: the origin summary is snapshot-at-switch like everything else; decays, refreshed per leap.

## Sequencing
After #63 (same switch-file/snapshot surfaces — its agent is mid-flight). Natural companion to #65 (slots make the origin data live instead of snapshot-stale) and #62 (label grammar).

---

## Comments

### gerchowl — 2026-06-11T22:20:40Z

Shipped: origin summary in the snapshot (proto 17), home-sentinel + focus_workspace targeting, spoke folding. The #65 slots upgrade later streams the same payload live.

