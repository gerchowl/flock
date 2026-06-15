---
number: 25
title: "spike: workspace-as-unit creation + floating throwaway pane; de-emphasize tabs"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-10T15:50:44Z
closed: 2026-06-10T17:18:26Z
url: https://github.com/gerchowl/herdr/issues/25
---

# spike: workspace-as-unit creation + floating throwaway pane; de-emphasize tabs

## Motivation / why

herdr's hierarchy is **Space (project/worktree group) → Workspace (sidebar unit; carries agent state, attention, git identity) → Tab (view within a workspace) → Pane (split)**. For an agent-centric tool the workspace is the meaningful unit; the tab layer is the weakest-justified: its only distinct job is "multiple layouts in one sidebar row", which sibling workspaces (visible rows, per-agent attention — what we actually want) or panes already cover. Meanwhile there is **no lightweight primitive for a throwaway command** — a quick `git status`/grep needs a persistent pane (disrupts layout, sticks around) or a tab (heavier still).

User direction: *"new tab = new workspace and vice versa? what is the case for differentiation? a quick cmd in the same workspace should be a pane — we could adopt a 'float' like zellij offers for throwaway execution."*

## Decision / proposed approach

Three moves, deliberately **keeping the Tab data model intact** (see Pitfalls — upstream merge friction dominates):

1. **Workspace-as-unit creation flow**: a config mode (e.g. `[ui] tab_mode = "workspace"`) where `new_tab` (prefix+c) creates a **sibling workspace in the same space group** (same checkout cwd → same space key → groups in the sidebar) instead of a tab. Tabs remain loadable/renderable; the *creation* path changes.
2. **Floating throwaway pane** (zellij-style): a keybind opens a centered overlay pane (~80%×70%) running the shell in the workspace cwd; `Esc`/keybind dismisses. **Not persisted** (ephemeral by definition), not a layout member, closes when the process exits. One float per workspace initially.
3. **De-emphasize tabs in the UI**: auto-hide the tab bar when a workspace has exactly one tab — this is literally upstream issue ogulcancelik/herdr#448, so it can be built upstream-compatible (and potentially upstreamed to delete divergence).

## What already exists

