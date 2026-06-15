---
number: 118
title: "fix(client): server switch no longer double-panics to SIGABRT"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-14T08:59:11Z
closed: 2026-06-14T11:24:44Z
merged: 2026-06-14T11:24:44Z
base: master
head: fix-server-switch
url: https://github.com/gerchowl/herdr/pull/118
---

# fix(client): server switch no longer double-panics to SIGABRT

## Symptom
Switching to another server crashed herdr — terminal garbled and the process exited (SIGABRT). Reproduced from the user's live setup; two macOS crash reports today (`herdr-2026-06-14-101107.ips`, `-100528.ips`) both show the same double-panic → `abort()`.

## Root cause (a double panic)
1. On the switch-exit path, `run_client_with_mode` runs `eprintln!("herdr: {err}")` with `err = "switching"`. During the seamless handoff stderr can be broken, and **`eprintln!` panics** when the write fails (`failed printing to stderr`).
2. The panic hook active at that moment is **ratatui's**, not herdr's protective one. `setup_terminal_with_capabilities` calls `ratatui::try_init()`, which installs ratatui's own hook *on top of* the protective hook `run_client_with_mode` set first. (ratatui's docs explicitly say to call `init` **before** installing your hook — we did it after.) Ratatui's hook `eprintln!`s while restoring → **second panic → `abort()`**, leaving the terminal in raw/alt-screen (garbled).

So the `#95` protective hook was dead in practice: every attach attempt's `try_init` clobbered it.

## Fix (two layers)
- **Re-assert herdr's protective hook after each `ratatui::try_init`** (in `setup_terminal_with_capabilities`). The process default hook is captured once into a `static`, so the per-attach re-assert never nests hooks unboundedly. Ratatui's restore-and-eprintln hook is dropped; ours restores first, writes a **non-fatal** diagnostic, and contains the chained hook in `catch_unwind` — a dead stderr can never abort us.
- **Replace panicking `eprintln!`/`eprint!`** on the client exit and handoff-retry paths with non-panicking `writeln!`/`write!` to `io::stderr()`, removing the first panic at its source.

## Testing
- `cargo build` ✅, full suite **2202 passed / 0 failed** ✅, `clippy -D warnings` ✅.
- Note: the double-panic→abort is process-global (panic hooks + broken fd 2), so it isn't cleanly unit-testable without adding test-only panic scaffolding to production code; verified via crash-report root-cause analysis + the full suite.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
