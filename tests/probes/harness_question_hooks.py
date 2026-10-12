#!/usr/bin/env python3
"""Re-measure the harness hook facts ADR-0034 rests on.

ADR-0034's mechanism choices rest on what the installed harnesses let a hook
do. Each fact is a probe here, so a harness upgrade can be re-checked in a few
minutes instead of re-argued.

Claude Code (interactive, in a private tmux server):

  claude-answer       a PreToolUse hook answers AskUserQuestion through
                      `updatedInput.answers`; no dialog, the model gets it
  claude-disallow     `--disallowedTools AskUserQuestion` removes the tool
  claude-timeout      a hook that times out lets the dialog through (fail open)
  claude-onfailure    the same hook with `"onFailure": "block"` blocks instead
  claude-missing      a hook whose command does not exist, with
                      `"onFailure": "block"`, blocks the call
  claude-exitplan     a PreToolUse hook approves ExitPlanMode with `allow` plus
                      `updatedInput`; no plan-approval dialog is drawn
  claude-midturn      PostToolUse `additionalContext` reaches the model between
                      two tool calls of the same turn
  claude-midturn-pre  the same through PreToolUse `additionalContext`

Codex (`codex exec`):

  codex-deny          a PreToolUse deny reaches the model as the tool's error
  codex-question      `request_user_input` is offered in default mode and its
                      call passes through PreToolUse with structured questions
  codex-midturn       PostToolUse `additionalContext` reaches the model between
                      two tool calls of the same turn
  codex-midturn-pre   the same through PreToolUse `additionalContext`
  codex-crash         a PreToolUse hook exiting 1 lets the tool run (fail open)
  codex-garbage       a PreToolUse hook printing non-JSON lets the tool run
  codex-missing       a PreToolUse hook whose command does not exist lets the
                      tool run
  codex-timeout       a PreToolUse hook that times out lets the tool run

Safety. Nothing here talks to flk: every inherited FLOCK_* variable is
removed, so a probe started from inside a flk pane can never reach the live
server. Claude runs in a private tmux server (`tmux -L`), never the operator's,
with `--strict-mcp-config` (no MCP servers) and `--tools` limited to what the
probe needs. Its interactive probes need `--cwd` to be a folder Claude already
trusts (a flk-created worktree is pre-trusted, #605), because an untrusted
folder holds every settings hook back behind the trust dialog. Claude cannot
be isolated from the operator's profile without a fresh login, so it uses the
active profile, and the tool list is what bounds it. Codex runs against a
throwaway CODEX_HOME holding only a copy of `auth.json` and a minimal
`config.toml` (workspace-write sandbox, no MCP servers, no hooks but the
probe's), deleted on exit, from a scratch directory. Each probe costs one short
model turn (Claude on `--claude-model`, default haiku).

    python3 -I tests/probes/harness_question_hooks.py --cwd <trusted dir> [probe ...]
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

TMUX_SERVER = "flk-adr0034-probe"
DENY_REASON = "PROBE-DENY: ask your architect via flk instead"
CODEWORD = "PELICAN-7731"

ANSWER_HOOK = r"""
import json, sys
d = json.load(sys.stdin)
ti = dict(d.get("tool_input", {}))
ti["answers"] = {q["question"]: q["options"][0]["label"] for q in ti.get("questions", []) if q.get("options")}
print(json.dumps({"hookSpecificOutput": {"hookEventName": "PreToolUse",
      "permissionDecision": "allow", "updatedInput": ti}}))
"""

APPROVE_PLAN_HOOK = r"""
import json, sys
d = json.load(sys.stdin)
print(json.dumps({"hookSpecificOutput": {"hookEventName": "PreToolUse",
      "permissionDecision": "allow", "updatedInput": d.get("tool_input", {})}}))
