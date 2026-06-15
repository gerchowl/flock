---
number: 95
title: "switch crash: next leg's ratatui::init() unwrap aborts via double panic (try_init + unfailable hook)"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-12T15:13:35Z
closed: 2026-06-12T17:01:48Z
url: https://github.com/gerchowl/herdr/issues/95
---

# switch crash: next leg's ratatui::init() unwrap aborts via double panic (try_init + unfailable hook)

## Live crash (ad42cd1, legs path), trace from herdr-2026-06-12-171137.ips

SIGABRT during a server switch. Faulting chain bottom-up: run_attach_legs -> auto_detect_launch -> run_client_with_mode -> **ratatui::init() -> unwrap_failed** (raw-mode/terminal IO failed on the mid-switch terminal) -> panic hook (ratatui set_panic_hook) -> restore + **std eprint PANICS** (stderr unwritable, EPIPE family) -> double panic -> abort. User sees a silent crash to shell.

## Fix
1. `ratatui::try_init()` at every client init site; failure becomes a ClientError -> the leg loop's existing failure rail (decide_next_leg -> relaunch previous + 'switch failed' notice, #67). No unwrap on terminal IO in the leg path.
2. Panic-hook hygiene: the restore hook's diagnostics must be best-effort (ignore write errors) so a hook can never escalate a panic into a message-less abort (#72's guard family).
3. Structural: the class dies under slots/#93 (no exit, no re-init) — this hardens the legs path that remains default until the flip.

## Sequencing
Rider on the #93 popup PR review round (same file, agent in flight there).

## References
#67 decide_next_leg, #72 HeldRestoreGuard, crash report herdr-2026-06-12-171137.ips.

---

## Comments

### gerchowl — 2026-06-12T17:01:48Z

Shipped as the rider on PR #99: ratatui::try_init (terminal IO failure exits cleanly instead of the double-panic SIGABRT), panic hook writes a best-effort diagnostic and contains a panicking chained hook.

