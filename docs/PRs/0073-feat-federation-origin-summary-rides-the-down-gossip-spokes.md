---
number: 73
title: "feat(federation): origin summary rides the down-gossip — spokes see the hub's spaces (#66)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-11T22:35:00Z
closed: 2026-06-11T22:35:05Z
merged: 2026-06-11T22:35:05Z
base: feat/sidebar-row-gap
head: origin-gossip
url: https://github.com/gerchowl/herdr/pull/73
---

# feat(federation): origin summary rides the down-gossip — spokes see the hub's spaces (#66)

The hub's OWN workspaces fold into spoke sidebars: `FleetSnapshot.origin_summary` (Boxed, **proto 16→17**), home-sentinel switch targeting (spokes have no route back), `SwitchServer.focus_workspace` so clicking `mba22:<ws>` on sage lands home with that workspace focused; pass-through preserves the original origin on nested leaps.

Note: the implementing agent stalled (infra) before committing; the changeset was recovered by replaying its transcript (46/46 ops) + finisher fixes (harness proto literals, focus-option split at the fleet re-feed, structural focus assert, Boxed origin to clear large-enum-variant). Rebased over #71/#72. Unit 2092/0 · peer_federation 7/7 · live_handoff 16/16 · clippy clean.
