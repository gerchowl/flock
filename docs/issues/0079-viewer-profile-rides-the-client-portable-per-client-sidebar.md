---
number: 79
title: "viewer profile rides the client: portable + per-client sidebar state (width/collapse/scopes/filter)"
kind: issue
state: OPEN
author: gerchowl
labels: []
created: 2026-06-12T08:26:53Z
closed: 
url: https://github.com/gerchowl/herdr/issues/79
---

# viewer profile rides the client: portable + per-client sidebar state (width/collapse/scopes/filter)

## Design gap (user question: "does the thin client gossip viewer state?")

No. Viewer state (sidebar width/split, collapse keys, panel scopes, server filter, scroll) is SERVER-side AppState: per-server (your collapse state doesn't follow a switch) and worse, per-server-not-per-client (two clients on one server share + fight over one collapse state).

## Proposal — the client owns the viewer profile

1. **ViewerProfile rides Hello** (the #52 theme precedent): { sidebar split/width, panel scopes, collapsed project keys, collapse_all, server filter, state-mark choice }. Server applies it to THAT client's view. Portable by construction: collapse keys are origin-shared project keys (#27/#62) — "collapsed gerchowl/dompt" means the same everywhere; host-keyed filter likewise.
2. **Live updates**: a small ClientMessage::ViewerStateChanged on toggle/drag, so mid-session changes propagate to the active server (and with #75's slots, to warm slots — switch anywhere, same view).
3. **Per-client view state server-side**: move the viewer fields off global AppState into per-client session view state (multi-client independence falls out). Render path already renders per client geometry; this extends the principle.
4. Persistence: the client persists the profile locally (~/.local/state/herdr/viewer.toml or similar) — the same profile attaches everywhere; per-server snapshots stop carrying scopes/collapse (migration: read old snapshot fields as the seed once).

## Sequencing
Natural #75 companion (slots stage 2: the client already becomes the long-lived state owner). Protocol additive; bump per the usual lockstep rule.

## References
#52 (theme-on-Hello precedent), #44 (scope toggles + snapshot fields), #62 (project-key grammar), #65/#75 (client as fleet anchor).
