---
number: 110
title: "feat(sidebar): new blank workspace from home + spaces-header new + right-click branch (#105)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-13T09:52:16Z
closed: 2026-06-13T09:52:59Z
merged: 2026-06-13T09:52:59Z
base: master
head: workspace-creation-ux
url: https://github.com/gerchowl/herdr/pull/110
---

# feat(sidebar): new blank workspace from home + spaces-header new + right-click branch (#105)

## Summary

Three workspace-creation UX changes (#105) that share one sidebar surface.

1. **New BLANK workspace from $HOME.** `AppState::request_blank_workspace_at_home` stages the cwd via the existing `request_new_workspace_cwd` seam, which both event loops (`src/app/mod.rs` and `src/server/headless.rs`) already drain into `create_workspace_with_options`. No new field, no new event-loop branch. Distinct from `create_sibling_workspace` (which pins to a space checkout) and from `create_workspace` (which follows the `new_terminal_cwd` policy).
2. **`new` affordance on the spaces header.** Rendered to the left of the `all`/`current` scope toggle, one column gap apart, in BOTH tab modes. The tabs-mode footer `new` stays unchanged (it creates tabs); the header `new` creates BLANK workspaces. The hit area is carved out of the header-row scope-toggle hit-area so clicks don't collide.
3. **Right-click any workspace/space row -> `Branch session`.** Extends the existing `ContextMenuKind::Workspace` and `::GitWorkspace` variants with a `branchable` flag computed at open time via `AppState::workspace_branchable` (mirrors the App-level `focused_branch_plan` guard). When the row has no resumable agent the entry is omitted, so `apply_context_menu_action` only ever sees it for a valid target. Forwards the row's `ws_idx` into `request_branch_session`, the same seam `prefix+y` uses.

## Judgment calls

- `request_new_workspace_cwd` was already wired through both event loops, so reusing it (#1) is strictly cheaper than inventing a `create_blank_workspace` field. The seam stays unchanged; only a new `AppState` helper stages it.
- The footer `new` (#41) stays as-is in tabs mode. The header `new` (#105) supersedes it in workspace mode and complements it in tabs mode. Spec called out either option; this matches each mode's existing creation grammar (tabs-mode footer makes tabs, header makes blank workspaces).
- For the disabled/grayed Branch session entry, the menu omits the entry when no resumable agent exists (rather than rendering it grayed). This keeps `apply_context_menu_action` honest -- a click on the entry always succeeds -- and the existing `open_branch_session_dialog` notice still catches stale-rendered paths.

## Test plan

- [x] `request_blank_workspace_at_home` stages `$HOME` into the shared deferred-request seam (both event loops consume).
- [x] Header `new` rect renders in both tab modes; click sets the request cwd; scope toggle is not collaterally fired.
- [x] `workspace_branchable` is false on a fresh workspace, true once a persisted agent session is attached, false for an out-of-range ws_idx.
- [x] `Branch session` context-menu item dispatches `request_branch_session` for the row's ws_idx, not the active workspace.
- [x] Full unit suite (`cargo test --bin herdr --test-threads=2`) green; `cargo test --test peer_federation --test server_headless` green.
- [x] `cargo fmt` clean, `cargo clippy --all-targets --locked -- -D warnings` clean.
