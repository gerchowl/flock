---
number: 127
title: "[Epic] Project-centric fleet organization + cross-machine worktree DX"
kind: issue
state: OPEN
author: gerchowl
labels: ["enhancement"]
created: 2026-06-14T12:13:54Z
closed: 
url: https://github.com/gerchowl/herdr/issues/127
---

# [Epic] Project-centric fleet organization + cross-machine worktree DX

Umbrella for the project-centric fleet model: organize everything by **project**, list each checkout/agent as `<server>:<branch>` in a fleet-stable order that reads the same on every machine, make worktree creation richer, and unlock the cross-machine "pull a peer's branch local" DX.

## Where we are today (already built)
- **Project fold** by `project_key` (normalized origin URL) — `src/workspace/git/discovery.rs:103`.
- **`<server>:<branch>` grammar** for members, `owner/repo` for leaders — `src/ui/grammar.rs`.
- **Fleet-stable sort** by lowercased `project_key`, identical on every server (#85) — `src/app/actions.rs:945`, `src/ui/sidebar.rs:584`.
- **Peer gossip:** SSH-polled `peers.summary` metadata folds remote checkouts under the local project (no PTY/frame sharing) — `src/peers.rs`.

So the organization layer is ~80% there. This epic closes the gaps and adds the summary panel + cross-machine checkout.

## Children
- [ ] #121 — navigator overlay: group by project (match the fleet-stable sidebar)
- [ ] #122 — sidebar: always render a project header, even for a solo project
- [ ] #123 — worktree create: pick the base ref (main / dev / defined branch), not just HEAD
- [ ] #124 — worktree create from an existing worktree (inherit its branch/state)
- [ ] #125 — cross-machine checkout: pull a peer's branch into a local worktree (no git/gh dance) ← headline DX
- [ ] #126 — project header summary panel: lazygit-style + agent/fleet info on click

## Notes
- #121/#122 are small wiring on top of the existing sidebar grouping.
- #123/#124 mostly expose machinery that already exists (`detect_default_branch`, CLI `--base`).
- #125 is the big one (new peer-fetch plumbing + a host-confirm `ConfirmMode`).
- #126 is the "lazygit + agent overlay" question — the agent/server layer is herdr's differentiator over a plain git TUI.
