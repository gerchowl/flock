#!/bin/sh
# installed by flock
# managed by flock; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# FLOCK_INTEGRATION_ID=claude
# FLOCK_INTEGRATION_VERSION=10
#
# Thin stub (#158). The hook body — parse the payload, report over the flock
# socket, scrape the transcript, lift the recap sentinel, emit the Stop nudge —
# lives in the flk binary at `flk hook claude <action>`, the single source of
# truth shared by every agent integration. This forwards the action ($1) and
# stdin (the hook JSON) and stays out of the way; flk writes the Stop nudge to
# stdout, which we pass through untouched.
#
# FLOCK_BIN is stamped into the pane env by flock (falls back to `flk` on PATH).
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

"$flock_bin" hook claude "${1:-}" 2>/dev/null
exit 0
