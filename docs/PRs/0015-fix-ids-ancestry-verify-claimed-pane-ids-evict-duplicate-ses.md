---
number: 15
title: "fix(ids): ancestry-verify claimed pane ids; evict duplicate session refs"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-07T00:49:46Z
closed: 2026-06-07T00:52:59Z
merged: 2026-06-07T00:52:59Z
base: feat/sidebar-row-gap
head: keyboard-shorcuts
url: https://github.com/gerchowl/herdr/pull/15
---

# fix(ids): ancestry-verify claimed pane ids; evict duplicate session refs

## Bug

`ctrl+shift+b` (branch_session) failed with **"no resumable agent session"** on panes directly attached to a space, while worktree panes worked — looking like a tier system between the two. There is no tier: those panes are simply the *oldest*, and their agents' env-baked `HERDR_PANE_ID` predates pane renumbering.

Live evidence on the real session: a claude in pane 9 carried env `p_10` — a raw id that now names a *different live pane* (10). The peer-PID heal (#13) only fires on dangling ids, so the report was delivered to pane 10 with full confidence: pane 10 accumulated a foreign session, pane 9 starved → branch_session guard tripped. Three panes ended up claiming the same session id.

## Fix

1. **Verify, don't just heal** — with a peer pid, the process tree is the authority. A parsed pane claim is accepted only when the caller is a descendant of the claimed pane's child process; a positive ancestry mismatch overrides the claim. No ancestry evidence (re-parented/daemonized callers) keeps the claim — no regression for unverifiable reporters.
2. **No alias memoization for colliding ids** — aliasing a raw id that still names a live pane would shadow that pane's own truthful reports. Dangling ids keep the existing memoize-into-alias fast path.
3. **Session-ref uniqueness sweep** — applying a session ref to one terminal evicts the identical ref from every other terminal, at the shared `update_terminal_state` chokepoint (covers both request loops). This self-heals already-poisoned state on the next report from each agent.

## Testing

- 5 new unit tests (collision override, fast-path verify, no-evidence claim survival, dangling memoization, duplicate eviction); full suite 1867/1867 green, fmt+clippy clean.
- e2e in a sandboxed client/server (tui-probe): report from inside pane 2 claiming `p_1` → lands on pane 2, pane 1 untouched; truthful `p_1` report → session moves to pane 1 **and** pane 2's stale copy is evicted.