"""


def scrubbed_vars() -> list[str]:
    """Every inherited FLOCK_* variable, plus TMUX so the private server starts clean."""
    return sorted(k for k in os.environ if k.startswith("FLOCK_")) + ["TMUX"]


def clean_env(**extra: str) -> dict[str, str]:
    env = {k: v for k, v in os.environ.items() if k not in scrubbed_vars()}
    env.update(extra)
    return env


def tmux(*args: str) -> str:
    return subprocess.run(
        ["tmux", "-L", TMUX_SERVER, *args], check=False, capture_output=True, text=True
    ).stdout


def wait_for(predicate, timeout_s: float) -> str:
    """Poll the pane until `predicate(screen)` holds or the timeout passes."""
    deadline = time.monotonic() + timeout_s
    screen = ""
    while time.monotonic() < deadline:
        screen = tmux("capture-pane", "-p", "-t", "p")
        if predicate(screen):
            break
        time.sleep(1)
    return screen


def dialog_open(screen: str) -> bool:
    return "Enter to select" in screen or "Would you like to proceed" in screen


def turn_over(screen: str) -> bool:
    """The turn ended: a dialog is drawn, or the spinner line reads `✻ <verb> for <n>s`."""
    if dialog_open(screen):
        return True
    return any(line.lstrip().startswith("✻ ") and " for " in line for line in screen.splitlines())


def claude_turn(cwd: str, model: str, extra: list[str], prompt: str, timeout_s: float = 90) -> str:
    """One interactive Claude turn in a private tmux server; returns the final screen."""
    argv = ["claude", "--model", model, "--strict-mcp-config", *extra]
    command = " ".join(shlex.quote(a) for a in argv)
    env_prefix = "env " + " ".join(f"-u {k}" for k in scrubbed_vars())
    tmux("kill-server")
    tmux("new-session", "-d", "-s", "p", "-x", "160", "-y", "50", "-c", cwd, f"{env_prefix} {command}")
    wait_for(lambda screen: "❯" in screen, 30)
    # The composer is drawn before the session accepts input (plan mode is slower).
    time.sleep(6)
    tmux("send-keys", "-t", "p", "-l", prompt)
    time.sleep(0.5)
    tmux("send-keys", "-t", "p", "Enter")
    wait_for(turn_over, timeout_s)
    time.sleep(2)
    screen = tmux("capture-pane", "-p", "-t", "p", "-S", "-200")
    tmux("kill-server")
    return screen


def reply_lines(screen: str) -> list[str]:
    """The model's own reply lines (Claude marks them with ⏺), never the echoed prompt."""
    return [line.strip().lstrip("⏺").strip() for line in screen.splitlines() if line.strip().startswith("⏺")]


def settings_file(scratch: str, name: str, event: str, matcher: str, hook: dict) -> str:
    path = os.path.join(scratch, f"{name}.json")
    with open(path, "w", encoding="utf-8") as fh:
        json.dump({"hooks": {event: [{"matcher": matcher, "hooks": [hook]}]}}, fh)
    return path


def script(scratch: str, name: str, body: str) -> str:
    path = os.path.join(scratch, name)
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(body)
    os.chmod(path, 0o755)
    return path


ASK = (
    'Use the AskUserQuestion tool exactly once to ask me "Which color?" with options '
    "red and blue. If it fails or is blocked, quote the error text verbatim. Then reply "
    "with only the answer you received."
)
ASK_TOOLS = ["--tools", "AskUserQuestion"]


def probe_claude_answer(a, scratch):
    hook = script(scratch, "answer_hook.py", ANSWER_HOOK)
    settings = settings_file(
        scratch, "answer", "PreToolUse", "AskUserQuestion",
        {"type": "command", "command": f"python3 -I {hook}", "timeout": 30},
    )
    screen = claude_turn(a.cwd, a.claude_model, [*ASK_TOOLS, "--settings", settings], ASK)
    return "Which color? → red" in screen and not dialog_open(screen), screen


def probe_claude_disallow(a, scratch):
    prompt = (
        "Is a tool named AskUserQuestion in your tool list? Answer YES or NO first. If YES, "
        'use it once to ask "Which color?" with options red and blue.'
    )
    screen = claude_turn(a.cwd, a.claude_model, ["--disallowedTools", "AskUserQuestion"], prompt)
    ok = not dialog_open(screen) and any(line.startswith("NO") for line in reply_lines(screen))
    return ok, screen


