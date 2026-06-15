---
number: 144
title: "feat(float): replies + recaps as distinct kinds, Stop-hook capture, task-notification filter"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-14T13:51:42Z
closed: 2026-06-14T13:56:00Z
merged: 2026-06-14T13:56:00Z
base: master
head: feat/float-replies-and-recaps
url: https://github.com/gerchowl/herdr/pull/144
---

# feat(float): replies + recaps as distinct kinds, Stop-hook capture, task-notification filter

## What the float looks like after this

A glanceable three-tone scrollback:
- **Prompt** — the user's input, `palette.subtext0` (dim grey).
- **Reply** — the agent's last assistant message, `palette.blue` (cool).
- **Recap** — the disciplined `※ recap: …` summary line, `palette.mauve` (warm accent, prefix bolded).

Today: only prompts are captured, no replies, no recaps; harness `<task-notification>` XML leaks in as fake "prompts" so the float reads as machine noise (the bug the user surfaced with a screenshot).

## Changes

### Server
- `PromptHistoryKind::Reply` variant + `TerminalState::record_reply`.
- `pane.report_reply` API method + `pane.report-reply` CLI subcommand. Mirrors `report_recap` shape; same sanitize, same kind-of-fire-and-forget contract. Reply does NOT touch `last_prompt`.
- `HookReplyReported` event flows through the same path `HookRecapReported` does.

### Rendering (`build_prompt_history_lines`)
- New `reply_style = Style::default().fg(palette.blue).bg(palette.surface_dim)`.
- Match arm extended for `Reply`.
- Heuristic re-styling, defense-in-depth:
  - Recap body lines starting with `※` get the `※ recap:` prefix bolded (chrome) while the summary keeps the recap accent (content).
  - Body lines that look like leaked XML chrome (`<task-notification>`, `<task-id>…</task-id>`, etc.) get dimmed to `palette.overlay0`. The shim filters these at the source — the renderer dims pre-fix history captured before this lands.

### Claude integration shim v7 (`assets/claude/herdr-agent-state.sh`)
- `action=prompt`: now filters `<task-notification>` / `<system-reminder>` / `<command-*>` / `<bash-*>` prefixes early. Harness internals stop appearing as fake user prompts.
- `action=stop` (new): scrapes `transcript_path` for the last assistant message, POSTs as `pane.report_reply` (capped 4KB), then scans the same text for a `※ recap:` line and POSTs it as `pane.report_recap`.
- Self-healing nudge: if no sentinel line was found AND we had any assistant text, the shim writes `{"decision":"block","reason":"End your turn with a single sentinel line: \`※ recap: <state>. Next: <step>.\` Then stop."}` to stdout. Claude Code interprets `decision: block` as "one more turn with this reason as context" — the agent gets a chance to emit the recap, then stops cleanly. Never user-facing. Subagent/empty-transcript turns skip the nudge so we don't loop.
- `CLAUDE_INTEGRATION_VERSION` bumped 6 → 7; existing installs auto-update on the next `herdr integration install claude`.
- Stop hook wired in `install_claude()`.

## Testing

- Full suite: **2209 passed / 0 failed / 1 ignored** (`cargo test --bin herdr`).
- `cargo clippy --bin herdr -- -D warnings`: clean.
- New tests:
  - `report_reply_round_trips_through_update_terminal_state` — prompt → reply → recap all land as distinct kinds in history.
  - `report_reply_rejects_unknown_pane_and_invalid_agent` — same validation as recap.
  - `xml_chrome_detects_system_reminder_tags_but_not_prose` — heuristic positives and negatives.
  - `recap_sentinel_prefix_isolates_marker_and_label` — bold-prefix span boundary.
  - `claude_hook_asset_has_v7_shape` — structural smoke test for the shim asset (catches refactor regressions without spawning python3).
  - Existing `install_claude_writes_hook_and_updates_settings` updated to expect the Stop hook + ` stop` action; version-pinned tests updated to v7.

Note: CI's `check (ubuntu-latest)` / `check (macos-latest)` may still flake on `cross_area` / `cli_wrapper` (per the existing known-flake memory entry) — those have failed identically on clean master and are unrelated.

## Coupled work

A follow-up will land the matching CLAUDE.md global rule for the `※ recap:` discipline so the self-healing nudge is the safety net, not the primary mechanism.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
