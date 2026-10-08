#!/bin/sh
# Keep hook failures actionable when a fresh worktree has no devShell loaded.
tool="$1"
shift
if ! command -v "$tool" >/dev/null 2>&1; then
    printf '%s not on PATH: run inside `nix develop` (for commits: `nix develop --command git commit`), or run `direnv allow` in this worktree and load it in a direnv-enabled shell.\n' "$tool" >&2
    exit 127
fi
exec "$tool" "$@"
