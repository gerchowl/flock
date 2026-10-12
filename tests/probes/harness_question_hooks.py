#!/usr/bin/env python3
"""Re-measure what each harness lets a hook do with a question tool (ADR-0034).

ADR-0034's mechanism choice rests on six facts about the installed harnesses.
Each one is a probe here, so a harness upgrade can be re-checked in a minute
instead of re-argued:

  claude-answer    a PreToolUse hook answers AskUserQuestion through
                   `updatedInput.answers`; no dialog is drawn and the model
                   receives the hook's answer
  claude-disallow  `--disallowedTools AskUserQuestion` removes the tool from an
                   interactive session
  claude-timeout   a PreToolUse hook that times out falls through to the
                   dialog (fail open)
  claude-onfailure the same hook with `"onFailure": "block"` blocks the call
                   instead (Claude Code 2.1.295 and later)
  codex-deny       a Codex PreToolUse deny reaches the model as the tool's
                   error text
  codex-question   Codex offers `request_user_input` in default mode, and its
                   call passes through PreToolUse with structured questions

Nothing here talks to flk. Every harness runs with the inherited FLOCK_*
variables removed, so a probe started from inside a flk pane can never reach the
live server, and Claude runs inside a private tmux server (`tmux -L`), never the
operator's. Claude's interactive probes need `--cwd` to be a folder Claude
already trusts (a flk-created worktree is pre-trusted, #605), because an
untrusted folder holds every settings hook back behind the trust dialog. Each
probe costs one short model turn (Claude on `--claude-model`, default haiku).

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

TMUX_SERVER = "flk-adr0034-probe"

ANSWER_HOOK = r"""
import json, sys
d = json.load(sys.stdin)
ti = dict(d.get("tool_input", {}))
ti["answers"] = {q["question"]: q["options"][0]["label"] for q in ti.get("questions", []) if q.get("options")}
print(json.dumps({"hookSpecificOutput": {"hookEventName": "PreToolUse",
      "permissionDecision": "allow", "updatedInput": ti}}))
"""

DENY_REASON = "PROBE-DENY: ask your architect via flk instead"


def scrubbed_vars() -> list[str]:
    """Every inherited FLOCK_* variable, plus TMUX so the private server starts clean."""
    return sorted(k for k in os.environ if k.startswith("FLOCK_")) + ["TMUX"]


def clean_env() -> dict[str, str]:
    return {k: v for k, v in os.environ.items() if k not in scrubbed_vars()}


# A Claude turn is over when the dialog is drawn or the turn's footer shows.
TURN_OVER = ("Enter to select", "Worked for", "Sautéed for", "done ")


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


def claude_turn(cwd: str, model: str, extra: list[str], prompt: str, wait_s: float) -> str:
    """One interactive Claude turn in a private tmux server; returns the screen."""
    command = " ".join(shlex.quote(a) for a in ["claude", "--model", model, *extra])
    env_prefix = "env " + " ".join(f"-u {k}" for k in scrubbed_vars())
    tmux("kill-server")
    tmux("new-session", "-d", "-s", "p", "-x", "160", "-y", "45", "-c", cwd, f"{env_prefix} {command}")
    wait_for(lambda screen: "❯" in screen, 30)
    tmux("send-keys", "-t", "p", "-l", prompt)
    time.sleep(0.5)
    tmux("send-keys", "-t", "p", "Enter")
    wait_for(lambda screen: any(m in screen for m in TURN_OVER), wait_s)
    time.sleep(2)
    screen = tmux("capture-pane", "-p", "-t", "p")
    tmux("kill-server")
    return screen


def reply_lines(screen: str) -> list[str]:
    """The model's own reply lines (Claude marks them with ⏺), never the echoed prompt."""
    return [line.strip().lstrip("⏺").strip() for line in screen.splitlines() if line.strip().startswith("⏺")]


def settings_file(scratch: str, name: str, hook: dict) -> str:
    path = os.path.join(scratch, f"{name}.json")
    with open(path, "w", encoding="utf-8") as fh:
        json.dump({"hooks": {"PreToolUse": [{"matcher": "AskUserQuestion", "hooks": [hook]}]}}, fh)
    return path


ASK = (
    'Use the AskUserQuestion tool exactly once to ask me "Which color?" with options '
    "red and blue. If it fails or is blocked, quote the error text verbatim. Then reply "
    "with only the answer you received. Use no other tool."
)


