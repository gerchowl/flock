# Codex dim hint at 26 columns

Captured through `flk pane read --source recent --format text` and `--format ansi` from an isolated real PTY with an event-driven fake Codex during #1139 round-two triage, now #1581. The owned client was 28×40, and `stty size` reported 31×26. No model or live operator profile was used.

The prompt and shortcuts footer are the invariant controls. The capacity banner and frame marker above them are incidental. The editor contains only the known Codex hint prefix in SGR 2 (dim), with no normal-weight draft. Other tests vary the width, styling and actual draft independently. A missing footer or dialog must still fail closed.
