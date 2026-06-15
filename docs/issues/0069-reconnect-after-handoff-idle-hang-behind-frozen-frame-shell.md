---
number: 69
title: "reconnect after handoff: idle hang behind frozen frame + shell left unusable (proto-16 hold path)"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-11T20:52:41Z
closed: 2026-06-11T22:29:30Z
url: https://github.com/gerchowl/herdr/issues/69
---

# reconnect after handoff: idle hang behind frozen frame + shell left unusable (proto-16 hold path)

## Bug (live, first proto-16 session)

After the deploy's reconnect (live-handoff reattach), the client sat **idle hanging**, and after escaping, **the shell was unusable** (terminal left in a bad state — raw mode / alt-screen not restored).

## Hypothesis (strong)

PR #67 introduced SWITCH_HANDOFF_PENDING: the exiting client HOLDS alt-screen + raw mode so the next leg paints over the frozen frame, with `force_restore_host_terminal()` as the launcher's escape when a held chain dies. Two suspect interactions:

1. **Frozen frame + #52 retry window = perceived hang**: a handoff-refused reattach silently retries up to ~30s behind the FROZEN last frame (the retry's spinner line may be invisible behind the held screen) — user sees a dead client.
2. **Unusable shell = a held-terminal exit path that misses restore**: the hold was designed for the SWITCH path; the live-handoff RECONNECT path (#38/#52 retry inside one leg, not a leg change) may set/hold without the launcher knowing to reclaim — e.g. user ctrl-C's the "hung" client → process exits with raw mode + alt screen still held, no force_restore runs.

## Fix sketch
- The retry/handoff wait must render its status line ON the held screen (visible progress, not frozen silence).
- Restore-on-exit must be unconditional on ANY client exit path (panic/signal/ctrl-c included — Drop guard or atexit), not just the launcher's happy path. Audit every exit from the held state.
- e2e: kill a client mid-held-handoff and assert the terminal restore sequence is emitted.

## Recovery (user-facing, for the next occurrence)
`stty sane; printf '\e[?1049l\e[?25h\e[<u'` (raw off, leave alt screen, show cursor, pop kitty flags).

## References
PR #67 (SWITCH_HANDOFF_PENDING, force_restore_host_terminal), #52 retry window, #38.

---

## Comments

### gerchowl — 2026-06-11T22:29:29Z

Shipped in PR #72: HeldRestoreGuard (panic hook + SIGINT/SIGTERM/SIGHUP + error-path force-restore), visible elapsed-seconds progress overlay on the held screen, and the genuine product fix — set_read_timeout errors on a half-closed fd no longer mask a live-handoff refusal as ConnectionFailed (the silent-retry-never-entered root cause).