def probe_claude_timeout(a, scratch):
    settings = settings_file(
        scratch, "timeout", "PreToolUse", "AskUserQuestion",
        {"type": "command", "command": "sleep 15", "timeout": 5},
    )
    screen = claude_turn(a.cwd, a.claude_model, [*ASK_TOOLS, "--settings", settings], ASK)
    return dialog_open(screen), screen


def probe_claude_onfailure(a, scratch):
    settings = settings_file(
        scratch, "onfailure", "PreToolUse", "AskUserQuestion",
        {"type": "command", "command": "sleep 15", "timeout": 5, "onFailure": "block"},
    )
    screen = claude_turn(a.cwd, a.claude_model, [*ASK_TOOLS, "--settings", settings], ASK)
    return "timed out; blocking" in screen and not dialog_open(screen), screen


def probe_claude_missing(a, scratch):
    missing = os.path.join(scratch, "no-such-hook")
    settings = settings_file(
        scratch, "missing", "PreToolUse", "AskUserQuestion",
        {"type": "command", "command": missing, "timeout": 10, "onFailure": "block"},
    )
    screen = claude_turn(a.cwd, a.claude_model, [*ASK_TOOLS, "--settings", settings], ASK)
    return "hook error" in screen and not dialog_open(screen), screen


def probe_claude_exitplan(a, scratch):
    hook = script(scratch, "plan_hook.py", APPROVE_PLAN_HOOK)
    settings = settings_file(
        scratch, "plan", "PreToolUse", "ExitPlanMode",
        {"type": "command", "command": f"python3 -I {hook}", "timeout": 30},
    )
    prompt = (
        "Plan a one-step change: print the word hello. Keep the plan to one line, then call "
        "ExitPlanMode to ask me to approve it. After it returns, say APPROVED if the plan "
        "was approved or REJECTED otherwise, and do nothing else."
    )
    screen = claude_turn(
        a.cwd, a.claude_model,
        ["--permission-mode", "plan", "--tools", "ExitPlanMode,Write,Read", "--settings", settings],
        prompt, 120,
    )
    ok = not dialog_open(screen) and any("APPROVED" in line for line in reply_lines(screen))
    return ok, screen


def claude_midturn(a, scratch, event: str):
    body = json.dumps({"hookSpecificOutput": {"hookEventName": event, "additionalContext": f"The codeword is {CODEWORD}."}})
    hook = script(scratch, f"midturn-{event}.sh", f"#!/bin/sh\ncat >/dev/null\nprintf '%s' {shlex.quote(body)}\n")
    settings = settings_file(
        scratch, f"midturn-{event}", event, "Bash", {"type": "command", "command": hook, "timeout": 10},
    )
    prompt = (
        "Run the Bash command `echo one`. Then, before doing anything else, write the line "
        "CODEWORD=<the codeword you were given after that command, or none>. Then run "
        "`echo two`, and stop."
    )
    screen = claude_turn(
        a.cwd, a.claude_model, ["--tools", "Bash", "--allowedTools", "Bash(echo:*)", "--settings", settings], prompt
    )
    return codeword_between(screen.splitlines(), is_claude_tool_line), screen


def is_claude_tool_line(line: str) -> bool:
    """Claude draws a finished shell call as `Ran N shell command(s)` or `⏺ Bash(...)`."""
    text = line.strip()
    return ("Ran " in text and "shell command" in text) or text.lstrip("⏺ ").startswith("Bash(")


def is_codex_tool_line(line: str) -> bool:
    """Codex prints each shell call as `<shell> -lc '<command>' in <dir>`."""
    return " -lc " in line and "echo " in line


