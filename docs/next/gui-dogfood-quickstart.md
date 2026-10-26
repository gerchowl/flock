# GUI dogfood quickstart

This is a fake-harness smoke of the packaged browser MVP, verified on macOS
at dev `f6b2be619`. It uses the public CLI, HTTP code exchange, fleet-lite,
composer, transcript and permission inbox. It never uses a real Claude login.
The disabled New agent, Terminal, Diff and Fork controls are outside this smoke.

## Build and sandbox

Start in a fresh worktree of `origin/dev`, with Python 3 and the Nix devShell
available. Run `nix build .#flock-web`. The fallback build is
`CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=/Volumes/build-rd/cargo/ibis-gui-smoke cargo build --features web`;
if using it, substitute that target's `debug/flk` below.

Run this setup from the worktree root in Bash. The Python block installs an
owned fake `claude` ahead of PATH and a fake Tailscale status command. The fake
OAuth value is fixture data, not a usable credential. Nothing inherits the
live server's `FLOCK_*` variables or the operator's HOME/XDG directories.

```bash
export SMOKE_REPO="$PWD"
export SMOKE_ROOT="$(mktemp -d /tmp/fg.XXXXXX)"
export SMOKE_BIN="$PWD/result/bin/flk"
python3 - <<'PYTHON'
import json, os, shlex, socket, sys
from pathlib import Path
repo = Path(os.environ["SMOKE_REPO"]).resolve()
root = Path(os.environ["SMOKE_ROOT"]).resolve()
binary = Path(os.environ["SMOKE_BIN"]).resolve(strict=True)
(root / "bin").mkdir()
(root / "project").mkdir()
for name in ("ready", "turn-ready", "resume-ready"):
    os.mkfifo(root / name, 0o600)
fixture = (repo / "tests/fixtures/structured-public/claude.py").read_text()
needle = "            value = json.loads(line)\n"
assert fixture.count(needle) == 1
fixture = fixture.replace(needle,
    '            with (root / "harness-input.jsonl").open("a") as log:\n'
    '                log.write(line)\n' + needle)
(root / "bin/claude").write_text(f"#!{sys.executable}\n" + fixture)
(root / "bin/claude").chmod(0o700)
(root / "bin/tailscale").write_text("#!/bin/sh\nprintf '{}\\n'\n")
(root / "bin/tailscale").chmod(0o700)
(root / "init.json").write_bytes(
    (repo / "tests/fixtures/structured-runtime/restricted-init.json").read_bytes())
(root / "token").write_text("fixture-token-not-a-credential")
(root / "token").chmod(0o600)
with socket.socket() as port:
    port.bind(("127.0.0.1", 0))
    number = port.getsockname()[1]
config = f'''onboarding = false
[client]
bind = "127.0.0.1:{number}"
[structured]
run_as = "operator"
profile = "gui-smoke"
token_file = {json.dumps(str(root / "token"))}
[security]
agent_account = "none"
'''
for app in ("flock", "flock-dev"):
    directory = root / "x" / app
    directory.mkdir(parents=True)
    (directory / "config.toml").write_text(config)
script = "SMOKE_ROOT=" + shlex.quote(str(root)) + "\n"
script += "SMOKE_BIN=" + shlex.quote(str(binary)) + "\n"
script += '''smoke() {
  env -i PATH="$SMOKE_ROOT/bin:$PATH" HOME="$SMOKE_ROOT" \
    XDG_CONFIG_HOME="$SMOKE_ROOT/x" XDG_STATE_HOME="$SMOKE_ROOT/s" \
    XDG_DATA_HOME="$SMOKE_ROOT/d" XDG_CACHE_HOME="$SMOKE_ROOT/cache" \
    TMPDIR="$SMOKE_ROOT" SHELL=/bin/sh LANG=C LC_ALL=C \
    USER=fixture-user LOGNAME=fixture-user TERM=xterm \
    FLOCK_SOCKET_PATH="$SMOKE_ROOT/a" FLOCK_CLIENT_SOCKET_PATH="$SMOKE_ROOT/c" \
    "$SMOKE_BIN" "$@"
}
'''
(root / "env.sh").write_text(script)
print("In both terminals: source " + shlex.quote(str(root / "env.sh")))
PYTHON
source "$SMOKE_ROOT/env.sh"
exec 3<>"$SMOKE_ROOT/ready" 4<>"$SMOKE_ROOT/turn-ready" 5<>"$SMOKE_ROOT/resume-ready"
cd "$SMOKE_ROOT/project"
smoke server
exec 3>&- 4>&- 5>&-
```

