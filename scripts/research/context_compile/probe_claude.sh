#!/usr/bin/env bash
# probe_claude.sh <cwd> <out.json> <prompt-file> [claude args...]
#
# One throwaway `claude -p` call for the #1042 cache measurements (ADR-0032).
# Isolated from the operator's setup: a scrubbed environment (no FLOCK_*
# variables, so nothing reaches a live flk server), project settings only (no
# user hooks), no MCP servers, no skills, no saved session. Prints the call's
# cache read / creation tokens and cost from the JSON result.
#
# Example (second builder in another worktree, layers 1-2 in the system prompt):
#   probe_claude.sh wtB out.json task.txt \
#     --exclude-dynamic-system-prompt-sections --append-system-prompt-file shared.md
set -u
cwd=$1; out=$2; prompt=$3; shift 3
model=${PROBE_MODEL:-claude-haiku-5-5}
cd "$cwd" && env -i HOME="$HOME" USER="$USER" LOGNAME="$LOGNAME" TMPDIR="${TMPDIR:-/tmp}" \
  SHELL="$SHELL" PATH="$PATH" TERM=dumb CLAUDE_CONFIG_DIR="${CLAUDE_CONFIG_DIR:-$HOME/.claude}" \
  claude -p --model "$model" --output-format json --setting-sources project --strict-mcp-config \
  --disable-slash-commands --no-session-persistence "$@" < "$prompt" > "$out" 2> "$out.err"
python3 -I - "$out" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
u = d.get("usage", {})
print(sys.argv[1].rsplit("/", 1)[-1], "turns", d.get("num_turns"),
      "read", u.get("cache_read_input_tokens"), "created", u.get("cache_creation_input_tokens"),
      "input", u.get("input_tokens"), "output", u.get("output_tokens"),
      "ms", d.get("duration_ms"), "usd", d.get("total_cost_usd"))
PY
