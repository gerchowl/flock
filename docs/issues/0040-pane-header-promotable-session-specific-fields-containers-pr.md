---
number: 40
title: "pane header: promotable session-specific fields (containers, progress, custom KV)"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-11T07:47:17Z
closed: 2026-06-11T11:53:25Z
url: https://github.com/gerchowl/herdr/issues/40
---

# pane header: promotable session-specific fields (containers, progress, custom KV)

## Motivation

The pane header (reserved HUD strip: project · worktree · branch + last prompt) shows herdr-derived context only. Sessions know more: dev containers they started, long-task progress, ports, model names. Let the SESSION register fields into its own pane's header.

## Proposed approach

- New pane-scoped API: `herdr pane set-field <key> <value> [--ttl <secs>]` / `clear-field <key>` (JSON-RPC `pane.set_header_field`), keyed by the calling pane (env pane id + the existing peer-PID ancestry healing).
- Fields render as compact `key value` chips on the header context line (after branch/PR), middle-truncated under width pressure; optional TTL auto-expires stale chips (progress that stops updating shouldn't lie).
- State on TerminalState (like custom_status/last_prompt), persisted? — NO: ephemeral by default (a restored session's containers are unknown); revisit if demand.
- Hooks/agents can call it from anywhere inside the pane (the herdr CLI is on PATH in panes); e.g. a podman wrapper registering `pg ●`, a build script registering `build 73%`.

## Pitfalls
- Both event loops must consume any new request state (or ride update_terminal_state like custom_status does).
- Width: header line already carries project/worktree/branch/arrows/PR — chips need a hard budget + priority order.
- Abuse/leak: cap field count + value length per pane.
- Protocol: pane.report swallows unknown args? Additive RPC method — no version bump expected.

## References
Pane header HUD (PR #5/#7/#9 lineage), custom_status flow (set_hook_authority_with_custom_status), fork CLI surface (src/cli/pane.rs).

---

## Comments

### gerchowl — 2026-06-11T07:51:58Z

## Scope additions (user)

- **CLI discoverability**: `herdr pane set-field/clear-field` documented in `herdr pane --help` usage text (src/cli/pane.rs) + the website config/CLI docs — same discoverability treatment as the other fork CLI verbs.
- **Visible in navigation surfaces, not just the pane's own header**: fields ride `PaneDetail` (the same plumbing as custom_status), so chips render in (a) the sidebar agent panel rows, (b) the leader → workspace nav → pane selection lists (navigate overlay), (c) mobile pane lists — i.e. everywhere you *choose* panes, where 'which pane has the build at 73%?' is the actual question. Each surface gets its own width budget + truncation priority (header > agent panel > nav list).

### gerchowl — 2026-06-11T11:53:24Z

Shipped in PR #50.

