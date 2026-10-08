# Codex startup screens (#723)

These text fixtures reconstruct Codex 0.160.1 screens from upstream source and
snapshots. They are replayed by an isolated PTY harness in `delegate_codex.rs`,
not captured from a user's live server. The version numbers are upstream's
snapshot values.

- `startup-passive-banners.txt` combines the [pnpm update history box](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/tui/src/history_cell/snapshots/codex_tui__history_cell__tests__pnpm_update_available_history_cell_snapshot.snap)
  with the [weekly warning string](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/tui/src/chatwidget/rate_limits.rs#L67),
  wrapped to fit an 80-column pane, and an empty composer. Codex
  [adds that warning to history](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/tui/src/chatwidget/rate_limits.rs#L391).
  It does not mean quota is exhausted and must not block submission.
- `startup-update-dialog.txt` reproduces the [interactive update modal](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/tui/src/snapshots/codex_tui__update_prompt__tests__update_prompt_modal.snap),
  with trailing padding removed. Its selection marker is not a text composer.
  The brief must never be typed into this menu.
