---
number: 56
title: "feat(sidebar): git-first restructure — project sections, primary rows, member tab-strip (#33)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-11T12:54:34Z
closed: 2026-06-11T13:08:43Z
merged: 2026-06-11T13:08:43Z
base: feat/sidebar-row-gap
head: git-first-sidebar
url: https://github.com/gerchowl/herdr/pull/56
---

# feat(sidebar): git-first restructure — project sections, primary rows, member tab-strip (#33)

Implements #33 (P0 scope + the locked design from the issue comments). Structurally resolves #29 — keep it open until this lands per its last comment.

## Stages (one commit each)

1. **Sectioning** — the spaces list groups git-first by project via `AppState::project_section_keys()`: every workspace lands in ONE project section (main checkout + linked worktrees + plain same-repo checkouts), remote rows keep folding into the same sections (#51), resolved non-git workspaces collect under the trailing positional `misc` section, and unresolved workspaces hold position (new `git_identity_resolved` flag — no misc flash). Collapse / auto-collapse / joins / counts / scope-current / drag-insert all moved onto the section key.
2. **Primary row** — tests pinning that the main checkout IS the section's selectable row (no synthetic header), carries the packed group-join rects (#54 wiring) across plain members too, and that members indent under it.
3. **Member tab-strip** — with `tab_mode = "workspace"` the tab bar renders the active session's members as `<ID> <name>` tabs: selected = accent, unselected = state-join head as DIM text tint only (#42/#43). Clicks reuse `switch_workspace`; `switch_tab`/`next_tab`/`previous_tab` branch inside their fork-owned bodies; prefix+1..9 work via a pre-match hook (see judgment calls). Tabs mode is byte-identical end to end.
4. **Two-level highlight** — the active row keeps the standard fill; its project's primary row carries the same always-on `surface_dim` currency fill (the current-server idiom, #36/#39); bold stays on the active row alone.

## Judgment calls

- **Key duality (documented in `project_section_keys`)**: rows GROUP by `project_key` (normalized origin URL — matches remote rows) whenever any member of the repo family has resolved one; membership/common-dir key is the fallback notion. The section's canonical KEY STRING prefers the first worktree-membership key so persisted collapse state survives restarts AND the mid-session moment git metadata resolves. The `dir:<name>` project fallback never merges across families (it would conflate unrelated same-named repos).
- **`misc` is positional** — no synthetic header row. Same principle as the primary rows (sections have no headers; the section is its rows), and it keeps `WorkspaceListEntry` and every hit-area/scroll/selection path stable. Remote-only project groups render before misc (git projects first, misc last).
- **Pending treatment**: `git_identity_resolved` on `Workspace`, set by the async sweep and the synchronous restore-time probe (restore counts as completed — its `None` is genuinely non-git). Pending rows hold storage position.
- **prefix+1..9 routing**: the upstream `SwitchTab` arm guards on `idx < ws.tabs.len()`, so a branch inside `switch_tab` alone can never see indices beyond the (usually single) real tab. Resolution: a small hook BEFORE the dispatch match in `execute_navigate_action_in_context` that defers entirely to fork-owned state methods — the match itself stays byte-identical with upstream (the #29 fork-strategist constraint). A missing member mirrors upstream's missing-tab no-op.
- **Strip semantics**: local members only (remote rows are a server switch), collapse state ignored (the strip always shows the whole session), single-member strips consume the cycle keys (workspace mode never cycles tabs the strip doesn't show), no tab reorder in workspace mode (sidebar order is the order), right-click offers the member's workspace menu, and mobile tab taps reroute through `switch_workspace_tab` so they keep their real-tab meaning.

## Tests

- Unit suite: **2054 passed, 0 failed** (one known headless_* parallel-load flake passed on rerun). fmt + clippy clean.
- Integration: `peer_federation` 6/6, `live_handoff` 16/16, `server_headless` 16/16.
- New coverage: sectioning (projects first / misc last / pending-not-misc / same-origin merge keyed by membership), primary-row join + indentation, strip labels + accent/state-tint styling, strip click + prefix+N + cycling (and tabs-mode byte-identity for each), two-level highlight, remote folding before misc.