def codeword_between(lines: list[str], is_tool_line) -> bool:
    """The model wrote the codeword AFTER one tool call and BEFORE another, in one turn.

    The echoed prompt contains `CODEWORD=<...>`, so only a line carrying the
    real codeword counts, and the first such line is the one the model wrote
    between the calls (a later summary may repeat it).
    """
    written = [i for i, line in enumerate(lines) if f"CODEWORD={CODEWORD}" in line]
    tools = [i for i, line in enumerate(lines) if is_tool_line(line)]
    if not written:
        return False
    first = written[0]
    return any(t < first for t in tools) and any(t > first for t in tools)


def probe_claude_midturn(a, scratch):
    return claude_midturn(a, scratch, "PostToolUse")


def probe_claude_midturn_pre(a, scratch):
    return claude_midturn(a, scratch, "PreToolUse")


def make_codex_home(scratch: str) -> Path:
    """A throwaway CODEX_HOME: a copy of auth.json, a minimal config, no MCP servers, no hooks."""
    home = Path(scratch) / "codex-home"
    if home.exists():
        return home
    home.mkdir(mode=0o700)
    source = Path(os.environ.get("CODEX_HOME", Path.home() / ".codex"))
    auth = source / "auth.json"
    if auth.is_file():
        shutil.copy2(auth, home / "auth.json")
        os.chmod(home / "auth.json", 0o600)
    model = ""
    config = source / "config.toml"
    if config.is_file():
        for line in config.read_text(encoding="utf-8").splitlines():
            if line.replace(" ", "").startswith("model="):
                model = line + "\n"
                break
    (home / "config.toml").write_text(
        model + 'approval_policy = "never"\nsandbox_mode = "workspace-write"\n', encoding="utf-8"
    )
    return home


def codex_exec(scratch: str, event: str, hook_command: str, prompt: str, timeout: int | None = None):
    home = make_codex_home(scratch)
    log = os.path.join(scratch, "codex-hook.log")
    open(log, "w", encoding="utf-8").close()
    timeout_part = f",timeout={timeout}" if timeout else ""
    hooks = f'hooks.{event}=[{{matcher=".*",hooks=[{{type="command",command="{hook_command}"{timeout_part}}}]}}]'
    work = Path(scratch) / "codex-work"
    work.mkdir(exist_ok=True)
    out = subprocess.run(
        ["codex", "exec", "--skip-git-repo-check", "-s", "workspace-write",
         "--dangerously-bypass-hook-trust", "-c", hooks, prompt],
        cwd=work,
        env=clean_env(CODEX_HOME=str(home)),
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
        timeout=300,
        check=False,
    )
    with open(log, encoding="utf-8") as fh:
        calls = [json.loads(line) for line in fh if line.strip().startswith("{")]
    return out.stdout + out.stderr, calls


def logging_hook(scratch: str, name: str, tail: str) -> str:
    log = os.path.join(scratch, "codex-hook.log")
    return script(scratch, name, f"#!/bin/sh\ncat >> {shlex.quote(log)}\necho >> {shlex.quote(log)}\n{tail}\n")


DENY_JSON = json.dumps(
    {"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": DENY_REASON}}
)
ECHO_PROMPT = (
    "Run the shell command `echo PROBE-RAN` exactly once. If it is blocked, quote the error text "
    "verbatim. Do not retry."
)


def ran(text: str) -> bool:
    """The probe command's own output reached the transcript (the tool really ran)."""
    return "PROBE-RAN" in text.replace("echo PROBE-RAN", "")


def probe_codex_deny(a, scratch):
    hook = logging_hook(scratch, "deny.sh", f"printf '%s' {shlex.quote(DENY_JSON)}")
    text, calls = codex_exec(scratch, "PreToolUse", hook, ECHO_PROMPT)
    return bool(calls) and DENY_REASON in text and not ran(text), text


def probe_codex_question(a, scratch):
    hook = logging_hook(scratch, "deny.sh", f"printf '%s' {shlex.quote(DENY_JSON)}")
    text, calls = codex_exec(
        scratch, "PreToolUse", hook,
        'Call the request_user_input tool exactly once to ask me "Which color?" with options red '
        "and blue. If it is blocked or unavailable, quote the exact error text verbatim. Use no other tool.",
    )
    seen = [c for c in calls if c.get("tool_name") == "request_user_input"]
    ok = bool(seen) and bool(seen[0].get("tool_input", {}).get("questions"))
    return ok, text + "\n" + json.dumps(seen, indent=1)


