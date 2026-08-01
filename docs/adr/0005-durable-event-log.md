# ADR 0005 — Durable event log: per-session JSONL mirror of the event hub

- Status: Accepted
- Date: 2026-08-01
- Issues: #175 (O1 — lineage/audit/telemetry substrate; gate G2), consumed by
  US-4 (`flk lineage`), future consumers: fork/message telemetry (§7), check
  runner audit, morning digest.
- Decision owner: operator (gate waiver + phase order 2026-08-01); design per
  the epic's adversarial-review corrections (P4, P5, §8.2, §8.4, §9).

## Context

The in-memory event hub (512-entry ring, monotonic sequences,
`events_after(seq)`) is flock's only event log, and it dies with the server.
Epic #175 needs events that survive restarts: fork lineage ("why does this
worktree exist"), audit, and the fork-vs-message telemetry that decides what
Phase 3+ keeps. §9 explicitly rules out adopting a message broker; P5 rules
out a second event system — the hub itself must become durable.

## Decision

- **Format**: append-only JSONL, one `{seq, ts_ms, envelope}` object per
  line. A torn (newline-less) tail from a crash mid-write is dropped on load
  and truncated before the next append; corruption anywhere earlier rejects
  the whole file loudly rather than skipping lines — a silent hole would
  break the §8.2 "prefix-consistent, no gaps" invariant.
- **Location**: `session::data_dir()/event-log.jsonl`, sibling to
  `session.json` — per-session, matching what "server restart" means in G2.
- **What persists**: every `EventKind` except `PaneOutputChanged` (fires per
  terminal revision; drowns an audit log and carries no audit signal). The
  classification is an exhaustive `EventKind::is_persisted` match so a new
  kind cannot dodge the decision.
- **Durability**: `sync_data` per event. Event rate (sans output events) is
  tens/second at worst; if the §8.9 throughput budget ever trips, downgrade
  to a bounded-interval batched fsync.
- **Rotation**: rotate the active file at 32 MB or 100 000 events; keep 4
  rotated files (`event-log.1.jsonl` newest … `.4` oldest). Reads span
  rotated + active transparently.
- **Failure posture (P4)**: a corrupt *active* file is quarantined by rename
  to `event-log.corrupt.jsonl` (content preserved, never deleted) so future
  appends stay readable; corrupt rotated files are skipped in place. If the
  log cannot be opened at all, the hub degrades to memory-only and logs it.
- **Sequences across restarts**: boot seeds `next_sequence` from the highest
  persisted sequence, so live subscribers (who subscribe at the current
  sequence and only ever read forward) keep the no-gaps guarantee by
  construction. Non-persisted kinds consume sequences but never hit disk.
- **History access**: `persisted_events_after(seq)` streams from disk for
  historical consumers (`agent.lineage`); the ring stays the hot path for
  live subscribers. Replaying history to *new* subscribers is a non-goal
  (that is broker territory, §9).

## Known gap — live-handoff overlap

During a `live-handoff`, the outgoing and incoming server processes briefly
hold the same active log file; a rotation in that window would leave the
loser appending to the rotated inode with stale byte accounting. The window
is seconds long and rotation needs 32 MB/100k events, so the practical risk
is negligible for now — but the per-session isolation story above assumes a
single writer. TODO (pre check-runner, which raises event volume): defer
opening the incoming server's persistent log until the handoff completes.

## Consequences

- `flk lineage <target>` reconstructs fork ancestry across restarts from
  `agent_forked` events alone (US-4 acceptance), keyed on identities that
  outlive panes: worktree path, branch, and snapshot-stable public pane ids.
- The log is the landing zone for O2 message-side telemetry (M1) and check
  runner audit events (C1) without further storage design.
- Retention is bounded (~128 MB + corrupt sidecar worst case per session);
  no compaction pass is needed at this scale.
