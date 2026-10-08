#!/bin/sh
# installed by flock
# managed by flock; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# FLOCK_INTEGRATION_ID=codex
# FLOCK_INTEGRATION_VERSION=7
#
# Thin stub (#158, #238). The hook body — validate the pane env, build the
# pane.report_* request, and speak the flock socket — lives in the flk binary
# at `flk hook codex <action>`, the single source of truth shared by every agent
# integration. This forwards the action ($1) and stdin (the hook JSON, if the
# host sends any) and stays out of the way.
#
# Actions: session (the only action codex's host emits)
#
# Replaces an embedded python3 heredoc. python3 was a hard dependency resolved
# from ambient PATH, and its absence was indistinguishable from "not running
# under flock" — the shim exited 0 either way (#238). flk is already stamped
# into the pane env, so the dependency is now gone rather than merely pinned.
#
# FLOCK_BIN is stamped into the pane env by flock (falls back to `flk` on PATH).
# Outside a flock pane this is a clean no-op. A missing binary inside a pane
# is a dependency error, reported once without failing the parent agent.
[ "${FLOCK_ENV:-}" = "1" ] || exit 0
[ -n "${FLOCK_SOCKET_PATH:-}" ] || exit 0
[ -n "${FLOCK_PANE_ID:-}" ] || exit 0
flock_bin="${FLOCK_BIN:-flk}"
if ! command -v "$flock_bin" >/dev/null 2>&1; then
    printf 'flock hook: required binary %s not found or not executable\n' "$flock_bin" >&2
    exit 0
fi

"$flock_bin" hook codex "${1:-}" 2>/dev/null
exit 0
