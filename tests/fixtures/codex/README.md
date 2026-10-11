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

# Codex slow-model notice (#763)

`slow-model-menu.txt` replays the owner-reported Codex 0.160.1 screen from
#763 through an isolated flk pane, captured with `flk pane read --source recent
--format text` before changing the detector. The ANSI capture had identical
content. The 50-column pane wraps the explanatory text and bottom notice.
This is a replay of the issue's screen, not a fresh live Codex capture.
The title, three choices and final no-action notice identify the active menu.
The selection marker may move between choices; none requires action.

# Codex folder-trust dialog (#828)

`folder-trust.txt` is a live Codex 0.160.1 capture (`tmux capture-pane`, 100
columns) of the first start in a subdirectory of a git repository Codex had
never trusted, run with a throwaway `CODEX_HOME` and no flk server. Only the
two scratch paths were replaced by `/fixture/codex-828/…`; the wording, the
selection marker, the blank rows and the `enter continue · esc quit` footer are
unchanged. A start at the repository root itself shows the same dialog without
the `Note:` paragraph. The dialog has no composer, so a brief typed at it
answers the dialog.
