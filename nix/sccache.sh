# shellcheck shell=sh
# sccache 0.15 has no server-only TMPDIR setting. Apply it on every invocation
# so automatic startup, explicit startup, and idle restarts use the same root.
# Leave artifact storage unset by default so sccache keeps its platform cache.
if [ -n "${SCCACHE_DIR:-}" ]; then
  sccache_runtime_root="$SCCACHE_DIR"
elif [ -n "${HOME:-}" ]; then
  sccache_runtime_root="$HOME/.cache/flock-sccache"
else
  printf 'sccache: HOME is unset and SCCACHE_DIR is not given\n' >&2
  exit 1
fi
export TMPDIR="$sccache_runtime_root/tmp"
if ! mkdir -p "$TMPDIR"; then
  printf 'sccache: cannot create persistent temp directory: %s\n' "$TMPDIR" >&2
  exit 1
fi
# A separate stable endpoint avoids inheriting a daemon started outside this
# devShell with a disposable TMPDIR. Explicit endpoints remain opt-in overrides.
if [ -z "${SCCACHE_SERVER_UDS:-}" ] && [ -z "${SCCACHE_SERVER_PORT:-}" ]; then
  export SCCACHE_SERVER_UDS="$sccache_runtime_root/flock.sock"
fi
exec "$SCCACHE_REAL" "$@"
