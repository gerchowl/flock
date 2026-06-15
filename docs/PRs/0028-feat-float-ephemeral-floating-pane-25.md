---
number: 28
title: "feat(float): ephemeral floating pane (#25)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-10T17:05:22Z
closed: 2026-06-10T17:18:04Z
merged: 2026-06-10T17:18:04Z
base: feat/sidebar-row-gap
head: float-pane
url: https://github.com/gerchowl/herdr/pull/28
---

# feat(float): ephemeral floating pane (#25)

PR 2 of 2 for #25 (spike: workspace-as-unit creation + floating throwaway pane). Implements the fixed design from the three-agent review consolidation.

## What

`keys.toggle_float` (unset by default) toggles a per-workspace ephemeral floating pane: a centered ~80%x70% overlay running the user's shell in the workspace's cwd, for quick throwaway commands.

Semantics (as locked in the review consolidation):
- First press: spawn + show. While visible: the same binding hides it (**hide-not-kill** — the PTY keeps running). Press again: re-show the same float.
- The float closes (PTY reaped, state removed) when its process exits; restart kills it implicitly (never persisted).
- **Esc is never a dismiss key** — it forwards into the float's shell (vi-mode/fzf keep working).
- One float per workspace.

## How (hard constraints from the reviews)

1. **State**: `AppState.floats: HashMap<workspace_id, FloatPane>` (`pane_id`, `terminal_id`, `visible`) — not on `Workspace`, not in `workspace.tabs`, not the `overlay_panes` split+persist pattern. Terminal metadata in `state.terminals`, PTY runtime in the app-level registry, like layout panes.
2. **Zero new `request_*` fields**: the spawn happens synchronously from the input path via the standalone `TerminalRuntime::spawn` primitive (the same one `Tab::split_focused_with_runtime` calls under the hood — no extraction was needed, it already exists with no layout coupling). Process-exit cleanup rides the shared `App::handle_internal_event` `PaneDied` preprocessing (`src/app/api.rs`), which runs in both the monolithic and headless loops; runtime teardown reuses `terminal_runtime_shutdowns` + `shutdown_detached_terminal_runtimes`.
3. **No new `Mode` variant**: rendered as a post-pass in `render_with_runtime_registry` after `render_panes`/`render_notifications` (above panes/notifications, below modal overlays). Fill is `palette.panel_bg` with `surface_dim` fallback for transparent themes; bordered block titled with the float's cwd; cells painted through the same `TerminalRuntime::render` path panes use. PTY resize mirrors the per-frame pane resize reconciliation in `compute_view_internal` (and the mobile compute path), gated on `resize_panes` so non-foreground headless clients never fight over the float's size.
4. **Input routing**: early hook in `prepare_terminal_key_forward` (the shared seam under both `handle_terminal_key` and `handle_terminal_key_headless`): while the active workspace's float is visible, (a) a direct-bound `toggle_float` hides it, (b) the prefix key still enters Prefix mode (so a prefix-bound toggle works — covered by test), (c) everything else encodes against and routes to the float's runtime via a new `PreparedInputTarget::Float` variant. Paste (both loops) and double-prefix passthrough route to the float too, via a shared `terminal_input_runtime()` helper. `ToggleFloat` is intercepted at the App level in the direct/prefix/navigate dispatchers, mirroring the existing `EditScrollback` pattern (`execute_navigate_action_in_context` arm stays a no-op).
5. **Alias landmine**: spawn goes through `AppState::register_float`, which calls `remove_alias_shadowed_by_new_pane(float_pane_id)`; teardown also purges aliases *targeting* the float. Floats are registered nowhere that enumerates workspace panes, so `find_pane`/ancestry/snapshot walkers exclude them by construction (pinned by test).
6. **Config**: `toggle_float: BindingConfig` in `KeysConfig` (default empty) + `toggle_float: ActionKeybinds` following the `toggle_collapse_all` pattern. Spawn/hide never call `mark_session_dirty`/`schedule_session_save` (asserted in tests). Also added: keybind-help entry and a website config-reference paragraph (issue P1, additive).

## Merge-gate tests (all present, all passing)

- **(a) Snapshot exclusion**: synthetic float whose pane id is in no workspace; `persist::capture` + `capture_history` serialized output contains neither the float pane id nor its terminal id (`app::float::tests::floats_are_excluded_from_session_snapshots`).
- **(b) Alias purge on spawn**: pre-inserted alias whose key collides with the float's pane id is gone after `register_float` (`register_float_purges_alias_shadowed_by_new_pane_id`), plus the inverse (`float_removal_purges_aliases_targeting_the_float`).
- **(c) Toggle state machine**: pure `AppState` transitions spawn→visible, toggle→hidden(still present), toggle→visible, exit→removed (`toggle_state_machine_spawn_hide_show_exit`); only the PTY spawn lives at App level.
- **Routing/exit integration** (channel-backed test runtimes, no PTY): keys route to the float not the focused pane; direct toggle hides then re-shows the same float; Esc forwards instead of dismissing; prefix-bound toggle hides via Prefix mode; `PaneDied` through `handle_internal_event` reaps float + terminal + runtime without touching the workspace tree or dirtying the session.
- **(d)** Full suite: **1918 passed, 0 failed** (1905 baseline + 13 new); `cargo clippy --all-targets`: zero warnings; `cargo fmt --check` clean. The headless `request_*`/forwarding source-scan guards stay green.

## Deviations / known limitations (none from the hard constraints)

- Mouse events are not float-aware yet: clicks/wheel still hit the layout panes underneath, and Shift+PageUp host-scrollback keys forward into the float rather than scrolling herdr's scrollback. Both fall under the issue's P1 "Float UX: scrollback, copy-mode parity" follow-up.
- `float_title` shows the float's spawn cwd (terminal metadata), not the live foreground cwd — kept simple deliberately.

Part of #25 (P0 float scope). Do not merge ahead of review.
