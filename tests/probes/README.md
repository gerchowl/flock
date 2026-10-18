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
python3 tests/probes/run.py --bin target/debug/flk \
  --runbook tests/probes/<name>.toml --artifacts /tmp/probe-out --driver "$PROBE"
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
- **Use the wrapper for either build.** It copies the selected binary inside a
  private `/tmp/flock-autodetect-test-<owner-pid>-<random>` sandbox, seeds config,
  removes inherited `FLOCK_*` variables, and records its server before invoking
  the driver. Normal completion stops that owned child and removes the copy.
  Optional `--artifacts` keeps driver artifacts separately; binary copies and
  state always stay disposable. Never leave an unfixed binary copy outside this
  lifecycle.

## Crash recovery and identity

Before starting a server, the auto-detect test suite sweeps the same sandbox
namespace. Live owner PIDs are kept. For dead owners, durable `.server-*.json`
records must prove the current server's executable, `server` argument, sandbox
HOME and state directory, and start time when recorded. The argv must be the
fixture's exact server invocation, rather than a client command containing the
word `server`. Identity is rechecked
before a signal; native exit notifications impose a finite cleanup ceiling.
Unproven identities, incomplete records, unavailable inspection and timeouts are
logged and retain the sandbox. There are no pattern-based signals in this sweep.
Concurrent sweepers use a nonblocking per-sandbox lock. Copied binaries disappear
only with their recovered sandbox.

Only newly recorded sandboxes are recoverable automatically: historical
sandboxes without records are retained, as are dead-owner PIDs reused by a live
process. The copied probe server is recorded by the wrapper; direct and detached
auto-detect servers are recorded by shared test support. Rust tests in
`tests/sandbox_sweep.rs` exercise the actual wrapper and recovery with real
isolated servers, including a wrong-PID refusal, so the proof runs in CI.

## Socket-driven measurement scripts

`agent_wake.py` is not a TUI runbook: it drives an isolated flk over its socket
to time wakes (idle, mid-turn, with a harness background task), and it reads an
`event-log.jsonl` or a System One endpoint for the volume and judge-latency
figures in ADR-0028 and ADR-0030. It refuses to run the pane probes without
`--sandbox`. `python3 -I tests/probes/agent_wake.py --help` lists the setup.

`fork_cache.py` is the runbook behind ADR-0033's fork-cost figures: its
docstring is the setup (an isolated flk, a warm throwaway Claude session, a
fork through `agent.fork` or in place), and `usage <session id>` prints the
cache read and creation tokens of every turn from the Claude transcript.

`harness_question_hooks.py` re-measures the harness facts ADR-0034 rests on.
For Claude these are: a `PreToolUse` hook answering `AskUserQuestion` and
approving `ExitPlanMode`, `--disallowedTools`, failures falling open by
default and `onFailure: "block"` closing them, and mid-turn
`additionalContext`. For Codex they are: a deny, `request_user_input` reaching
`PreToolUse`, mid-turn `additionalContext`, and a crashing, garbled, missing
or slow hook failing open. It talks to no flk at all: it unsets every
`FLOCK_*` variable, runs Claude in a private `tmux -L` server with no MCP
servers and a minimal `--tools` list, and runs Codex against a throwaway
`CODEX_HOME` (a copy of `auth.json`, deleted on exit) under
`workspace-write`. `--cwd` must be a folder Claude already trusts. Re-run it
on every harness version the fleet pins.

`question_hook_claude.py` is #1180's live acceptance (ADR-0034 §3, §4): a real
Claude Code session behind flk's question hook. In an isolated flk, a Claude
delegate started from a root pane calls `AskUserQuestion`: no dialog is drawn,
the hook returns `deny` with the server's reason, and a following `Read` starts
no hook process. With the server stopped, a root's dialog draws and its hook
takes under 500 ms, and a `route`-hinted delegate is refused with the fallback
sentence. The hook entry is the one `flk integration install claude` writes
into a sandbox `CLAUDE_CONFIG_DIR`, passed to Claude with `--settings`, so the
operator's profile is never written. Run it once per build, `--flk` naming a
build with the change and then one without, for the A/B.
