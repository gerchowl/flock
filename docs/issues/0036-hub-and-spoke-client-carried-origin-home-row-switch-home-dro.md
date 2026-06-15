---
number: 36
title: "hub-and-spoke: client-carried origin + home row + switch_home (drop the full mesh)"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-11T06:29:20Z
closed: 2026-06-11T11:53:15Z
url: https://github.com/gerchowl/herdr/issues/36
---

# hub-and-spoke: client-carried origin + home row + switch_home (drop the full mesh)

## Motivation

First two-server dogfood: the full-mesh peer topology (every host lists every other, N×N ssh auth) was built to solve "switched to a peer, no way back" — but the client process always lives on the hub (the laptop that launched it). The way home is CLIENT knowledge; encoding it as server-side peer config forces reverse ssh trust (spoke keys authorized on the laptop), N×N key distribution, and per-spoke config — all unnecessary for the actual usage: leap from the hub into a spoke and back.

## Proposed design — client-carried provenance (hub-and-spoke)

1. **Attach handshake carries origin**: when the client attaches via `--remote`/SwitchServer, it sends its origin host label (the hub's short_host_name) + a "came from elsewhere" flag.
2. **The server renders a home row**: first row of the servers band (above self): `← mba22 (home)`. Selecting it emits SwitchServer{home sentinel} — the client interprets it as "re-attach local" (it already has the attach-loop; no ssh from the spoke required).
3. **`switch_home` keybind** (unset by default): same action without touching the sidebar.
4. No reverse polling: while attached to a spoke you see the spoke's world + the home row. Live hub summaries inside the spoke view are explicitly out of scope (home is one keypress away).

## Topology consequence (g-fleet side, after this lands)
Only the hub generates [[peers]]; spokes get none. Spoke→hub inbound keys get dropped. Adding a server = one hub-side entry.

## Pitfalls
- Protocol: the origin label rides the attach handshake — additive field; avoid a protocol bump if the handshake tolerates unknown fields, else bump deliberately.
- The home sentinel must survive the client relaunch loop in main.rs (run_remote attach-loop) — reuse the existing SwitchServer exit path with a reserved target value rather than a new message.
- Headless/monolithic mode: a locally-attached client has no origin — no home row (flag absent), zero behavior change.
- Nested leaps (hub→sage, then sage's UI offers its own peers→anvil): origin should be the ORIGINAL hub, not the previous hop — pass-through, don't re-stamp.

## References
#32/PR #34 (servers band v2 — the home row slots above the self row), peer federation PRs #18/#19, g-fleet mesh commit (to be partially reverted).

---

## Comments

### gerchowl — 2026-06-11T06:30:09Z

## Design extension (user): down-gossip, not just a home row

```
mba22 <-- sage    up-gossip: hub polls each spoke's summary        (exists)
mba22 --> sage    down-gossip: hub hands the spoke its fleet view  (this issue)
```

The switch handshake carries not just the origin label but the hub's **current fleet summary snapshot** (peer names + summaries + switch addresses). The spoke renders them as the remote rows it already draws — so from inside sage you see mba22 (home) AND anvil/ksb, and selecting any of them leaps directly (the client lives on the hub, which holds all ssh edges; the spoke needs zero outbound config).

- Staleness: snapshot-at-switch, refreshed every leap. Deliberately NOT a live relay — that would require the client to hold dual connections (the fat-client trap).
- The home row is then just the origin entry of the snapshot, pinned first.
- Spoke-side: render-only; reuse the existing peer-row machinery fed from the handshake payload instead of the spoke's own pollers.
- g-fleet consequence unchanged: only the hub generates [[peers]]; spokes none; spoke→hub inbound keys dropped.

### gerchowl — 2026-06-11T06:32:30Z

## Additional requirement (user): current-machine row uses the standard highlight

The currently-attached machine's row in the servers band must be highlighted with the SAME selection/highlight idiom that workspace/agent (session) rows use — the highlight bar / selected-row styling — not merely a marker glyph. "Where am I" should read exactly like the focused session highlight everywhere else in the side pane: one visual language for 'current' across workspaces, agents, and servers.

### gerchowl — 2026-06-11T11:53:14Z

Shipped across PR #39 (snapshot + home row + switch_home), #51 (spaces folding + server filter), #52 (attach retry #38 + theme port #47), #54/#55 (band medallion + state SSoT). Hub-and-spoke is the live topology; g-fleet generates hub-only peers.

