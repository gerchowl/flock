---
number: 98
title: "feat(pane): prompt history scrollback with recaps (#96)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-12T16:32:14Z
closed: 2026-06-12T16:32:48Z
merged: 2026-06-12T16:32:48Z
base: master
head: prompt-history
url: https://github.com/gerchowl/herdr/pull/98
---

# feat(pane): prompt history scrollback with recaps (#96)

Implements #96. Per-pane prompt + recap history with a bounded scrollable expanded panel.

## What changes

- `pane.report_prompt` now APPENDS a timestamped entry to a per-pane history ring instead of just overwriting `last_prompt` (the legacy field still mirrors the LATEST prompt — collapsed-header byte-identity preserved).
- New additive RPC `pane.report_recap` appends a recap entry (visually distinct from prompts). API just stores it; the Claude Stop-hook wiring follows later, per the issue.
- Expanding the header (prefix+shift+e or click) now opens a **bounded scrollable panel** over the pane: bordered, capped at ~70% of pane inner height, history above, latest pinned at bottom. Wheel / PageUp / PageDown / Home / End / Esc are routed to the panel (scroll state on AppState, reset on close).
- Cap: ~1000 RENDERED lines per pane (chrome line + body lines); drops oldest WHOLE entries on overflow.
- History is **ephemeral** — never persisted into snapshots (`last_prompt` continues to ride snapshots for v0 restore compatibility).
- Same `update_terminal_state` chokepoint as #40 / #50 — both event loops share the path.

## Surfaces touched

- `src/api/schema.rs` (additive `PaneReportRecap`), `src/api/server.rs`, `src/app/api.rs` dispatch, `src/app/api/panes.rs` handler.
- `src/events.rs` (new `HookRecapReported`), `src/app/actions.rs` (event handlers + scroll helpers + `close_prompt_history_panel`).
- `src/terminal/prompt_history.rs` (new module: `PromptHistoryEntry`, cap, append, relative-age), wired into `TerminalState`.
- `src/ui/panes.rs` (panel renderer + rect helper, caret flip on collapsed header).
- `src/app/input/mouse.rs` (wheel → panel routing), `src/app/input/terminal.rs` (PageUp/PageDown/Home/End/Esc → panel routing).
- `src/cli/pane.rs` (new `pane report-recap` subcommand + help text).
- `website/src/content/docs/socket-api.mdx` (new "Prompt history scrollback" section + raw-methods table entry).

## Tests

- `terminal::state::prompt_history::tests::*` — append/cap/drop-oldest, relative-age buckets, trimmed line counting.
- `app::api::panes::tests::report_prompt_appends_timestamped_history_entry` — append round-trip + last_prompt mirror.
- `app::api::panes::tests::report_recap_round_trips_through_update_terminal_state` — recap rides the chokepoint, does NOT touch `last_prompt`.
- `app::api::panes::tests::report_recap_rejects_unknown_pane_and_invalid_agent`.
- `app::api::panes::tests::prompt_history_drops_oldest_whole_entries_past_cap`.
- `server::headless::tests::headless_handle_internal_event_appends_prompt_history` — both-loops guarantee (HookPromptReported + HookRecapReported routed through the headless event handler).
- `persist::snapshot::tests::capture_contract_excludes_prompt_history_scrollback` — snapshot exclusion contract.
- `app::actions::tests::scroll_prompt_history_clamps_and_resets_on_close`.
- `ui::panes::tests::prompt_history_panel_{rect_returns_none_when_panel_closed,keeps_latest_pinned_at_bottom}`.
- `cli::pane::tests::pane_help_lists_report_recap_and_history_explanation`.

Full suite: **2154 passed, 0 failed**. `--test server_headless` + `--test client_mode` green. fmt + `clippy --all-targets -D warnings` clean.

## Test plan
- [ ] `cargo test --bin herdr -- --test-threads=2`
- [ ] `cargo test --test server_headless --test client_mode -- --test-threads=2`
- [ ] `cargo fmt --check && cargo clippy --all-targets -- -D warnings`
- [ ] Manually: open a pane with several `pane.report_prompt` + `pane.report_recap` calls, verify the expanded panel pins the latest entry, wheel/PageKeys/Esc work, Esc closes, collapsed view byte-identical to before when there is a single entry.
