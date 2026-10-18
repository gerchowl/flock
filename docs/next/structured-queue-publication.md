# Queue replay and publication

ADR-0036 sections 4 and 5 use the worker's single journal owner for replay and
live publication. `Provider` and `Subscription` are internal, associated-type
interfaces: queue snapshots retain their own gap semantics, while the session
history provider (#1829) retains session-envelope and history-loss semantics.
Neither interface adds a Runtime receiver or another journal writer.

An active subscription is a bounded request to the I/O owner, not a completed
snapshot. The owner persists the first checkpoint before serving a snapshot.
The checkpoint follows the existing fsync-temp, rename, fsync-directory protocol.
Capture and continuation registration run under one publication lock, including
the checkpoint callback. No append/publication can fall between those steps.

Pages contain a replacement gap with `snapshot_to`, original records, typed
status and exactly one `synced` barrier before live changes. Empty checkpoints
still have a gap and high-water barrier. An ahead cursor is `invalid_request`.
Bootstrap/storage failure closes with its typed refusal and never claims a
successful snapshot or barrier. Unexpected producer loss closes explicitly.
A real terminal worker retains its final cut for late subscriptions.

The owner already recovers decoded records. Replay captures an immutable cut;
paging reads zero filesystem bytes and emits at most one bounded journal record
per call. Resume binary-searches the retained sequence index and shares the immutable
cut, without copying its prefix. It never reopens or scans the JSONL file. Queue record size is
bounded by the journal's 2 MiB limit; snapshot capture retains the decoded cut,
so this interface does not promise constant total snapshot memory.

Each continuation has a four-cut, 256 KiB bound, separate from its initial replay
cut. Overflow closes explicitly as `lagged`; recover from the last applied cursor,
not from the reported overflow head. Enable the readiness future before paging
and recheck after waking. Lock contention returns readiness/pending rather than
waiting for filesystem work on the server loop. There is no queue deadline.

Cold recovery and its I/O-worker/generation-fenced installation are #1956 part 2.
Until that lands, a registered generation without a retained live source returns
`queue_mode_unsupported`, never an empty successful subscription. A stale or
unknown generation returns `agent_not_found`. This limit does not affect the
retained source of a worker that has exited in the current process. Global
lifecycle facts are #1956 part 3; producer routing/acknowledgements remain #1957
and #1958.
