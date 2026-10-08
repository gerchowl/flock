# Codex 0.160.1 guarded submission captures (#721)

Captured from real Codex 0.160.1 panes in an isolated Flock instance at
`037aef61`, with a sandbox HOME and separate state and sockets. These fixtures
are separate from the startup-banner fixtures for #723.

Each `.txt` and `.ansi` pair was captured with:

```sh
flk pane read <pane> --source recent --format text
flk pane read <pane> --source recent --format ansi
```

The sandbox directory was replaced with `/fixture/codex-721`, and space-only
rows were normalized to empty lines. Other whitespace, line wrapping,
displayed directory truncation, and ANSI styling are retained.
The two formats are consecutive snapshots, so a working timer may differ.

- `inline-*`: `codex --no-daemon --no-alt-screen -s read-only -a never`.
- `alt-*`: `codex --no-daemon -s read-only -a never` (default alternate screen).
- `idle`: empty composer before typing.
- `owned`: after raw `agent send`, before Enter. The inline draft wraps.
- `working`: immediately after `pane send-keys <pane> Enter`.
- `alt-model`: `/model` picker, with no live message composer.

The live composer requires a prompt row AND a footer below it. Footer
alternatives are the existing shortcut/context controls OR the model/effort
and directory status line separated by ` · `. In alternate-screen mode the
status line can precede a separate shortcut row. It belongs to the footer,
not to the draft. Transcript and startup-banner text do not authorize a
composer. Working chrome and dialog controls continue to veto submission.
