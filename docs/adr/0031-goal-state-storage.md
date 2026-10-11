# ADR 0031 — Goal state is a fold over the durable event log, held in memory and folded once per start in the existing startup pass

- Status: Proposed
- Date: 2026-10-11
- Issues: #1033 (G0), #573 (goal mode, storage spike), #1001 (slice table)
- Builds on: ADR-0005 (durable event log), ADR-0027 (re-issued edges),
  ADR-0030 §1 (the event fold as the recommendation to test here).
- Decision owner: operator.
- Implemented: the benchmark only (`src/api/event_hub/goal_storage_bench.rs`,
  an ignored test). No goal state exists yet.

## Context

ADR-0030 §1 records goal state as durable `Goal*` events folded into a live
record, and leaves open whether the fold is cheap enough on the Stop-hook path
or a SQLite store is needed. The Stop hook asks the server at every turn
boundary of an armed agent, inside a 7.5 s adapter deadline that also has to
hold a 3.5 s judge call (ADR-0030 §2). Two costs matter there: `goal.status`
(read on every stop) and a verdict write (one per judged stop).

Neither candidate adds a dependency. `rusqlite` (bundled) is already linked
for opencode's DB and the mesh mail store (`src/mesh/store.rs`, WAL,
`synchronous=FULL`), so SQLite costs no build time or binary size it does not
already cost.

## Measurements

Method: `cargo test --release --bin flk goal_storage_bench -- --ignored
--nocapture`, on the operator's Mac Studio (Apple M3 Ultra, macOS 26.4),
2026-10-11. The test writes a full log, four rotated files plus the active one
(`MAX_ROTATED = 4`): filler lines of about 200 bytes (the live log averages
155) and one goal's 400 verdicts (about 400 bytes per line) spread evenly
across all five. It then measures both stores in the same temp dir. Two sizes:
100,000 events per file (the event-count rotation bound) and 164,000 (the 32
MiB byte bound, `FLOCK_BENCH_EVENTS_PER_FILE=164000`). OS page cache not
flushed, so "cold" means after a process restart, not after a reboot. The
socket round trip to the server is the same for both stores and is not
included.

| Operation | Fold, 97 MiB / 500k events | Fold, 159 MiB / 820k events | SQLite (independent of log size) |
| --- | --- | --- | --- |
| `goal.status`, in-memory record (lock, clone, serialize) | p50 0.29 µs, p99 0.46 µs | p50 0.29 µs, p99 0.42 µs | warm connection: p50 1.7 µs, p99 2.3 µs |
| `goal.status`, re-folded from disk per call | p50 346 ms, p99 352 ms | p50 560 ms, p99 644 ms | — |
| Verdict write: fold = one append + `sync_data`; SQLite = insert + upsert in one tx | p50 4.3 ms, p99 8.2 ms | p50 4.3 ms, p99 6.1 ms | mesh pragmas: p50 64 µs, p99 202 µs; `fullfsync=ON`: p50 4.6 ms, p99 7.9 ms |
| Cold start: `EventHub::with_persistence`, paid today | p50 281 ms | p50 465 ms | — |
| Cold start: one more full-log read + fold | p50 345 ms | p50 550 ms | open + first status: p50 0.35 ms, p99 0.57 ms |
| Cold start: the fold alone, in a pass that already read the log | p50 29 ms | p50 47 ms | — |

Across repeated runs (four at 97 MiB, two at 159 MiB) the shape held and the
absolute values moved by up to 1.8× (hub open 281–818 ms, fold write p99 6–12
ms); the table shows the last run of each. The operator's live log holds 131k
events (19.4 MiB) over 71 days, so the 97 MiB case is a log that has rotated
several times, and the 159 MiB case is the most the five files can hold.

The two verdict writes cost the same at the same durability. Rust's
`sync_data` is `F_FULLFSYNC` on macOS, and SQLite's default `fsync` is not, so
the 64 µs SQLite write is buying less durability, not doing less work.
SQLite with `fullfsync=ON` costs the same 4–5 ms as the fold's append.

## Decision

1. **Goal state is the event fold** ADR-0030 §1 recommended. Every state change
   is one appended `Goal*` event. A verdict write is about 4 ms p50 and 12 ms
   p99 at worst: under 0.2 % of the 7.5 s adapter deadline, and the same as a
   SQLite write at the same durability.
2. **The live record is held in memory and never re-folded on a read.**
   `goal.status` is a map lookup, under 1 µs. Re-folding from disk per call
   costs 0.35–0.65 s and is ruled out on the hot path. A read never touches
   `persisted_events_after`.
3. **The fold runs once per server start, inside the pass that already reads
   the log.** Startup already reads the full log at least five times (hub open,
   issue guard, mailboxes, notifications, delegation re-issue), up to about
   0.5 s each at the largest log. A goal fold sharing the delegation pass costs
   29–47 ms. A pass of its own would add 0.35–0.55 s and is ruled out.
4. **Open goals are re-issued on start**, as ADR-0027 does for open edges, so
   rotation never drops a goal's current record. The re-issued event carries
   the record (goal text, status, counters, last verdict), not the verdict
   history.
5. **SQLite is not used for goal state.** It would be faster on paper only at
   weaker durability, and it would add a second store, a schema migration
   story and a second crash-consistency rule next to the log, which already
   answers #573's crash-safety, versioning and audit questions.

## Consequences

- **Verdict history lasts as long as the log does.** Retention counts events,
  not time: five files of 100k events or 32 MiB each. At the operator's current
  rate that is months, but a busy host shortens it. G3c's `flk goal decisions`
  export reads the cold path (`persisted_events_after`) and can only return
  what has not rotated out. If labels must outlive rotation, G3c exports them
  on `GoalMet` / `GoalDisarmed`, or revisits this decision with its own
  retention number.
- **A verdict write holds the event hub lock across its fsync**, as every
  persisted event already does. One more 4 ms write per judged stop does not
  change that contention class, but adapters must not wait on the write to
  return their verdict: the server answers from the in-memory record.
- **Slices.**
  - **G1 (#1034):** add the `Goal*` kinds as persisted events (ADR-0028 §3a
    reader check), the in-memory record keyed by `AgentId`, the fold beside
    `reissue_open_delegations` in one log read, and the re-issue of open goals.
    The acceptance probe (restart, pane move, duplicated event) tests exactly
    this fold.
  - **G2 (#1035), G5 (#884):** rungs and the pre-compaction `GoalStatusSet`
    handshake read the in-memory record only.
  - **G3a (#885), G3b (#1036):** the breaker, `degraded` state and
    `(agent, turn_cursor)` cache are server memory, not stored state; the
    record keeps only what must survive a restart.
  - **G3c (#886):** one `GoalVerdict` event per judged stop at the write cost
    measured here; the decisions export is a cold read bounded by retention.
  - **G6 (#965):** SessionStart re-inject reads the in-memory record; goal
    events reach digests and notifications through the existing event stream.
- **The benchmark stays in the tree** as an ignored test, so a later change to
  the log format, rotation bounds or startup passes can be re-measured with
  the same command.
