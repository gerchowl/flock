#!/usr/bin/env python3
"""Live acceptance runbook for #1180 (ADR-0034 §3, §4): a real Claude Code
session behind flk's question hook.

Run by hand, never by `just check`. Each case costs one short Claude turn
(`--claude-model`, default haiku).

  delegate      An isolated flk server starts a root pane, and the root starts
                a Claude delegate from its own pane, so the delegate's record
                says `route` (#1179). The delegate is asked to use
                AskUserQuestion: no dialog is drawn, and the model quotes the
                reason the server gave. It is then asked to use Read: the
                question entry starts no process.
  root-stopped  A root's Claude (no FLOCK_QUESTIONS) with the server stopped:
                the dialog draws as today, and the hook took under 500 ms.
  delegate-stopped
                A delegate's Claude (FLOCK_QUESTIONS=route) with the server
                stopped: no dialog, and the model quotes the fallback reason.

The hook entry is exactly the one `flk integration install claude` writes:
the probe installs into a sandbox CLAUDE_CONFIG_DIR, takes the `PreToolUse`
entry from that settings.json, and hands it to Claude with `--settings`. The
only change is a logging prefix on its command, which records when the stub
starts and ends, so the probe can count spawns and time them. Claude itself
runs on the operator's active profile, because it cannot be logged in
otherwise, and the profile's own settings are never written.

Safety. Every inherited FLOCK_* variable is removed. The isolated flk uses a
short sandbox XDG tree and its own socket; the stopped-server cases point at
a socket nobody listens on. Claude runs with `--strict-mcp-config` and a
`--tools` list limited to AskUserQuestion and Read. `--cwd` must be a folder
Claude already trusts (a flk-created worktree is, #605), because an untrusted
folder holds every settings hook back behind the trust dialog.

    python3 -I tests/probes/question_hook_claude.py --flk target/debug/flk \\
        --cwd <trusted dir> [case ...]
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import shutil
import subprocess
import sys
import time

TMUX_SERVER = "flk-q2-probe"
UNROUTED = "state the question and options in your final message and end your turn"
ROUTE_FALLBACK = "flk could not reach your architect"
ASK = (
    'Use the AskUserQuestion tool exactly once to ask me "Which color?" with options '
    "red and blue. If it fails or is blocked, quote the error text verbatim. Then stop."
)
READ = "Use the Read tool once to read the first line of README.md, then quote it and stop."
TOOLS = ["--tools", "AskUserQuestion,Read"]


def dialog_open(screen: str) -> bool:
    return "Enter to select" in screen


def turn_over(screen: str) -> bool:
    if dialog_open(screen):
        return True
    return any(line.lstrip().startswith("✻ ") and " for " in line for line in screen.splitlines())


def wait_for(read, predicate, timeout_s: float, seen: dict | None = None) -> str:
    """Poll until `predicate(screen)`. Each key of `seen` is a text, set True
    once it was on screen at any poll: the pane is short, and a later redraw
    can scroll a tool result away before the turn ends."""
    deadline = time.monotonic() + timeout_s
    screen = read()
    while True:
        flat = " ".join(screen.split())
        for text in seen or {}:
            seen[text] = seen[text] or text in flat
        if predicate(screen) or time.monotonic() >= deadline:
            return screen
        time.sleep(0.5)
        screen = read()


class Sandbox:
    """Paths and an environment that can only reach the sandbox flk."""

    def __init__(self, flk: str, root: str):
        self.root = root
        self.flk = os.path.abspath(flk)
        self.socket = os.path.join(root, "f.sock")
        self.log = os.path.join(root, "spawns.log")
        self.settings = os.path.join(root, "q.json")
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("FLOCK_")}
        self.env.pop("TMUX", None)
        self.env.update(
            XDG_CONFIG_HOME=os.path.join(root, "x"),
            XDG_STATE_HOME=os.path.join(root, "s"),
            XDG_DATA_HOME=os.path.join(root, "d"),
            FLOCK_SOCKET_PATH=self.socket,
        )

    def run(self, *argv: str, env: dict | None = None) -> str:
        out = subprocess.run(
            [self.flk, *argv], capture_output=True, text=True, env=env or self.env, timeout=60
        )
        if out.returncode != 0:
            sys.exit(f"flk {' '.join(argv)} failed ({out.returncode}): {out.stderr}")
        return out.stdout

    def prepare(self) -> dict:
        """Install into a sandbox Claude dir and write the --settings file."""
        app = "flock-dev" if "/debug/" in self.flk else "flock"
        os.makedirs(os.path.join(self.root, "x", app), exist_ok=True)
        with open(os.path.join(self.root, "x", app, "config.toml"), "w") as fh:
            fh.write("onboarding = false\n")
        claude_dir = os.path.join(self.root, "cc")
        os.makedirs(claude_dir, exist_ok=True)
        print(self.run("integration", "install", "claude", env={**self.env, "CLAUDE_CONFIG_DIR": claude_dir}))
        with open(os.path.join(claude_dir, "settings.json")) as fh:
            entry = json.load(fh)["hooks"].get("PreToolUse", [])
        if not entry:
            # A build without the question entry: Claude runs with no
            # PreToolUse hook, which is the behaviour before #1180.
            with open(self.settings, "w") as fh:
                json.dump({"hooks": {}}, fh)
            return {}
        assert len(entry) == 1, entry
        hook = entry[0]["hooks"][0]
        stamp = "perl -MTime::HiRes=time -e 'printf \"%.3f\", time'"
        script = (
            f'echo "start $({stamp})" >> {self.log}; out=$({hook["command"]}); '
            f'echo "end $({stamp})" >> {self.log}; echo "out $out" >> {self.log}; '
            'printf "%s\\n" "$out"'
        )
        hook["command"] = "sh -c " + shlex.quote(script)
        with open(self.settings, "w") as fh:
            json.dump({"hooks": {"PreToolUse": entry}}, fh)
        return entry[0]

    def spawns(self) -> list[tuple[float, float, str]]:
        """(start, end, stdout) of each hook run, in order."""
        if not os.path.exists(self.log):
            return []
        runs, run = [], {}
        for line in open(self.log):
            kind, _, value = line.rstrip("\n").partition(" ")
            run[kind] = value
            if kind == "out":
                runs.append((float(run["start"]), float(run["end"]), value))
                run = {}
        return runs


def claude_argv(a, sb: Sandbox) -> list[str]:
    return ["claude", "--model", a.claude_model, "--strict-mcp-config", *TOOLS, "--settings", sb.settings]


def agent_record(sb: Sandbox, name: str) -> dict:
    """`agent get` as a dict, or {} while the agent does not exist yet."""
    out = subprocess.run(
        [sb.flk, "--json", "agent", "get", name], capture_output=True, text=True, env=sb.env, timeout=30
    )
    try:
        value = json.loads(out.stdout)
    except json.JSONDecodeError:
        return {}
    for key in ("result", "data", "agent"):
        if isinstance(value, dict) and isinstance(value.get(key), dict):
            value = value[key]
    return value if isinstance(value, dict) else {}


def case_delegate(a, sb: Sandbox) -> tuple[bool, str]:
    server = subprocess.Popen(
        [sb.flk, "server"], env=sb.env, stdin=subprocess.DEVNULL,
        stdout=open(os.path.join(sb.root, "server.out"), "w"), stderr=subprocess.STDOUT,
    )
    try:
        deadline = time.monotonic() + 15
        while not os.path.exists(sb.socket) and time.monotonic() < deadline:
            time.sleep(0.2)
        claude = " ".join(shlex.quote(x) for x in claude_argv(a, sb))
        # The root's only job is to start the delegate from its own pane.
        root_cmd = (
            f"{shlex.quote(sb.flk)} agent start q2-delegate --cwd {shlex.quote(a.cwd)} "
            f"--no-focus -- {claude}; exec sleep 3600"
        )
        sb.run("agent", "start", "q2-root", "--cwd", a.cwd, "--no-focus", "--", "/bin/sh", "-c", root_cmd)
        got = {}
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline and not got.get("pane_id"):
            time.sleep(0.5)
            got = agent_record(sb, "q2-delegate")
        pane = got.get("pane_id")
        if not pane:
            return False, "the delegate never started"
        policy = got.get("questions")
        read = lambda: sb.run(
            "pane", "read", pane, "--source", "recent-unwrapped", "--lines", "400", "--format", "text"
        )
        wait_for(read, lambda s: "❯" in s, 40)
        time.sleep(6)
        before = len(sb.spawns())
        sb.run("pane", "send-text", "--raw", pane, ASK)
        time.sleep(0.5)
        sb.run("pane", "send-keys", pane, "Enter")
        seen = {UNROUTED: False}
        wait_for(read, turn_over, 120, seen)
        time.sleep(2)
        asked = wait_for(read, lambda _: True, 0, seen)
        runs = sb.spawns()[before:]
        after_ask = before + len(runs)
        hook_out = runs[0][2] if runs else ""
        if dialog_open(asked):
            # Without the hook the dialog holds the pane: the case has failed.
            return False, f"policy={policy} dialog=True spawns: ask={after_ask - before}\n{asked}"
        sb.run("pane", "send-text", "--raw", pane, READ)
        time.sleep(0.5)
        sb.run("pane", "send-keys", pane, "Enter")
        time.sleep(3)
        wait_for(read, turn_over, 120)
        after_read = len(sb.spawns())
        denied = '"permissionDecision":"deny"' in hook_out and UNROUTED in hook_out
        ok = (
            policy == "route"
            and not dialog_open(asked)
            and denied
            and seen[UNROUTED]
            and after_ask == before + 1
            and after_read == after_ask
        )
        report = (
            f"policy={policy} dialog={dialog_open(asked)} hook_denied={denied} "
            f"reason_on_screen={seen[UNROUTED]} spawns: ask={after_ask - before} "
            f"read={after_read - after_ask}\nhook stdout: {hook_out}\n{asked}"
        )
        return ok, report
    finally:
        server.terminate()
        server.wait(timeout=10)


def tmux(*args: str) -> str:
    return subprocess.run(["tmux", "-L", TMUX_SERVER, *args], capture_output=True, text=True).stdout


def stopped_turn(a, sb: Sandbox, hint: str | None) -> tuple[str, list[tuple[float, float]]]:
    """One Claude turn in a private tmux, inside a pane whose flk is gone."""
    env = {"FLOCK_ENV": "1", "FLOCK_PANE_ID": "p_probe", "FLOCK_SOCKET_PATH": os.path.join(sb.root, "gone.sock"), "FLOCK_BIN": sb.flk}
    if hint:
        env["FLOCK_QUESTIONS"] = hint
    prefix = "env " + " ".join(f"-u {k}" for k in os.environ if k.startswith("FLOCK_") or k == "TMUX")
    prefix += " " + " ".join(f"{k}={shlex.quote(v)}" for k, v in env.items())
    command = prefix + " " + " ".join(shlex.quote(x) for x in claude_argv(a, sb))
    tmux("kill-server")
    tmux("new-session", "-d", "-s", "p", "-x", "160", "-y", "50", "-c", a.cwd, command)
    read = lambda: tmux("capture-pane", "-p", "-t", "p", "-S", "-200")
    wait_for(read, lambda s: "❯" in s, 30)
    time.sleep(6)
    before = len(sb.spawns())
    tmux("send-keys", "-t", "p", "-l", ASK)
    time.sleep(0.5)
    tmux("send-keys", "-t", "p", "Enter")
    wait_for(read, turn_over, 90)
    time.sleep(2)
    screen = read()
    tmux("kill-server")
    return screen, sb.spawns()[before:]



def case_root_stopped(a, sb):
    screen, spawns = stopped_turn(a, sb, None)
    took = [round((end - start) * 1000) for start, end, _ in spawns]
    ok = dialog_open(screen) and len(took) == 1 and took[0] < 500
    return ok, f"dialog={dialog_open(screen)} hook_ms={took}\n{screen}"


def case_delegate_stopped(a, sb):
    screen, spawns = stopped_turn(a, sb, "route")
    flat = " ".join(screen.split())
    ok = not dialog_open(screen) and ROUTE_FALLBACK in flat and len(spawns) == 1
    return ok, f"dialog={dialog_open(screen)} fallback_quoted={ROUTE_FALLBACK in flat} spawns={len(spawns)}\n{screen}"


CASES = {"delegate": case_delegate, "root-stopped": case_root_stopped, "delegate-stopped": case_delegate_stopped}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--flk", required=True, help="the flk build under test")
    parser.add_argument("--cwd", required=True, help="a folder Claude already trusts")
    parser.add_argument("--claude-model", default="haiku")
    parser.add_argument("--root", default=f"/tmp/q2p{os.getpid()}", help="short sandbox root")
    parser.add_argument("cases", nargs="*", default=list(CASES))
    a = parser.parse_args()
    a.cwd = os.path.abspath(a.cwd)
    os.makedirs(a.root, exist_ok=True)
    sb = Sandbox(a.flk, a.root)
    entry = sb.prepare()
    print("installed entry:", json.dumps(entry))
    failed = 0
    try:
        for name in a.cases:
            ok, report = CASES[name](a, sb)
            failed += not ok
            print(f"{'PASS' if ok else 'FAIL'} {name}: {report.splitlines()[0]}")
            if not ok:
                print(report)
    finally:
        shutil.rmtree(a.root, ignore_errors=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
