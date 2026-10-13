# shellcheck shell=sh
# sccache 0.15 has no server-only TMPDIR setting. Apply it on every invocation
# so automatic startup, explicit startup, and idle restarts use the same root.
export SCCACHE_DIR="${SCCACHE_DIR:-$HOME/.cache/sccache}"
export TMPDIR="$SCCACHE_DIR/tmp"
if ! mkdir -p "$TMPDIR"; then
  printf 'sccache: cannot create persistent temp directory: %s\n' "$TMPDIR" >&2
  exit 1
fi
# A separate stable endpoint avoids inheriting a daemon started outside this
# devShell with a disposable TMPDIR. Explicit endpoints remain opt-in overrides.
if [ -z "${SCCACHE_SERVER_UDS:-}" ] && [ -z "${SCCACHE_SERVER_PORT:-}" ]; then
  export SCCACHE_SERVER_UDS="$SCCACHE_DIR/flock.sock"
fi
exec "$SCCACHE_REAL" "$@"
