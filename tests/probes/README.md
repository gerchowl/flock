# probes — end-to-end runbooks driven through a real TUI

Cargo tests can reach the socket API and the headless server, but not the
thing a user actually looks at: a real flk process, rendering a real screen,
reacting to real keystrokes. These runbooks cover that last hop.

They are **run by hand**, not by `just check` — they need `tmux` and a
harness that can drive a pty (this fork's agents use the `tui-probe` skill;
any tmux driver works). Treat them as reproductions you can re-run against
two builds, not as CI.

## Running one

```sh
PROBE=~/.claude/skills/tui-probe/tui-probe
$PROBE run tests/probes/<name>.toml --artifacts /tmp/out \
  --var bin=target/debug/flk --var home=<sandbox> --var xdg=/tmp/<short>
```

Every runbook takes `bin`, so the same script runs against two builds. That
A/B — fixed binary passes, unfixed binary fails on exactly the assertions
that describe the bug — is the point of the format.

## Sandboxing rules (learned the hard way, see #328)

- **`$HOME` is the fixture root.** Agent transcripts are resolved under
  `$HOME/.claude/projects/<slug>/<session-id>.jsonl`, so pointing `HOME` at a
  sandbox is how you control what the app reads. Never point a probe at your
  real `~`.
- **Sockets need a SHORT path.** `sun_path` caps around 104 bytes on macOS.
  A deep scratch directory silently fails to bind and the client dies with
  "server did not become ready". Keep `XDG_CONFIG_HOME` under something like
  `/tmp/fp1` even when `HOME` is long.
- **Unset the inherited `FLOCK_*` vars.** An agent pane already has
  `FLOCK_ENV`, so a nested flk refuses to start ("nested flock is disabled").
- **Debug builds use a different app dir** (`flock-dev`, see `app_dir_name`),
  so a debug probe cannot collide with a release install even before
  sandboxing.
- **Seed the config.** `onboarding = false` skips the welcome dialogs, and
  binding what you need to a plain key (`toggle_prompt_expand = ["f8"]`)
  avoids driving prefix-mode chords.
- **Kill the server the probe leaves behind.** Stopping the probe stops the
  client; flk's server daemonises and survives. Match on the sandbox binary
  path so you can never signal the user's real server.