Keep the first terminal running. Wait for its `client socket:` readiness line.
The FIFO descriptors let the fixture report readiness without blocking its
stdout stream. In a second terminal, run the exact `source` command printed
by setup, then:

```bash
cd "$SMOKE_ROOT/project"
smoke --json agent start smoke --runtime structured \
  --model dogfood-wire-child --cwd "$SMOKE_ROOT/project" -- claude
unset BROWSER
smoke client open --print
```

Structured admission accepts the closed `-- claude` form, not arbitrary
`python3 fixture.py` argv. The sandbox wrapper executes the required fake.
The model name selects its permission-producing branch; it is not a real model.

## Browser checks

Open the printed URL in a fresh browser context. The smoke used headless
Chromium through Playwright. The one-use `#code=` is exchanged automatically
and removed from the URL. Do not publish it or the sandbox's browser token.

1. Expect **Live**, one `smoke` fleet row, and no permission cards. Select
   `smoke`; its transcript contains **System · hook_started**.
2. In **Message smoke**, send exactly
   `UNTRUSTED TASK dogfood-task — fake fixture bootstrap`. This fake's initial
   input assertion expects those words. This is fixture bootstrapping, not a
   claim that user-written words prove prompt framing. Expect the message's
   **accepted by the server** echo, `dogfood-complete` and a success result.
3. Send `GUI smoke allow`. Expect its accepted echo, `wire-send-observed`,
   **needs you**, and one Bash permission card for command `true`.
4. Click **Allow once**. Expect `wire-decision-observed`, `wire-complete`,
   **Permission request closed**, and **No open permission prompts**.
5. Send `GUI smoke deny`. Expect the next Bash card; click **Deny** and expect
   the same close/result signals and empty inbox.
6. Save screenshots under `.local/`. With Playwright MCP, use a path within
   its configured allowed root, then copy into the task worktree if needed.

The fake emits receipt markers rather than repeating the message body in its
assistant response; the UI shows the sent body under **Your messages**.
The only fixture instrumentation above appends input lines to an owned log;
it leaves the fixture's assertions, permission protocol and outputs intact.
Verify both actual harness decisions in the second terminal:

```bash
python3 - "$SMOKE_ROOT" <<'PYTHON'
import json, sys
from pathlib import Path
rows = [json.loads(line) for line in
        (Path(sys.argv[1]) / "harness-input.jsonl").read_text().splitlines()]
responses = [row["response"] for row in rows if row["type"] == "control_response"]
assert [r["request_id"] for r in responses] == ["typed-request", "typed-request-2"]
assert [r["response"]["behavior"] for r in responses] == ["allow", "deny"]
print("Allow and Deny both reached the fake harness")
PYTHON
```

The initial smoke observed duplicate-looking permission transcript entries
(one card and one harness decision per prompt), and a `/favicon.ico` 404.
Tracked as #2559 and #2560; neither prevented the exchange or either decision.
WebSocket errors after intentionally stopping the server are teardown effects.

## Teardown

Close the browser context, then in the second terminal run:

```bash
smoke agent stop smoke
smoke server stop
```

Wait for the first terminal's foreground server to return successfully before
removing this sandbox. Never stop another server or kill unrelated processes.
Keep only the screenshots and sanitized evidence you need, then:

```bash
rm -rf -- "$SMOKE_ROOT"
```
