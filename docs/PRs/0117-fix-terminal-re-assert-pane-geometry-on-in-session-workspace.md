---
number: 117
title: "fix(terminal): re-assert pane geometry on in-session workspace/pane switch (#114)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-13T21:58:58Z
closed: 2026-06-13T21:59:33Z
merged: 2026-06-13T21:59:33Z
base: master
head: geometry-on-switch
url: https://github.com/gerchowl/herdr/pull/117
---

# fix(terminal): re-assert pane geometry on in-session workspace/pane switch (#114)

Fixes #114. In-session sibling of #82.

## Root cause

In-session workspace/tab/pane switches do not re-assert geometry to the just-activated pane. A multi-pane workspace self-heals because the user dragging the split handle fires a `Resize` that flows into the layout pipeline. A single-pane workspace has no such trigger, so the pane keeps stale narrow geometry until an external window-resize event arrives.

PR #82 fixed the same symptom on the slot-flip (warm-server switch) path. This PR fixes the local-switch sibling using the same mechanism.

## Fix

In `src/server/headless.rs`:

- New helper `active_focus_snapshot()` captures `(workspace_id, focused pane_id, active tab index)`.
- New helper `reassert_geometry_if_active_focus_changed(before)` compares the snapshot and calls `resize_shared_runtime_to_effective_size()` only when focus actually changed. The server's resize handler reflows every pane via the vendored libghostty-vt reflowing resize — this is the same downstream layer #82's layer-1 fix relied on.
- Wire into both switch chokepoints:
  - `ServerEvent::ClientInput` (keybind path): snapshot before `route_client_events`, re-assert after `sync_foreground_client_state`.
  - `handle_api_request_with_shutdown_check` (`workspace.focus`, `tab.focus`, etc.): snapshot after `sync_foreground_client_state`, re-assert after sending the response.

Snapshot equality gates the call so no-op input does not trigger a redundant layout pass.

## Tests

In `src/server/headless.rs` tests:

- `in_session_workspace_switch_reasserts_pane_geometry_single_pane` — the single-pane case (the broken case in the issue).
- `in_session_tab_switch_reasserts_pane_geometry` — tab switch within the same workspace.
- `in_session_workspace_switch_does_not_reassert_when_focus_unchanged` — no-op gate.
- `workspace_focus_via_api_reasserts_single_pane_geometry_end_to_end` — drives the full `Method::WorkspaceFocus` API path; fails without the fix (verified by toggling the call off).

## Suites (devShell, `--test-threads=2`)

- unit (`--bin herdr`): 2196 passed.
- `server_headless`: 16 passed.
- `client_mode`: 19 passed.
- `live_handoff`: 16 passed.
- `peer_federation`: 7 passed.
- `cargo fmt` clean, `cargo clippy --all-targets -- -D warnings` clean.

## References

#82 (slot-flip geometry + reflow), #77 (the upstream symptom that #82 closed), `resize_shared_runtime_to_effective_size`, `current_pane_focus_target`.
