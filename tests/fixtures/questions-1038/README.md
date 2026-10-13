# Question forms (#1038)

Live captures behind the structured `question` payload of ADR-0028 §8a: the
Claude Code `AskUserQuestion` form, read by `claude_code::question`.

Captured on 2026-10-13 from Claude Code 2.1.296 (model haiku) in a private tmux server
(`tmux -L`, an 80x45 pane, every inherited `FLOCK_*` variable unset, so no flk
server was involved), with `--strict-mcp-config` and `--tools AskUserQuestion`.
The prompt asked for exactly one `AskUserQuestion` call with fixed questions.
Each file is `tmux capture-pane -p -S -200` of the live pane, taken after the
dialog drew or after one key. The only edit is the repo path in the welcome
banner, replaced with `/fixture/questions-1038/repo`.

| File | Screen | Reads as |
| --- | --- | --- |
| `claude-single.txt` | one single-choice question, header `Colour`, cursor on option 1 | `Colour`, options `1 Red` / `2 Blue`, `allow_other` |
| `claude-single-redraw.txt` | the same dialog after `Down`: cursor on option 2 | the same question and `question_id` |
| `claude-single-other-focused.txt` | after a second `Down`: cursor on "Type something.", footer gains `ctrl+g to edit` | the same question and `question_id` |
| `claude-multi.txt` | one `multiSelect` question, header `Checks`, all boxes `[ ]` | `multi_select`, options `Lint` / `Tests` / `Format` |
| `claude-multi-toggled.txt` | the same dialog after `Space`, `Down`, `Space`: two boxes `[✔]` | the same question and `question_id` |
| `claude-two-q1.txt` | a form with two questions, first tab | the first question, no `header` |
| `claude-two-q2.txt` | its second tab: a prompt and a description that wrap, the prompt drawn behind `│` | the second question, joined lines, a new `question_id` |
| `claude-two-review.txt` | its review page ("Review your answers") | `needs_human{question}` with no `question` |

Codex's `request_user_input` form is `../attention-1023/codex-question.txt`
(Codex 0.160.1), and #1023's capture of the same single-choice form, taken
in an isolated flk against a fake endpoint, is
`../attention-1023/claude-question.txt`. Both are read by the same tests.
