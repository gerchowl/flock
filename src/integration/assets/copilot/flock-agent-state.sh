#!/bin/sh
# installed by flock
# managed by flock; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# FLOCK_INTEGRATION_ID=copilot
# FLOCK_INTEGRATION_VERSION=2
# The event mapping and socket reports live in `flk hook copilot event`.

# Outside a flock pane this is a clean no-op. A missing binary inside a pane
# prints a diagnostic on each hook call without failing the parent agent.
[ "${FLOCK_ENV:-}" = "1" ] || exit 0
[ -n "${FLOCK_SOCKET_PATH:-}" ] || exit 0
[ -n "${FLOCK_PANE_ID:-}" ] || exit 0
flock_bin="${FLOCK_BIN:-flk}"
flock_bin_executable=1
case "$flock_bin" in
    */*) [ -x "$flock_bin" ] || flock_bin_executable=0 ;;
esac
if [ "$flock_bin_executable" = 0 ] || ! command -v "$flock_bin" >/dev/null 2>&1; then
    printf 'flock hook: required binary %s not found or not executable\n' "$flock_bin" >&2
    exit 0
fi

"$flock_bin" hook copilot event 2>/dev/null
exit 0
