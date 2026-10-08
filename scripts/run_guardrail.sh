#!/bin/sh
# Load hook tooling automatically for shells outside the repository devShell.
# The Python gates need tomllib, which the macOS system Python does not provide.
tool="$1"
shift
if command -v "$tool" >/dev/null 2>&1 && {
    [ "$tool" != python3 ] || "$tool" -c 'import tomllib' >/dev/null 2>&1
}; then
    exec "$tool" "$@"
fi
if [ "${FLOCK_GUARDRAIL_IN_DEVSHELL:-}" != 1 ] && command -v nix >/dev/null 2>&1; then
    repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd) || exit 1
    export FLOCK_GUARDRAIL_IN_DEVSHELL=1
    exec nix develop "$repo_root" --command "$tool" "$@"
fi
printf '%s not on PATH or missing required runtime support: cannot run hook tooling. Load the repository environment with `nix develop`, or run `direnv allow` in this worktree and load it in a direnv-enabled shell.\n' "$tool" >&2
exit 127
