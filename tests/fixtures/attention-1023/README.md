# Attention screens (#1023)

Live captures behind the `attention` axis of ADR-0028 §1: the dialogs a human
answers (`needs_human { reason }`) and the quota screens that block an agent
(`blocked { provider_limit }`), for Claude Code 2.1.296 and Codex 0.160.1.

Each was captured with `flk pane read --source recent --format text` from a
throwaway session in an isolated flk server: a copied 1.2.0 binary, a sandbox
`HOME`, `XDG_CONFIG_HOME` and `XDG_STATE_HOME`, a sandbox `CLAUDE_CONFIG_DIR`
and `CODEX_HOME`, and a 52-to-54-column pane. No owner profile, credential or
live server was involved.

The model behind the harness was a local fake endpoint (`ANTHROPIC_BASE_URL`,
or a Codex `model_providers` entry with `wire_api = "responses"`) that answered
a keyword in the prompt with a canned reply: a shell tool call, a question tool
call, `ExitPlanMode`, or a quota refusal. So the screens are the harnesses' own
rendering of those replies. The quota refusals are HTTP 429s shaped like the
providers' own:

- Claude: `anthropic-ratelimit-unified-status: rejected` with a
  `…-unified-reset` time and a `five_hour` or `seven_day` representative claim,
  against a session holding fake subscription OAuth credentials in the sandbox
  config dir (an API-key session renders a 429 as a plain
  `API Error: Request rejected (429)` instead).
- Codex: `{"error": {"type": "usage_limit_reached", "plan_type": "plus",
  "resets_at": …}}`.

The weekly line matches the field report in #821 ("You've hit your weekly
limit · resets Oct 11 at 7am"). Sandbox paths were replaced by
`/fixture/attention-1023/…`, nothing else.

| File | Screen | `attention` |
| --- | --- | --- |
| `claude-trust.txt` | first-start folder trust; 2.1.296 draws it under a `──` rule, without the rounded frame of 2.1.285 | `needs_human{trust}` |
| `claude-login.txt` | login method picker, empty config dir | `needs_human{login}` |
| `claude-api-key.txt` | "Detected a custom API key", use it? | `needs_human{login}` |
| `claude-permission.txt` | Bash tool permission prompt | `needs_human{permission}` |
| `claude-question.txt` | `AskUserQuestion` form | `needs_human{question}` |
| `claude-plan-exit.txt` | "Exit plan mode?" | `needs_human{dialog}` |
| `claude-limit-footer.txt` | session limit, auto-continue armed: footer under the prompt box | `blocked{provider_limit}` |
| `claude-limit-cancelled.txt` | session limit after Esc cancelled auto-continue: only the last turn says so | `blocked{provider_limit}` |
| `claude-limit-options.txt` | `/rate-limit-options` menu | `blocked{provider_limit}` |
| `claude-weekly-options.txt` | weekly limit, the menu Claude opens by itself | `blocked{provider_limit}` |
| `claude-weekly-stopped.txt` | weekly limit after "Stop and wait for limit to reset" | `blocked{provider_limit}` |
| `codex-login.txt` | sign-in picker, empty `CODEX_HOME` | `needs_human{login}` |
| `codex-permission.txt` | escalated `exec_command` approval | `needs_human{permission}` |
| `codex-question.txt` | `request_user_input` in Plan mode | `needs_human{question}` |
| `codex-limit.txt` | `■ You've hit your usage limit` as the last turn | `blocked{provider_limit}` |

Codex's folder-trust dialog is `../codex/folder-trust.txt` (#828); the same
dialog captured here was identical apart from the path.

Neither harness shows a duration until the reset, only a wall-clock time
("resets 5:41am (Europe/Zurich)", "try again at 5:46 AM"), so these carry no
`retry_after_ms`.
