#!/bin/sh
# installed by flock
# managed by flock; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# FLOCK_INTEGRATION_ID=claude
# FLOCK_INTEGRATION_VERSION=9
#
# Thin stub (#158). The hook body — parse the payload, report over the flock
# socket, scrape the transcript, lift the recap sentinel, emit the Stop nudge —
# lives in the flk binary at `flk hook claude <action>`, the single source of
# truth shared by every agent integration. This forwards the action ($1) and
# stdin (the hook JSON) and stays out of the way; flk writes the Stop nudge to
# stdout, which we pass through untouched.
#
# FLOCK_BIN is stamped into the pane env by flock (falls back to `flk` on PATH).
# A hook must never fail the parent agent, so a missing binary or any error is
# a silent no-op: we swallow stderr and always exit 0.

"${FLOCK_BIN:-flk}" hook claude "${1:-}" 2>/dev/null
exit 0