def codex_midturn(scratch, event: str):
    body = json.dumps({"hookSpecificOutput": {"hookEventName": event, "additionalContext": f"The codeword is {CODEWORD}."}})
    hook = logging_hook(scratch, f"midturn-{event}.sh", f"printf '%s' {shlex.quote(body)}")
    text, _ = codex_exec(
        scratch, event, hook,
        "Run the shell command `echo one`. Then, before running anything else, write the line "
        "CODEWORD=<the codeword you were given after that command, or none>. Then run `echo two`, and stop.",
    )
    return codeword_between(text.splitlines(), is_codex_tool_line), text


def probe_codex_midturn(a, scratch):
    return codex_midturn(scratch, "PostToolUse")


def probe_codex_midturn_pre(a, scratch):
    return codex_midturn(scratch, "PreToolUse")


def failing_probe(scratch: str, name: str, tail: str, timeout: int | None = None):
    hook = logging_hook(scratch, name, tail)
    text, calls = codex_exec(scratch, "PreToolUse", hook, ECHO_PROMPT, timeout)
    return bool(calls) and ran(text), text


def probe_codex_crash(a, scratch):
    return failing_probe(scratch, "crash.sh", "echo boom >&2\nexit 1")


def probe_codex_garbage(a, scratch):
    return failing_probe(scratch, "garbage.sh", "printf 'not json at all'")


def probe_codex_missing(a, scratch):
    text, _ = codex_exec(scratch, "PreToolUse", os.path.join(scratch, "no-such-hook"), ECHO_PROMPT)
    return ran(text), text


def probe_codex_timeout(a, scratch):
    return failing_probe(scratch, "slow.sh", "sleep 20", timeout=3)


PROBES = {
    "claude-answer": probe_claude_answer,
    "claude-disallow": probe_claude_disallow,
    "claude-timeout": probe_claude_timeout,
    "claude-onfailure": probe_claude_onfailure,
    "claude-missing": probe_claude_missing,
    "claude-exitplan": probe_claude_exitplan,
    "claude-midturn": probe_claude_midturn,
    "claude-midturn-pre": probe_claude_midturn_pre,
    "codex-deny": probe_codex_deny,
    "codex-question": probe_codex_question,
    "codex-midturn": probe_codex_midturn,
    "codex-midturn-pre": probe_codex_midturn_pre,
    "codex-crash": probe_codex_crash,
    "codex-garbage": probe_codex_garbage,
    "codex-missing": probe_codex_missing,
    "codex-timeout": probe_codex_timeout,
}


def version(cmd: str) -> str:
    try:
        return subprocess.run([cmd, "--version"], capture_output=True, text=True, timeout=20).stdout.strip()
    except (OSError, subprocess.TimeoutExpired) as err:
        return f"unavailable ({err})"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("probes", nargs="*", choices=list(PROBES), help="default: all")
    parser.add_argument("--cwd", required=True, help="a folder Claude already trusts")
    parser.add_argument("--claude-model", default="haiku")
    parser.add_argument("--verbose", action="store_true", help="print each probe's screen or output")
    a = parser.parse_args()
    if not shutil.which("tmux"):
        print("tmux is required", file=sys.stderr)
        return 2
    print(f"claude: {version('claude')}  codex: {version('codex')}")
    failed = 0
    with tempfile.TemporaryDirectory(prefix="adr0034-") as scratch:
        for name in a.probes or list(PROBES):
            ok, evidence = PROBES[name](a, scratch)
            failed += not ok
            print(f"{'PASS' if ok else 'FAIL'}  {name}", flush=True)
            if a.verbose or not ok:
                lines = [line for line in evidence.splitlines() if line.strip()]
                print("\n".join("    " + line for line in lines[-30:]))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
