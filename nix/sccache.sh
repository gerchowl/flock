# shellcheck shell=sh
# sccache 0.18 has no server-only TMPDIR setting. Apply it on every invocation
# so automatic startup, explicit startup, and idle restarts use the same root.
# Resolve the account home, not an agent's isolated HOME or XDG_CACHE_HOME.
# Keep the usual platform artifact location warm across isolated environments.
if [ -z "${SCCACHE_DIR:-}" ]; then
  sccache_account_home=$("$SCCACHE_PYTHON" -c 'import os, pwd; print(pwd.getpwuid(os.getuid()).pw_dir)') || exit 1
  if [ -z "$sccache_account_home" ]; then
    printf 'sccache: cannot resolve account home; set SCCACHE_DIR\n' >&2
    exit 1
  fi
  export SCCACHE_DIR="$sccache_account_home/$SCCACHE_CACHE_SUFFIX"
fi
sccache_runtime_root="$SCCACHE_DIR"
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