- Tab layer: `src/workspace/tab.rs` (419 LoC), `tabs: Vec<Tab>` in `src/workspace.rs:106`, full keybind suite (`new_tab` prefix+c, `rename_tab`, `next_tab`, `switch_tab` prefix+1..9, `close_tab`, `prompt_new_tab_name`) in `src/config/model.rs:325-475`.
- Persistence: `tabs: Vec<TabSnapshot>` + `active_tab` per workspace in `src/persist/snapshot.rs:65-68`; a `LegacyWorkspaceSnapshot` migration pattern already exists (`snapshot.rs:71`).
- Sibling-workspace creation in a space already exists (branch_session / new_worktree flows spawn grouped workspaces); space grouping by key is the sidebar's native unit (collapse-all, traffic lights — fork PRs #17).
- Overlay rendering precedent: navigate/prefix/settings overlays in `src/ui.rs:406-433`; the expanded-prompt header overlay (fork PR #24).
- No float subsystem exists. Upstream has no floating-pane issue; #448 covers tab-bar auto-hide.

## Scope

**P0**
- [ ] `[ui] tab_mode = "tabs" | "workspace"` (default `"tabs"` — no behavior change unconfigured)
- [ ] In workspace mode: `new_tab` creates a sibling workspace in the current space (same cwd); `switch_tab`/`next_tab` cycle the space group's workspaces
- [ ] Tab bar auto-hides when a workspace has exactly one tab (works in both modes; upstream-#448-compatible)
- [ ] Float pane: `toggle_float` keybind (unset by default), centered overlay, shell in workspace cwd, ephemeral (never persisted), closes on process exit; Esc dismisses (kill or hide — reviewer input wanted)
- [ ] Both event loops (App deferred-request loop + headless mirror) handle any new request state
- [ ] Existing multi-tab snapshots restore exactly as today

**P1**
- [ ] Float UX: scrollback, copy-mode parity, resize with terminal
- [ ] Settings-pane toggle for tab_mode
- [ ] Docs (website/config reference)

**P2**
- [ ] Offer tab-bar auto-hide upstream (#448) to shrink divergence
- [ ] Evaluate retiring tab keybinds from default config in workspace mode

## Pitfalls

1. **Upstream merge friction is the dominant constraint.** The fork tracks ogulcancelik/herdr daily; upstream actively develops tabs (#303 tab-bar status dots, #224 cross-workspace tab cycling, #448). Deleting `Tab` from the model = permanent conflict surface across UI/model/persistence. Hence: change *creation semantics + visibility*, not the data model.
2. **Dual event-loop trap**: the headless server duplicates App's deferred-request loop (`src/server/headless.rs` ~439 mirrors `src/app/mod.rs` ~765). Any `state.request_*` added for float spawning MUST be consumed in both. (This silently broke branch_session once.)
3. **Persistence**: floats must be excluded from snapshots *and* from the pane-id alias map GC assumptions; tabs in existing snapshots must restore unchanged (no forced migration).
4. **Float input routing**: focus model is mode-based; a float needs unambiguous key routing (terminal mode into the float PTY, Esc-to-dismiss without eating the agent's Esc — prefix-based dismiss may be safer).
5. **Federation/API surfaces**: peer workspace summaries, `herdr` CLI (`pane list` etc.), and the mobile UI enumerate tabs/panes — floats must not leak into counts or break schemas (protocol is versioned; avoid a bump if possible).
6. **switch_tab (prefix+1..9) overload** in workspace mode: cycling group workspaces vs global workspace indexes — ambiguity to resolve explicitly.
7. **Kitty graphics / scrollback** inside an overlay pane — the VT pipeline assumes rect-stable panes; float resize must reuse the existing resize path.

## Acceptance criteria

- [ ] Default config: zero behavior change; full suite green
- [ ] `tab_mode = "workspace"`: prefix+c produces a sibling workspace grouped under the same space, sidebar shows it as a normal group row
- [ ] Tab bar absent for single-tab workspaces; reappears at 2+
- [ ] Float: opens ≤500ms, runs interactive commands, never appears in session files, survives nothing (gone after restart), closes cleanly on exit
- [ ] e2e: tui-probe runbook covering create-sibling, float open/run/dismiss, restore-of-legacy-multi-tab snapshot
- [ ] All commits pass guardrails gates; PR(s) into feat/sidebar-row-gap

## References

- Upstream: ogulcancelik/herdr#448 (tab-bar auto-hide), #303, #224 (active tab development = merge-friction evidence)
- Fork: PR #17 (space-group collapse), #24 (HUD overlays), branch_session (sibling-workspace creation flow)
- `src/workspace/tab.rs`, `src/workspace.rs:106`, `src/persist/snapshot.rs:48-75`, `src/config/model.rs:325-475`, `src/ui.rs:376-435`, `src/server/headless.rs` (dual-loop)
- Prior art: zellij floating panes (toggle-visibility semantics), tmux display-popup (ephemeral run-and-exit semantics — closer to our "throwaway" intent)

---

## Comments

### gerchowl — 2026-06-10T15:58:43Z

## Review consolidation (3 fresh agents: multiplexer architect · fork strategist · state/persistence)

**All three: proceed.** Scope updates from the reviews:

### Unanimous corrections adopted
1. **Grouping is NOT cwd-driven** (caught independently twice): sibling creation must explicitly clone/synthesize `WorktreeSpaceMembership` (`workspace.rs:33,101`) — and pin `identity_cwd` to `worktree_space.checkout_path` so a root-pane `cd` can't break group membership across restart (`resolved_identity_cwd_from`, `workspace.rs:582`).
2. **Float semantics fixed**: toggle = hide-not-kill (PTY lives), process exit closes (via the shared `handle_internal_event` path, `api.rs:179` — both loops), restart kills (never persisted). **No Esc binding** — Esc belongs to the shell's inhabitants (vi-mode/fzf); dismiss = same `toggle_float` keybind. One float per workspace.
3. **Float structure**: state in `AppState.floats: HashMap<workspace_id, FloatPane>` (NOT on Workspace, NOT the `overlay_panes` split+zoom pattern — that pattern persists by construction); greenfield modules; **no new `Mode` variant**; exactly 3 shared touch points (render call after `render_panes`, early-return key hook in `handle_terminal_key`, additive config/keybind fields). **Zero new `request_*` fields** → dual-loop trap dodged by design.
4. **Alias-collision guard** (real landmine): float ids share `NEXT_PANE_ID` with restored alias keys; a claude-in-float hook could alias-route its session ref onto a persistent pane and get snapshotted. Guard: `remove_alias_shadowed_by_new_pane(float_id)` on spawn + floats excluded from `find_pane`/ancestry/alias targets.
5. **Name prompt**: same-cwd siblings render identical labels — reuse `prompt_new_tab_name` semantics for sibling naming.

### Cut from scope
- ~~Tab-bar auto-hide (#448)~~ — upstream has **already approved a contributor PR for exactly this** (imminent). In-fork = guaranteed conflict. Watch upstream; throwaway patch only if it stalls 4–6 weeks.

### Deferred to follow-up (genuinely contested)
- `switch_tab`/`prefix+1..9` group-local rerouting: multiplexer-architect wants it (muscle memory, tmux session-local numbering); fork-strategist vetoes touching the navigate.rs dispatch (upstream's hottest conflict surface; #224 just churned cycling). Possible synthesis: branch inside the called state method (dispatch arm stays byte-identical). → separate issue after the core lands; `SwitchWorkspace`/`NextWorkspace` cover sibling navigation meanwhile.

### Merge-gate tests (from the persistence review)
1. Float snapshot exclusion (capture + capture_history contain neither float pane id nor content)
2. Alias-collision: pre-inserted alias `{float_raw → layout_pane}` purged on float spawn; foreign report doesn't mutate the layout pane
3. Restore-fidelity golden: multi-tab fixture restores capture-identical under `tab_mode = "workspace"`
4. Sibling round-trip: space-key sharing + snapshot round-trip + the root-pane-cd identity-drift case
5. Source-scan guard: feature adds no `request_*` field absent from either event loop

Implementation proceeding (user pre-authorized landing the unambiguous core): **PR 1** `tab_mode` workspace-sibling creation, **PR 2** float pane.

### gerchowl — 2026-06-10T17:18:25Z

## Landed

- **PR #26** — `[ui] tab_mode = "workspace"`: new_tab spawns a sibling workspace (membership stamped explicitly, cwd pinned to checkout, name prompt carries over). Post-impl review: approve; both nits addressed (membership snapshot round-trip test + double-save rationale).
- **PR #28** — ephemeral floating pane (`keys.toggle_float`): hide-not-kill toggle, exit via shared internal-event path (both loops), alias-collision guard, never persisted, no Mode variant. Post-impl review found one blocker — **float PTY leak on workspace close** — fixed at the shared teardown chokepoints (terminal_ids_for_workspace + remove_unattached_terminal_ids) with a hidden-float reap test.

## Follow-ups filed
- #29 group-local tab-switching keys in workspace mode (the contested item, deferred by design)
- #30 float UX round 2 (mouse, host scrollback, live cwd title; + float resize-while-hidden)

## Deliberately not done
- Tab-bar auto-hide: upstream #448 has an approved contributor PR imminent — watching upstream instead of carrying a conflicting in-fork patch.

Decision trail: issue body (design) → review consolidation comment (3-agent panel) → PR bodies (implementation) → this comment (post-impl reviews + outcomes). Closing.

