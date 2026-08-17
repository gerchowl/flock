#!/bin/sh
# installed by flock
# managed by flock; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# FLOCK_INTEGRATION_ID=kimi
# FLOCK_INTEGRATION_VERSION=2
#
# Thin stub (#158, #238). The hook body — validate the pane env, build the
# pane.report_* request, and speak the flock socket — lives in the flk binary
# at `flk hook kimi <action>`, the single source of truth shared by every agent
# integration. This forwards the action ($1) and stdin (the hook JSON, if the
# host sends any) and stays out of the way.
#
# Actions: working|idle|blocked|release
#
# Replaces an embedded python3 heredoc. python3 was a hard dependency resolved
# from ambient PATH, and its absence was indistinguishable from "not running
# under flock" — the shim exited 0 either way (#238). flk is already stamped
# into the pane env, so the dependency is now gone rather than merely pinned.
#
# FLOCK_BIN is stamped into the pane env by flock (falls back to `flk` on PATH).
# A hook must never fail the parent agent, so a missing binary or any error is
# a silent no-op: we swallow stderr and always exit 0.

"${FLOCK_BIN:-flk}" hook kimi "${1:-}" 2>/dev/null
exit 0