def probe_claude_answer(a: argparse.Namespace, scratch: str) -> tuple[bool, str]:
    hook_py = os.path.join(scratch, "answer_hook.py")
    with open(hook_py, "w", encoding="utf-8") as fh:
        fh.write(ANSWER_HOOK)
    settings = settings_file(
        scratch, "answer", {"type": "command", "command": f"python3 -I {hook_py}", "timeout": 30}
    )
    screen = claude_turn(a.cwd, a.claude_model, ["--settings", settings], ASK, 60)
    ok = "Which color? → red" in screen and "Enter to select" not in screen
    return ok, screen


def probe_claude_disallow(a: argparse.Namespace, scratch: str) -> tuple[bool, str]:
    prompt = (
        "Is a tool named AskUserQuestion in your tool list? Answer YES or NO first. If YES, "
        'use it once to ask "Which color?" with options red and blue. Use no other tool.'
    )
    screen = claude_turn(a.cwd, a.claude_model, ["--disallowedTools", "AskUserQuestion"], prompt, 60)
    ok = "Enter to select" not in screen and any(line.startswith("NO") for line in reply_lines(screen))
    return ok, screen


def probe_claude_timeout(a: argparse.Namespace, scratch: str) -> tuple[bool, str]:
    settings = settings_file(scratch, "timeout", {"type": "command", "command": "sleep 15", "timeout": 5})
    screen = claude_turn(a.cwd, a.claude_model, ["--settings", settings], ASK, 60)
    return "Enter to select" in screen, screen


def probe_claude_onfailure(a: argparse.Namespace, scratch: str) -> tuple[bool, str]:
    settings = settings_file(
        scratch,
        "onfailure",
        {"type": "command", "command": "sleep 15", "timeout": 5, "onFailure": "block"},
    )
    screen = claude_turn(a.cwd, a.claude_model, ["--settings", settings], ASK, 60)
    ok = "timed out; blocking" in screen and "Enter to select" not in screen
    return ok, screen


def codex_exec(scratch: str, prompt: str) -> tuple[str, list[dict]]:
    log = os.path.join(scratch, "codex-hook.log")
    hook = os.path.join(scratch, "deny_hook.sh")
    with open(hook, "w", encoding="utf-8") as fh:
        fh.write(
            "#!/bin/sh\n"
            f"cat >> {shlex.quote(log)}\n"
            f"echo >> {shlex.quote(log)}\n"
            "printf '%s' '"
            + json.dumps(
                {
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "deny",
                        "permissionDecisionReason": DENY_REASON,
                    }
                }
            )
            + "'\n"
        )
    os.chmod(hook, 0o755)
    open(log, "w", encoding="utf-8").close()
    hooks = f'hooks.PreToolUse=[{{matcher=".*",hooks=[{{type="command",command="{hook}"}}]}}]'
    out = subprocess.run(
        ["codex", "exec", "--skip-git-repo-check", "--dangerously-bypass-hook-trust", "-c", hooks, prompt],
        cwd=scratch,
        env=clean_env(),
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
        timeout=240,
        check=False,
    )
    with open(log, encoding="utf-8") as fh:
        calls = [json.loads(line) for line in fh if line.strip()]
    return out.stdout + out.stderr, calls


def probe_codex_deny(a: argparse.Namespace, scratch: str) -> tuple[bool, str]:
    text, calls = codex_exec(
        scratch, "Run the shell command `echo hi` exactly once. If it is blocked, quote the error text verbatim."
    )
    ok = any(c.get("hook_event_name") == "PreToolUse" for c in calls) and DENY_REASON in text
    return ok, text


def probe_codex_question(a: argparse.Namespace, scratch: str) -> tuple[bool, str]:
    text, calls = codex_exec(
        scratch,
        'Call the request_user_input tool exactly once to ask me "Which color?" with options red '
        "and blue. If it is blocked or unavailable, quote the exact error text verbatim. Use no other tool.",
    )
    seen = [c for c in calls if c.get("tool_name") == "request_user_input"]
    ok = bool(seen) and bool(seen[0].get("tool_input", {}).get("questions"))
    return ok, text + "\n" + json.dumps(seen, indent=1)


PROBES = {
    "claude-answer": probe_claude_answer,
    "claude-disallow": probe_claude_disallow,
    "claude-timeout": probe_claude_timeout,
    "claude-onfailure": probe_claude_onfailure,
    "codex-deny": probe_codex_deny,
    "codex-question": probe_codex_question,
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
            print(f"{'PASS' if ok else 'FAIL'}  {name}")
            if a.verbose or not ok:
                lines = [line for line in evidence.splitlines() if line.strip()]
                print("\n".join("    " + line for line in lines[-30:]))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
