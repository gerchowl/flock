#!/usr/bin/env bash
# Run toolchain_survey.py read-only on each given ssh host and fetch the JSON.
#
#   run_survey.sh OUT_DIR HOST... [-- COLLECTOR_ARGS...]
#
# For each HOST: copy the collector to a unique /tmp path, run it with
# `python3 -I`, fetch the JSON to OUT_DIR/<host>.json, then delete the remote
# copies. An unreachable host is reported and skipped. OUT_DIR holds redacted
# but private data: keep it outside the repository.
set -euo pipefail

if [ $# -lt 2 ]; then
  echo "usage: $0 OUT_DIR HOST... [-- COLLECTOR_ARGS...]" >&2
  exit 2
fi
out=$1
shift
hosts=()
while [ $# -gt 0 ] && [ "$1" != "--" ]; do
  hosts+=("$1")
  shift
done
[ "${1:-}" = "--" ] && shift
collector="$(cd "$(dirname "$0")" && pwd)/toolchain_survey.py"
mkdir -p "$out"

for host in "${hosts[@]}"; do
  if ! ssh -o BatchMode=yes -o ConnectTimeout=5 "$host" true 2>/dev/null; then
    echo "$host: unreachable, skipped" >&2
    continue
  fi
  remote="/tmp/toolchain-survey-$$-$RANDOM"
  scp -q -o BatchMode=yes "$collector" "$host:$remote.py"
  # Arguments are passed through printf %q so globs reach the collector unexpanded.
  ssh -o BatchMode=yes "$host" "python3 -I $remote.py $(printf '%q ' "$@") > $remote.json" \
    && scp -q -o BatchMode=yes "$host:$remote.json" "$out/${host%%.*}.json" \
    && echo "$host: $out/${host%%.*}.json" >&2
  ssh -o BatchMode=yes "$host" "rm -f $remote.py $remote.json"
done
