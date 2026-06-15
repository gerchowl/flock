---
number: 51
title: "feat(federation): fold carried fleet into the spaces list + per-server filter (#46)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-11T10:07:31Z
closed: 2026-06-11T10:08:34Z
merged: 2026-06-11T10:08:34Z
base: feat/sidebar-row-gap
head: snapshot-folding
url: https://github.com/gerchowl/herdr/pull/51
---

# feat(federation): fold carried fleet into the spaces list + per-server filter (#46)

Implements #46 — both live-dogfood gaps from the first real hub→spoke session.

## 1. Spokes render the full fleet in the spaces list

PR #39 rendered carried snapshot peers in the servers band only; the spaces list lost cross-server project folding after a switch. Now the remote-row machinery feeds from **both** caches via a new `RemotePeerRef` (`Config { peer_idx }` / `Snapshot { entry_idx }`):

- `AppState::remote_peers()` yields config-peer summaries first, then carried `fleet_snapshot.peers` entries not shadowed by a config peer (**dedup by ssh target — the live-polled entry wins**; on a typical spoke there are no config peers, so the whole carried fleet folds in).
- `fold_remote_entries` / `WorkspaceListEntry::Remote` / `RemoteCardArea` now carry the peer ref; rendering (incl. staleness dimming — snapshot freshness keeps decaying, no polling added) and labels are unchanged otherwise.
- Selecting a snapshot-fed row emits `PeerSwitchRequest::SnapshotPeer` — the band's existing pass-through switch path: carried ssh address, fleet rides along with the original origin.

## 2. Right-click a server row → 'only this server'

- Right-clicking a servers-band row (self, config peer, or snapshot peer) opens the existing context-menu machinery with a new `ContextMenuKind::Server`: **Show only this server** narrows the spaces list (self row = local workspaces only; a peer = its remote rows only, regrouped by project); **Show all servers** clears it. The home row gets no menu — the origin's workspaces are never in this list.
- State is a transient `Option<ServerFilter>` on AppState (ssh-target keyed, **not persisted**, excluded from session snapshots). Toggling the spaces scope (#44) also clears it.
- The filter applies inside `workspace_list_entries` — the same single source #44 put scope filtering in — so hit-areas, scroll, and selection clamp stay consistent. The spaces header announces an active filter (` spaces · only <host>`).

## Tests

11 new tests: snapshot folding (and absence without a snapshot), config-peer-wins dedup, snapshot-row click → SwitchServer with carried address + pass-through fleet, right-click filter apply/clear via menu and via scope toggle, scroll/hit-area clamping under filter, band slot hit-testing incl. the self row, header announcement, home-row no-menu, menu-items matrix.

Full suite: **1982 passed, 0 failed**; fmt + clippy (`-D warnings`) clean; guardrails gates green.
