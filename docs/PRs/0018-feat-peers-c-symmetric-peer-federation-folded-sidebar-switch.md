---
number: 18
title: "feat(peers): C′ symmetric peer federation — folded sidebar + switch-on-select"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-09T08:25:35Z
closed: 2026-06-09T09:05:40Z
merged: 2026-06-09T09:05:40Z
base: feat/sidebar-row-gap
head: feat/peer-federation
url: https://github.com/gerchowl/herdr/pull/18
---

# feat(peers): C′ symmetric peer federation — folded sidebar + switch-on-select

## What

Drive **multiple herdr servers from one client** without merging runtime state — the C′ design for upstream [#334](https://github.com/ogulcancelik/herdr/issues/334) (*Support multiple remote Herdr servers*).

Reframes the tree from `server → project → worktree → agent` to **`project → {server:worktree → agent}`**: the server becomes an *attribute of the checkout*, not the root. Each server gossips a lightweight summary index over SSH; the sidebar folds peer workspaces into project groups; selecting a remote row switches the client to that server.

**Invariant preserved:** muscles never merge, they gossip. PTYs and frames never cross machines — the server stays the single render brain, the client stays a framebuffer, and the only new mechanism is a *steered* reconnect (a generalization of the detach/handoff path we already trust).

## How

| Layer | Change |
|---|---|
| Config | `[[peers]]` (name / ssh / summary_command), validated + live-reloadable |
| Identity | machine-independent `project_key` = normalized origin URL (`github.com/owner/repo`), or `dir:<name>` fallback. Folds checkouts of one project across machines. Derived live during the cold-start window before the async git-space cache populates. |
| RPC/CLI | `peers.summary` + `herdr peers summary --json` — id, project key/label, branch, attention-leading agent + status + age |
| Poller | one SSH worker per peer (staleness tracked) on the shared `api.rs` chokepoint — consumed by **both** the App and headless loops |
| Sidebar | `WorkspaceListEntry::Remote` folds under matching local project blocks (indented) or trails as remote-only groups; host-tagged, status-colored, dimmed when stale, hidden with a collapsed group |
| Switch | `ServerMessage::SwitchServer` (protocol **12→13**); client records target + exits like a detach; a launcher attach-loop in `main` chains into `herdr --remote <target>`; best-effort remote pre-focus |

```
 spaces
 ▾ herdr                       ← folds by origin URL
     herdr            cc · idle    (local)
     anvil:fix/pty    ● blocked    (peer row — click to jump)
 dotfiles · sage:vm-dev  ● working  (remote-only project, trailing)
```

Cross-machine attention falls out for free: `focus_attention` ranks blocked-oldest across the merged set, so `ctrl+shift+a` from one machine lands you on another's blocked agent — switch included.

## Tests
- **E2E** (`tests/peer_federation.rs`): two sandboxed servers + a fake-ssh shim; the peer row folds into the sidebar; a mouse click yields `SwitchServer{ssh_target}`.
- Unit: URL normalization (transport variants, scp/local paths), origin-less `dir:` fallback, subsection git-config reader, peers config validation, summary handler, poll-result merge.
- Full suite **1876 + e2e green**, clippy clean, fmt clean. Protocol-version pins updated across integration tests.

## Honest limits
- Sidebar freshness = gossip cadence (seconds), not frame-rate — fine for attention, the price of never muxing frames.
- Per-peer auth is plain SSH (the only acceptable transport given upstream #481).
- Cross-server `branch_session` / workspace-creation routing are follow-ups.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
