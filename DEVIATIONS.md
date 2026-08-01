# Phase 4 (check-runner CORE) — deviations from the accepted design

## Commit split: 4 → 2

The accepted design outlines four commits (config, executor, runner, App
wiring). The dead-code trap (called out in the design itself: "code
introduced in one commit must be consumed in the same commit or the
clippy gate fails; if needed, merge commits rather than adding #[allow]")
forced commits 2+3+4 to land as ONE commit:

- `crate::checks::script::run_script` and `Outcome` are only consumed by
  the runner (commit 3) and the App wiring (commit 4). Landing commit 2
  alone would leave the executor items unused under
  `cargo clippy --all-targets -- -D warnings`, which polices the bin
  target where `cfg(test)` is off.
- Similarly, `CheckRunner::next_runnable` returns a `RunnableCheck` whose
  `check: ScriptCheck` field is only consumed by the App tick's
  `std::thread::spawn` dispatch (commit 4).

Merging preserves the "each commit independently green" invariant. The
resulting two commits are:

1. `feat(config): checks section skeleton` — [checks] config, drift
   guard, EventHub MAX_EVENTS bump.
2. `feat(checks): script executor, runner state machine, and App tick
   wiring` — the combined executor + runner + App wiring (originally
   commits 2, 3, 4).

## `CheckRunner::ack` retains `#[allow(dead_code)]`

The `ack` method exists per the design (§8.1 + §8.4) but has no product
call site in phase-4-CORE — the interactive ack path (UI action or API
verb) lands in a later phase-4 commit. Rather than delay landing the
mechanism, the method carries a `#[allow(dead_code, reason = …)]` with a
scoped rationale (see `src/checks/runner.rs`). This is one of the only
`#[allow]`s introduced — the design guidance says "prefer merge over
allow", but here the merge partner (the interactive ack) is outside the
CORE scope.

## `ActionSpec::Event` dispatch is a no-op

Commit 4 routes `ActionSpec::Notify` through the existing
`Method::NotificationShow` handler (so `[ui.toast]` delivery policy
applies uniformly). `ActionSpec::Event { label }` intentionally does NOT
enrich the durable `CheckFired` event with the label yet — that event is
already emitted before the action dispatch, and adding the label needs a
schema-level `label: Option<String>` field on `EventData::CheckFired`. A
follow-up in the phase-4 tail should either enrich `CheckFired` or add a
distinct `CheckAction { label }` event.

## Heartbeat only arms when scripts exist

Because the built-in checks (blocked-alert / hibernation / issue-guard)
aren't executed by the runner yet, arming the heartbeat unconditionally
would wake a defaulted server for a no-op every `heartbeat_secs` and
would break an existing `next_headless_loop_deadline_with_git_refresh`
test that asserts `None` on a workspace-less app. Gate:
`config.checks.enable && !config.checks.scripts.is_empty()`. Once the
built-ins land, widen the gate.
