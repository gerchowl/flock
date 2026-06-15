---
number: 126
title: "project header summary panel: lazygit-style + agent/fleet info on click"
kind: issue
state: OPEN
author: gerchowl
labels: ["enhancement"]
created: 2026-06-14T12:13:36Z
closed: 
url: https://github.com/gerchowl/herdr/issues/126
---

# project header summary panel: lazygit-style + agent/fleet info on click

Clicking a **project header** in the sidebar should open a summary view for that project — a fleet-aware dashboard of everything interesting about the repo.

**Candidate contents:**
- **GitHub:** open issues, milestones, PRs (via `gh`).
- **Fleet:** which servers have a checkout, agents running per checkout (+ status/activity — already gossiped in `PeerWorkspaceSummary`).
- **Git:** checkouts, worktrees, branches (local + per-peer), ahead/behind.
- Whatever else is cheap and useful (last activity, dirty state, …).

**Framing — "how far is this from lazygit in a pane?"** A lot of this is lazygit's territory (branches, PRs, status). The thing herdr adds that lazygit can't is the **fleet + agent layer**: *which machine* has the branch, *which agent* is working it, and its live status — herdr already gossips exactly that metadata (`src/peers.rs`, `grammar.rs:152` `agent_location_label`). So the differentiator is the agent/server overlay on top of a git dashboard. Worth deciding: build a native summary panel, or embed/launch lazygit in a pane and augment it with the fleet/agent strip?

Part of milestone: Fleet project view + cross-machine worktrees.
