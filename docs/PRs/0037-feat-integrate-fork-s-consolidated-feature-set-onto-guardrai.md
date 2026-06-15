---
number: 37
title: "feat: integrate fork's consolidated feature set onto guardrails base (sidebar-row-gap → master)"
kind: pr
state: OPEN
author: gerchowl
labels: []
created: 2026-06-11T06:29:32Z
closed: 
merged: 
base: master
head: integration/fork-consolidated
url: https://github.com/gerchowl/herdr/pull/37
---

# feat: integrate fork's consolidated feature set onto guardrails base (sidebar-row-gap → master)

## Summary

Lands the fork's full feature set on master. `feat/sidebar-row-gap` has been the fork's de-facto master — every feature merged via its internal PR chain (#17–#31) lives there. This PR brings all of it onto `origin/master`, rebased on top of [PR #35](https://github.com/gerchowl/herdr/pull/35)'s clean guardrails adoption.

**Depends on #35** — merge that first. This PR's base implicitly includes #35's content; merging this on top of a master that doesn't yet have #35 will replay #35's commit too, which is harmless but means the order matters for clean history.

## What's in here

Everything that landed on `feat/sidebar-row-gap` via its sub-PRs:

- **#17 keyboard-shorcuts** — collapse-all keybind + auto-collapse setting + traffic-light counts in the sidebar
- **#18 feat/peer-federation** — C′ symmetric peer federation with folded sidebar + switch-on-select; protocol pins bumped 12→13
- **#19 feat/peer-servers** — per-machine health, free-latency, switch-on-click servers section
- **#20 fix-status-flicker** — sidebar status row no longer flickers between spinner text and state label
- **#21 guardrails-devenv** — wire guardrails into the flake devShell (superseded by #35)
- **#22 gate-promotions** — hard-gate promotions (superseded by #35)
- **#23 feat/scrollback-keys** — Shift+PageUp/Down/Home/End + Shift+wheel scrollback navigation
- **#24 hud-polish** — sidebar + status line paint panel_bg chrome; stable hostname; header divider; prompt-expand keybind + in-place expand
- **#25 (split)** — tab_mode=workspace (new_tab spawns sibling workspace) + ephemeral per-workspace float pane
- **#26 tab-mode-workspace** — persistence round-trip tests
- **#27 header-owner** — owner-qualify the header repo segment (`org\|person/repo`)
- **#28 float-pane** — reap float PTYs on workspace close
- **#31 fork-build-identity** — bake fork build identity into the version string

Plus the pane-ancestry id verification, agent-scoped attention, and worktree kill from earlier on the branch.

## Conflict resolution (full summary in the merge commit)

PR #35's content was preferred for the guardrails configs and the tracing facade extensions; `feat/sidebar-row-gap` was preferred for everything else (it has peer-pid plumbing, additional `Method` enum variants, etc.).

Key decisions:

- **`.pre-commit-config.yaml`, `deny.toml`, `flake.nix`, `flake.lock`** — PR #35's cleaner version (exclude-based gate patterns, all three agent-drift gates hard, RUSTSEC ignore).
- **`src/logging.rs`** — auto-merged: sidebar-row-gap's base facade + PR #35's Stopwatch/slow-path helpers.
- **`src/api/server.rs`** — sidebar-row-gap base (preserves the `peer_pid` plumbing through `dispatch_to_app{_with_timeout}`, `handle_request`, `ApiRequestMessage`, and `socket_peer_pid`). Layered PR #35's logging delta on top: import `method_name as api_method_name` from `api/mod.rs`, drop the duplicated local helper, wrap the dispatch arm in `Stopwatch::start()` + `api_server_roundtrip_observed`.
- **`src/api/mod.rs`** — added `PeersSummary` and `PaneReportPrompt` arms to the canonical `method_name` match so it stays exhaustive against sidebar-row-gap's extended `Method` enum.
- **5 cosmetic conflicts** in `src/{app/actions.rs, input/encode.rs, integration/mod.rs, protocol/render_ansi.rs, remote.rs}` — kept sidebar-row-gap's inline `guardrails-ok` form.
- **`.gitignore`** — added `.direnv/` so the locally-committed direnv state that snuck onto `gate-promotions` doesn't keep coming back. Discarded the 8-file `.direnv/` tree the merge brought in.

## Verification

```
no fake/placeholder implementations......................................Passed
no debug-print leftovers (use the tracing facade)........................Passed
no commented-out code....................................................Passed
secrets scan.............................................................Passed
rustfmt..................................................................Passed
clippy (-D warnings).....................................................Passed
cargo-deny check.........................................................advisories ok, bans ok, licenses ok, sources ok
cargo check --all-targets................................................clean
cargo test --no-run......................................................clean
```

## Branches superseded after merge

These can be deleted on `origin` once this lands (their content is included):

- \`feat/sidebar-row-gap\` (this PR's base)
- \`fix-status-flicker\`, \`float-pane\`, \`fork-build-identity\`, \`gate-promotions\`, \`guardrails-devenv\`, \`header-owner\`, \`hud-polish\`, \`keyboard-shorcuts\`, \`pane-header-hud\`, \`tab-mode-workspace\`, \`top-prompt-float\`
- \`latest\` is a rolling pointer — leave it, but it'll be stale until repointed at the new master tip

## Test plan

- [ ] CI green
- [ ] \`nix develop\` enters cleanly; \`prek install\` wires hooks
- [ ] \`prek run --all-files\` passes
- [ ] Smoke: launch herdr, verify panes/sidebar/peers/float-pane all work
- [ ] Smoke: induce a slow render and confirm WARN-level \`render.frame\` slow-path event fires
- [ ] Smoke: send an API roundtrip and confirm \`api.request.roundtrip\` event fires with peer_pid

🤖 Generated with [Claude Code](https://claude.com/claude-code)
