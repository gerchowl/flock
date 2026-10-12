# Folded paste chips in the composer (#995)

Captured from real panes in an isolated Flock instance (sandbox XDG dirs and
socket) after `flk agent send --no-submit <agent> <text>` with a 1099-byte,
one-line message, before any Enter:

```sh
flk pane read <pane> --source recent --format text
flk pane read <pane> --source recent --format ansi
```

- `claude-*`: Claude Code v2.1.296. The editor shows `[Pasted text #1]`
  instead of the text (a multi-line paste adds ` +N lines`).
- `codex-*`: `codex --no-alt-screen -s read-only -a never`, Codex 0.160.1.
  The editor shows `[Pasted Content 1099 chars]`.

The sandbox directory was replaced with `/fixture/paste-chip`, and the Claude
status line's host and profile with `host` and `profile:fixture`. Nothing else
was edited.

The `.txt` captures drive the composer and guarded-submit tests. The `.ansi`
captures are checked to strip to the same chip row
(`ansi_captures_strip_to_the_same_chip_row`).
