# ADR 0007 — Peer I/O isolation: no blocking syscall or Drop on the client render loop

- Status: Accepted
- Date: 2026-08-01
- Issues: #176 (client froze on a slow/flaky peer — the motivating incident);
  builds on #65 (connection slots), #93/#139 (warm/switch dial), #101 (bridge)
- Decision owner: human; advised by three round-1 design reviews (Rust
  async/concurrency, TUI latency, distributed-systems failure-isolation) and two
  round-2 implementation reviews (concurrency correctness, acceptance/#65).

## Context

The client renders its TUI on a **single thread**: a `current_thread` tokio
runtime driving one `run_client_loop` task. Its event loop is a `select!` over a
few mpsc channels and a 100 ms timer. Because it is one task on one thread, any
blocking syscall on that thread halts *every* arm — the whole UI freezes.

A client holds multiple server connections as *slots* (#65): the local `home`
socket plus fleet peers reached over an ssh-stdio bridge (an `ssh` subprocess
whose stdio is bridged to a local `UnixStream`). Switching servers flips which
slot feeds the painter, in-process, without releasing the terminal.

#176 exposed the flaw: a single slow/flaky peer (`node-b-meatgrinder` via
ProxyJump; ssh exit 255; probes taking 2.7–3.8 s vs ~20–50 ms healthy) froze the
**entire** client — local tabs and healthy peers included — until the user
killed the terminal and relaunched. The server stayed healthy throughout; it was
purely a client-side freeze. Two mechanisms, both on the loop thread:

1. **Un-timed blocking writes.** Every keystroke/resize/subscription toggle went
   through `write_message` → `write_all` + `flush` on the active slot's stream.
   For a peer slot that stream is an ssh bridge's stdin; if the transport chokes,
   the write blocks indefinitely. There was zero `set_write_timeout` in the tree.
2. **Blocking `Drop`.** `SshStdioBridge::drop` joins an accept thread parked in
   `child.wait()`; three `handle_dead` sites dropped a dead peer's bridge inline
   on the loop thread, blocking it until ssh keepalive expired (~tens of seconds).

Peer flakiness (a sleeping hopper, ProxyJump latency, ssh 255, a paused remote)
is a **normal, expected** condition on a fleet — not exceptional. The client must
degrade the affected peer and keep rendering.

## Decision

Establish and enforce one invariant:

> **No blocking syscall and no blocking `Drop` may run on the client render-loop
> thread. All peer transport I/O lives behind a per-slot actor.**

Concretely (implemented in #176):

- **`SlotWriter` actor** — one dedicated writer thread per slot owns the
  transport write half and any client-built `SshStdioBridge`, fed by a bounded
  `SyncSender`. The loop only ever `try_send`s; it never touches a peer socket.
  Writes are offset-tracked under a `set_write_timeout`, so a transient stall
  delays delivery without corrupting framing; a sustained stall fills the bounded
  queue and the loop demotes the slot. Because the writer thread owns the bridge,
  its teardown (the blocking join / ssh child reap) runs there, off the loop.
- **Bounded bridge teardown** — `SshStdioBridge::drop` SIGKILLs the in-flight
  ssh child so `child.wait()` returns promptly instead of parking on ssh
  keepalive.
- **Circuit breaker** — an escalating redial backoff (15 s → 5 min cap) keyed on
  consecutive dial failures / transport deaths, in the pure `SlotRegistry`,
  suppresses the 2 s warm-sweep from re-dialing a flaky peer every tick.
- **Graceful active-slot degrade** — when the *active* slot's writer fails, the
  loop demotes the peer and flips to the always-warm home slot instead of
  exiting; only home-dying / slots-disabled is fatal.
- **Heartbeat** — a coarse `client.tick` trace makes a freeze a bounded log gap
  rather than silence (a frozen loop cannot log).

## Alternatives considered

- **Per-write `set_write_timeout` only, no actor.** Cheapest (the house pattern),
  and it stops the keystroke freeze. Rejected as the *sole* fix: it leaves the
  blocking-`Drop` teardown and the flip-time subscription writes on the loop, and
  a timeout on a partial `write_all` desyncs framing. The actor subsumes it and
  removes the whole class of loop-thread blocking, not one call site.
- **Rewrite the write path onto async `tokio::net` + `timeout`.** Would plumb
  through `write_message` (a sync `Write` generic) and every call site, and teach
  `SshStdioBridge` to hand out an async half — a large change for what a
  per-slot thread + bounded channel achieves in isolation. The loop is
  `current_thread` with no reactor doing useful work anyway.
- **Owned-`Child` teardown polled via `try_wait`** (fully closes the pid-reuse
  window in the SIGKILL). Rejected for now: it replaces the efficient blocking
  `child.wait()` on the live session-carrying data path with a poll loop, trading
  a real steady-state cost for a vanishingly-narrow race. Documented in
  `SshStdioBridge::drop` as the exact hardening if the race ever matters.

## Consequences

- One thread per live slot (bounded by the `[slots] max` cap). Cheap; the writer
  parks on `recv` when idle.
- The active write path is now asynchronous. Within a slot, ordering is preserved
  (one `SyncSender` → one `writer_pump` → FIFO), so Resume-then-Resize on a flip
  still holds. Cross-slot ordering was never guaranteed and still isn't.
- On demotion, queued-but-undelivered messages for the dead peer are dropped;
  the server session survives and geometry is re-asserted on the fallback slot.
- The pure `SlotRegistry` state machine is unchanged, so #65 in-process
  switch semantics and their unit tests carry over verbatim.
- New surface to keep honest: any future code that writes to a slot must go
  through `SlotWriter::send`, never a raw stream — the invariant is a convention,
  not yet a type-level guarantee.

## Server git-refresh worker (#1722)

The same isolation principle applies to the sidebar git refresh: filesystem
repository discovery and git probes run in one owned worker, outside the app
loop. The #262/#265 viewer gate remains a dispatch gate, not cancellation of an
already owned round. A worker panic returns a typed failure outcome; failure to
start the worker is also counted. Neither is a successful refresh.

The existing per-command ceiling is **10 seconds**, clamped to the round's
**60-second** deadline. This covers `rev-list`, reftable `symbolic-ref` and
`rev-parse`; command groups are killed and reaped by the common process runner.
Nonzero `rev-list`, command I/O and invalid UTF-8/counts fail that repository's
probe and never produce a cache entry for it. Other repositories keep refreshing
within the remaining round budget. `rev-parse --verify --quiet` exit 1 means
optional absence; fatal errors, including exit 128, remain `git_exit`. Quiet
verification also hides malformed loose refs, so absence is confirmed by a bounded
`show-ref` database check (exit 1 for an empty database). Neither command parses
stderr. An absent revision also requires a symbolic or resolvable detached HEAD:
a damaged reftable stack can otherwise appear to be an empty database. Detached
symbolic refs retain their exit-1 absence semantics.

A missing fingerprint keeps the earlier branch-only fallback, with the same
round/command bounds on reftable discovery. Symbolic reftable HEAD outside
`refs/heads/` has no branch fingerprint, rather than being malformed output:
the branch-only fallback may still show its shortened symbolic target. Such
snapshots are not cached and carry no ahead/behind counts.

The app watchdog stays scheduled while a round is in flight, including without
viewers. At 60 seconds it records `round_stalled` once and retains worker
ownership: native filesystem calls cannot be forcibly cancelled, so dispatching
a replacement could accumulate blocked threads. The existing worker's eventual
completion permits recovery. If the filesystem never returns, git refresh stays
broken and in flight, with its rising age visible, while other app work continues.
The operator should restore access to the checkout's filesystem; this change
does not restart the server or terminate an unconfirmed filesystem operation.

Summary `last_error` is a closed code: `timeout` (command/round deadline),
`git_io` (process I/O), `git_exit` (unsuccessful command), `git_output`
(malformed output), `worker_panic`, `worker_spawn`, or `round_stalled`.
No command output, path, host or panic payload is exposed in that field. Each
round-level failure increments the global streak at most once, even when its
watchdog fires before an error completion. A round with at least one healthy
repository resets that streak; a round where every probe fails still counts as
failed and never stamps success. Each failed repository has a separate
consecutive-probe count, reset only by its own successful snapshot. Completed
partial rounds are `degraded`, not permanently `broken` because one checkout
fails repeatedly. Worker panics, spawn errors and stalled rounds retain their
worker-level failure semantics.

Summary `repositories` is an additive optional list of failed repositories,
with ASCII-escaped checkout basenames, workspace IDs, counts and typed error codes. Basenames
can collide; workspace IDs disambiguate them. No absolute paths enter these
rows. Older mesh-v5 JSON summary readers ignore the extra field. Failed
workspaces retain their last values with `[git stale]` in sidebar labels until
a successful snapshot for that workspace's current cwd. Results for removed or
moved workspace/cwd pairs do not mutate their current health. Operators should
repair the named checkout's repository or filesystem and let the next scheduled
round confirm recovery; healthy checkouts continue independently.
