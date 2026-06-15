---
number: 46
title: "spoke renders the full fleet in the spaces list (fold snapshot workspaces) + right-click server filter"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-11T08:46:13Z
closed: 2026-06-11T11:53:31Z
url: https://github.com/gerchowl/herdr/issues/46
---

# spoke renders the full fleet in the spaces list (fold snapshot workspaces) + right-click server filter

## Live dogfood gaps (first real hub→spoke session)

1. **Same-project-different-server info gets lost on a spoke.** PR #39 deliberately rendered carried snapshot peers in the servers band only. In practice: switching to sage, the spaces list shows only sage's own workspaces — the hub view's cross-server project folding (same project_key grouping local + remote rows) disappears. Requirement: the serving spoke folds the SNAPSHOT peers' workspaces into the spaces project groups exactly like config-peer summaries fold on the hub (the render machinery exists — feed it from `fleet_snapshot.peers` in addition to `peer_summaries`). Staleness chips apply as in the band. Selecting a snapshot remote row switches via its carried address (already wired for band rows — reuse).
2. **Right-click a server row → 'only this server'**: context-menu entry on servers-band rows filtering the spaces list to that server's workspaces (a per-server narrowing of the spaces scope; clears via the same menu or the spaces all/current toggle). Composes with #44's scope toggles.

## Constraints
Render/state only — no new polling on spokes (snapshot stays decay-only). Sequencing: after PR #44 merges (same sidebar surfaces).

---

## Comments

### gerchowl — 2026-06-11T11:53:30Z

Shipped in PR #51.

