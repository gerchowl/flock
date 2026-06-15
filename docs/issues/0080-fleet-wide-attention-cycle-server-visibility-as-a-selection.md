---
number: 80
title: "fleet-wide attention cycle + server visibility as a selection set (SSoT with sidebar + switcher)"
kind: issue
state: OPEN
author: gerchowl
labels: []
created: 2026-06-12T08:50:17Z
closed: 
url: https://github.com/gerchowl/herdr/issues/80
---

# fleet-wide attention cycle + server visibility as a selection set (SSoT with sidebar + switcher)

## Design (user): fleet-wide attention + server visibility as a SELECTION

1. **Attention cycle goes fleet-agnostic**: focus_attention / focus_attention_previous (+ the project-scoped variants) rank LOCAL pane states AND remote agents (peer/origin summaries carry status + age) in one queue (blocked-oldest > done-unseen, as today). Landing on a remote agent performs its row's switch (leap + focus target, #73 plumbing; instant under warm slots #65/#75). Chime-when-clear stays fleet-wide-aware.
2. **One visibility model (SSoT)**: the server visibility set drives sidebar rows, the agents panel, AND the attention/switcher queue — filtered-out servers' agents are neither rendered nor visited. (Also: number-key indexing follows visible entries, the existing precedent.)
3. **Visibility = a selection set, not "only"**: per-server show/hide toggles (context menu / click on band rows; multi-hide supported); "only X" = select-one shortcut, "all" = full set; replaces #51's single Option<ServerFilter>. The selection lives in the VIEWER PROFILE (#79) — it follows the user across servers and is per-client.

## Acceptance
- ctrl+shift+a reaches a blocked agent on sage from mba22 (e2e with a remote blocked summary), respecting visibility.
- Hiding N servers removes their rows + agents + queue entries everywhere at once; counts/joins on the band remain (the band always shows the fleet — visibility filters the SPACES/AGENTS lists, not the servers band).
- Selection persists in the viewer profile (#79) once that lands; interim: session snapshot.

## References
attention machinery (focus_attention, fork), #51 ServerFilter, #62/#74 remote agent rows, #73 focus targeting, #65/#75 slots, #79 viewer profile.
