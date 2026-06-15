---
number: 125
title: "cross-machine checkout: pull a peer's branch into a local worktree (no git/gh dance)"
kind: issue
state: OPEN
author: gerchowl
labels: ["enhancement"]
created: 2026-06-14T12:13:34Z
closed: 
url: https://github.com/gerchowl/herdr/issues/125
---

# cross-machine checkout: pull a peer's branch into a local worktree (no git/gh dance)

**The headline DX.** Right-click a **peer row** (e.g. `sage:feature-x`) → "Check out here" → confirm → you have the same checkout locally as a worktree, ready to build/test — without manually pushing/pulling by hand.

Today every worktree op is a local `git` subprocess (`src/worktree.rs:139`), peer workspaces are **render-only**, and the right-click menu only fires on local rows (`src/app/input/mouse.rs:1196`). No cross-machine creation, no host-targeting.

## Default path: clean, via origin (MVP)
herdr orchestrates the dance you'd otherwise do by hand:
1. On the peer (over the SSH already used for `peers.summary`): `git -C <repo> push -u origin <branch>`.
2. Locally: `git fetch origin <branch>` + `git worktree add`.
3. Open the result as a local workspace.

Reuses existing GitHub auth, near-zero new plumbing. This is the **default** and the 1-day version.

## Preflight + confirmations (the UX contract)
Before acting, probe the peer's state and gate accordingly:
- **Dirty working tree** on the peer → **warn / confirm**. Only committed refs transfer; unstaged/uncommitted edits do NOT come across. User must acknowledge they're pulling the last commit, not live edits.
- **Branch not pushed** (ahead of `origin`, or no upstream) → **warn / confirm**, then offer to push as part of the flow (default) — making "clean, via gh" the path of least resistance.
- Clean + already pushed → proceed with just a host confirm ("you're on `mba22` — check out `sage`'s `feature-x` here?").

## Opt-in alternate paths (allow if required)
- **Direct peer fetch** — `git fetch ssh://peer/path <branch>` straight from the peer's disk → worktree add. Never touches GitHub, so private/never-pushed WIP works. Needs a repo-path field added to `PeerWorkspaceSummary` (`src/api/schema.rs:1110`) + git-over-SSH to the peer. Follow-up phase; surfaced as the choice when the branch isn't pushed and the user declines to publish.
- **Dirty-tree mirror** (out of scope for now) — moving an uncommitted working tree would need rsync-like transfer; explicitly not in the default flow.

## Plumbing
- New `ConfirmMode` for the host + dirty + unpushed warnings (herdr has no host-aware confirmations today — only merge-gate ones).
- Peer-state probe (clean? pushed? upstream?) — either extend `peers.summary` or a one-shot SSH query at action time.
- `PeerWorkspaceSummary` repo-path field (only needed for the direct-fetch path).

Part of milestone: Fleet project view + cross-machine worktrees. Tracked in #127.
