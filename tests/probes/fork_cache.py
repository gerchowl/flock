#!/usr/bin/env python3
"""Measurement runbook behind ADR-0033 (#1043): what a fork of a warm Claude
session costs on its first turn.

Run by hand, never by `just check`. It drives an ISOLATED flk (a copied
binary and a short sandbox XDG directory, as README.md in this directory and
agent_wake.py describe). Never point it at a live server.

Setup (once per run):

    SB=/tmp/fk1   # keep it short: sun_path caps near 104 bytes
    mkdir -p $SB/bin $SB/x/flock $SB/repo
    cp "$(readlink -f "$(command -v flk)")" $SB/bin/flk
    printf 'onboarding = false\\n' > $SB/x/flock/config.toml
    # A server started from inside a Claude session inherits CLAUDECODE and
    # CLAUDE_CODE_CHILD_SESSION, its Claude panes then run with transcript
    # saving off, and `agent.fork` refuses (no transcript on disk). Unset them.
    env -u FLOCK_ENV -u FLOCK_SOCKET_PATH -u FLOCK_PANE_ID -u FLOCK_BIN \\
        -u CLAUDECODE -u CLAUDE_CODE_CHILD_SESSION -u CLAUDE_CODE_SESSION_ID \\
        -u CLAUDE_CODE_ENTRYPOINT -u CLAUDE_PID \\
        XDG_CONFIG_HOME=$SB/x XDG_STATE_HOME=$SB/s $SB/bin/flk server &

Then, with the same environment:

    1. `flk agent start parent --cwd $SB/repo -- claude --model <m> ...`,
       accept the trust dialog, and give it one turn that reads enough files
       to build a large context (the ADR used ~84k and ~136k tokens).
    2. When it is idle, fork it, either through the verb
       (`flk agent fork <pane> --branch ax/probe --pivot "reply FORKED"`,
       which also creates a linked worktree) or in place
       (`flk agent start forkb --cwd $SB/repo -- claude --model <m>
       --resume <session id> --fork-session "reply FORKED"`).
    3. Read the usage of each session with this script:

        python3 -I tests/probes/fork_cache.py usage <session id>

The fork's first assistant message is the line to compare with the parent's
last one: `cache_read` close to the parent's total context means the fork
re-used the parent's cached prefix. Fork onto a different `--model` to see
the opposite (only the harness's shared system prefix hits).

Gotchas seen while producing the ADR's figures:

- `gh` inside a sandbox pane reads its config from `$XDG_CONFIG_HOME/gh`, so
  the sandbox's XDG override hides the operator's gh login (exit 4).
- `agent.fork` puts the worktree under the default worktree directory in the
  real `$HOME`, not in the sandbox. Remove it with `git worktree remove`
  from the sandbox repo afterwards.
- Stop hooks of the inherited Claude profile can hold every turn for
  minutes; read the transcript, not the pane status, for the answer time.
"""
import argparse
import glob
import json
import os
import sys


def transcript(session_id):
    root = os.environ.get("CLAUDE_CONFIG_DIR") or os.path.expanduser("~/.claude")
    paths = glob.glob(os.path.join(root, "projects", "*", f"{session_id}.jsonl"))
    if not paths:
        sys.exit(f"no transcript for session {session_id} under {root}/projects")
    return paths[0]


def usage(session_id):
    seen = set()
    rows = []
    with open(transcript(session_id)) as handle:
        for line in handle:
            entry = json.loads(line)
            if entry.get("type") != "assistant":
                continue
            message = entry["message"]
            if message.get("id") in seen:
                continue
            seen.add(message.get("id"))
            used = message.get("usage", {})
            read = used.get("cache_read_input_tokens") or 0
            created = used.get("cache_creation_input_tokens") or 0
            fresh = used.get("input_tokens") or 0
            total = read + created + fresh
            rows.append(
                {
                    "at": entry.get("timestamp"),
                    "model": message.get("model"),
                    "cache_read": read,
                    "cache_create": created,
                    "input": fresh,
                    "output": used.get("output_tokens"),
                    "hit": round(read / total, 4) if total else None,
                    "ttl_1h": (used.get("cache_creation") or {}).get("ephemeral_1h_input_tokens"),
                }
            )
    for row in rows:
        print(json.dumps(row))


def main():
    parser = argparse.ArgumentParser(description="Token usage of a Claude session, for the ADR-0033 fork-cost runbook.")
    sub = parser.add_subparsers(dest="command", required=True)
    one = sub.add_parser("usage", help="per-message token usage of one Claude session")
    one.add_argument("session_id")
    args = parser.parse_args()
    if args.command == "usage":
        usage(args.session_id)


if __name__ == "__main__":
    main()
