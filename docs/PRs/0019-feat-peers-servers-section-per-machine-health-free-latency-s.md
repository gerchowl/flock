---
number: 19
title: "feat(peers): servers section — per-machine health, free latency, switch-on-click"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-09T09:28:40Z
closed: 2026-06-09T09:35:41Z
merged: 2026-06-09T09:35:41Z
base: feat/sidebar-row-gap
head: feat/peer-servers
url: https://github.com/gerchowl/herdr/pull/19
---

# feat(peers): servers section — per-machine health, free latency, switch-on-click

## What

A collapsible **`servers`** section above `spaces`, one row per federated peer:

```
 servers
 ● mba22    12ms   cpu 19% mem 13/16G   3 agents
 ● anvil    34ms   cpu 71% mem 48/64G   1 ● blocked
 ◐ sage    210ms   cpu  4% mem  2/8G    idle
 ○ ksb      —      unreachable 2m
```

Builds on #18 (peer federation). Answers the spike question — *what is informative per machine without throttling*.

## Informative, never throttling

The governing constraint: **add zero new round-trips.**

| Signal | Source | Cost |
|---|---|---|
| latency | wall-time of the summary SSH RTT we already make each poll | **free** — no ICMP, no extra ssh |
| reachability (live / slow >150ms / down) | latency + existing staleness/error tracking | free |
| cpu / mem / disk | piggybacked from each peer's existing 2s status-line sampler onto `peers.summary` | free — no new sampling |
| herdr version | one field — spot un-deployed peers | free |
| agent rollup (`3 agents` / `1 ● blocked`) | aggregate of workspace summaries already fetched | free |

A dead peer costs one already-bounded (`ConnectTimeout=5`) timed-out SSH and renders `unreachable 2m` — no retry storm, no faster cadence.

## How

- `PeerSystemSummary` (`cpu_percent` as `u8` to keep the response `Eq`) + `version` added to the `peers.summary` envelope, sourced from `state.system_stats`. **No wire-protocol change** — it rides the JSON summary, not `ServerMessage`.
- Poller times the SSH call → `latency_ms`; `PeerReachability` derives live/slow/down.
- Sidebar band carved off the top of the spaces area through a single shared `carve_servers_band` chokepoint, so render / scroll / hit-test stay consistent. Header click toggles collapse; a peer-row click switches to that peer's first workspace (reuses #18's `request_peer_switch`).

## Tests
- Unit: section height + collapse, hit-area layout, row formatting (live + unreachable), summary parse with/without the `system` block, reachability thresholds.
- E2E (`peer_federation`) extended to assert the `servers` section renders alongside the folded workspace row.
- Full suite **1980 passed / 0 failed**, clippy clean, fmt clean.

## Follow-ups (noted, not in scope)
- Persist `servers_collapsed` across restarts (currently resets to expanded).
- Optional overflow line for battery/gpu/net throughput.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
