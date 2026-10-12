# A long message in a narrow Claude composer (#1122, #1126)

`claude-16-cols.txt` is the bottom of a Claude Code v2.1.296 screen in a pane
16 columns wide, after typing a 225-character, one-line message without
pressing Enter. The composer shows only the last rows of the wrapped text, and
the `❯` sits on the first visible row, not on the start of the message.

The status line's host and profile were replaced with `host · fixtu…`, and a
notification row above the composer was blanked. Nothing else was edited.
