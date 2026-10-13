#!/bin/sh
# installed by flock
# managed by flock; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# FLOCK_INTEGRATION_ID=claude
# FLOCK_INTEGRATION_VERSION=11
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
#
# The one exception is the question action in a pane whose FLOCK_QUESTIONS
# hint is not ask_human (ADR-0034 §4). There a question tool is refused rather
# than let through to a dialog nobody watches, and this stub refuses it itself
# when flk is missing, exits non-zero, prints nothing or anything but JSON, or
# outlives a 10 s watchdog (below the entry's 30 s timeout).
[ "${FLOCK_ENV:-}" = "1" ] || exit 0
[ -n "${FLOCK_SOCKET_PATH:-}" ] || exit 0
[ -n "${FLOCK_PANE_ID:-}" ] || exit 0
reason=
if [ "${1:-}" = question ]; then
    case "${FLOCK_QUESTIONS:-ask_human}" in
        ask_human) ;;
        route) reason="flk could not reach your architect. Do not ask the human. Choose the most conservative option, state the assumption in your result, or end your turn with a blocked report." ;;
        ask_operator) reason="flk could not reach the operator's question queue, and nobody is watching this pane. Choose the most conservative option, state the assumption in your result, or end your turn with a blocked report." ;;
        *) reason="Questions are disabled for this agent. Choose the most conservative option, state the assumption in your result, or end your turn with a blocked report." ;;
    esac
fi
deny() {
    printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"%s"}}\n' "$reason"
    exit 0
}
flock_bin="${FLOCK_BIN:-flk}"
flock_bin_executable=1
case "$flock_bin" in
    */*) [ -x "$flock_bin" ] || flock_bin_executable=0 ;;
esac
if [ "$flock_bin_executable" = 0 ] || ! command -v "$flock_bin" >/dev/null 2>&1; then
    printf 'flock hook: required binary %s not found or not executable\n' "$flock_bin" >&2
    [ -n "$reason" ] && deny
    exit 0
fi

if [ -z "$reason" ]; then
    "$flock_bin" hook claude "${1:-}" 2>/dev/null
    exit 0
fi

out=$(mktemp "${TMPDIR:-/tmp}/flock-question.XXXXXX") || deny
# A background job reads /dev/null unless told otherwise: hand it the payload.
exec 3<&0
"$flock_bin" hook claude question <&3 >"$out" 2>/dev/null &
flk_pid=$!
(
    trap 'kill "$nap" 2>/dev/null; exit 0' TERM
    sleep 10 &
    nap=$!
    wait "$nap"
    kill -9 "$flk_pid" 2>/dev/null
) >/dev/null 2>&1 &
watchdog_pid=$!
# The braces keep the shell's own "Killed" report off the hook's stderr.
{ wait "$flk_pid"; } 2>/dev/null
status=$?
kill "$watchdog_pid" 2>/dev/null
verdict=$(cat "$out")
rm -f "$out"
[ "$status" = 0 ] || deny
case "$verdict" in
    "{"*) printf '%s\n' "$verdict" ;;
    *) deny ;;
esac
exit 0
