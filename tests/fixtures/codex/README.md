# Codex startup screens (#723)

These fixtures derive from the real Codex 0.160.1 captures added by #748 in
[`../codex-submit-721/`](../codex-submit-721/README.md). They preserve the
captures' prompt, footer, directory truncation and spacing. The added warning
and interactive update controls come from Codex 0.160.1 source. These composed
screens are replayed by an isolated PTY harness in `delegate_codex.rs`, with
40 rows so the complete startup history and controls fit on screen.

- `startup-passive-banners.txt` is `../codex-submit-721/inline-idle.txt` with
  the [weekly warning string](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/tui/src/chatwidget/rate_limits.rs#L67)
  inserted immediately above the prompt, wrapped to the capture's width.
  The real capture already contains the passive update box. Its empty
  `› Ask Codex to do anything` prompt, blank row and
  `GPT-6.1-Sol default · /fixture/codex-721/…` footer are unchanged; there is
  no shortcut footer in this inline capture. Codex
  [adds the quota warning to history](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/tui/src/chatwidget/rate_limits.rs#L391).
  It does not mean quota is exhausted and must not block submission.
- `startup-update-dialog.txt` retains the history and spacing before the prompt
  from `../codex-submit-721/alt-idle.txt`, then replaces the composer and its
  footer with the [interactive update modal](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/tui/src/snapshots/codex_tui__update_prompt__tests__update_prompt_modal.snap),
  with trailing padding removed. This is a composed update-menu case, not a
  claim that #748 captured the update dialog. The real `alt-model.txt` capture
  demonstrates that an active menu has selection controls and no composer
  footer. The update menu's selection marker must likewise never receive a brief.
