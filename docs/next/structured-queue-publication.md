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
still have a gap and high-water barrier. An ahead cursor is `cursor_ahead`,
checked before bootstrap can advance the head. A checkpoint's covered range
identifies it independently of a particular worker's requested cursor, so the
first post-bootstrap update does not repeat the snapshot.
Bootstrap/storage failure closes with its typed refusal and never claims a
successful snapshot or barrier. Unexpected producer loss closes explicitly.
A real terminal worker retains its final cut for late subscriptions.

The owner already recovers decoded records. Replay captures an immutable cut;
paging reads zero filesystem bytes and emits at most one bounded journal record
per call. Resume binary-searches the retained sequence index and shares the
immutable cut, without copying its prefix. It never reopens or scans the JSONL
file. Status-only updates retain the same cut. Replacements share their new
cut directly. An actual append while a reader retains the old cut uses
copy-on-write and copies the retained decoded prefix once for that shared cut,
so publication cost and peak memory can grow with snapshot size. Subsequent
appends reuse the owner's unshared cut until another reader captures it.
Queue record size is bounded by the journal's 2 MiB limit. This interface does
not promise constant total snapshot memory or constant-time append publication.

Each continuation has a four-cut, 256 KiB bound, separate from its initial replay
cut. Overflow closes explicitly as `lagged` and discards any unread replay
packet along with queued continuation. Recover from the last applied cursor,
not from the reported overflow head. Enable the readiness future before paging
and recheck after waking. Reader pages use their own packet queues and never
take the shared capture/book lock, avoiding reader-to-reader pending stalls.
Subscription admission may return `session_busy` on book contention, without
waiting for filesystem work on the server loop. Subscription requests are
serviced even when the worker's completion channel is full. There is no queue
deadline. Final and bootstrap status retain the last operation error.

Cold sources return `queue_source_cold` (`retry: later`) until the registry
owner installs a completed recovery. `Workers::recover_queue` admits at most one
I/O loader per worker owner. It opens an existing private journal and lease;
a missing namespace never creates an empty successful queue. An existing
checkpoint retains its original sequences and bytes. A first uncheckpointed
subscription uses the same durable bootstrap as a live owner.

All opening, fsync, supported torn-tail repair and bootstrap run on that I/O
worker. The completion owns no journal lease or file, and polling/dropping the
handle never joins the thread on the server loop. Cancelling a consumer does
not interrupt filesystem operations; the slot stays occupied until loading
ends. The same agent's launch is `session_busy` while that slot is active.
Unrelated agents can still launch. Consumer request/result retention limits
belong to the subscription adapter (#1830); recovered snapshot memory remains
proportional to the journal, as for the live provider.

Installation rechecks the registry generation, namespace and active or
unconfirmed owner. A live producer or the first installed recovery wins
canonically; later completions cannot replace it. Terminal recovered sources
serve their immutable replay and `synced`, then `End`. A later live generation
uses its own fenced owner and continues at the persisted queue sequence.
Retired sources are removed from the owner cache during registry polling.

A missing queue namespace is `queue_not_found` (`retry: never`): resolve or
create the queue before requesting recovery. It performs no creation. An
existing journal may receive its durable first-subscription bootstrap.
Lease contention is `session_busy`. Other I/O/opening failures are
`queue_recovery_failed` with `retry: after_inspecting`: recovery
may have repaired a tail or completed a bootstrap before the failure. No
mutation is automatically retried. Worker loss/panic is
`structured_worker_panicked`. The configured rewrite policy re-derives
`queue_journal_newer`/`queue_journal_corrupt` refusals without repairing or
rewriting read-only bytes; these sources refuse rather than claim a snapshot.
A stale or unknown registry generation remains `agent_not_found`.

The app consumes the single owner's committed transition completions and the
same in-process provider used by S7-min. It appends `queue_item_state` to the
global EventHub after queue durability, retaining typed producer, item/batch
identity and the original queue cursor. It never derives transitions from a
replacement snapshot: bootstrap and rewrite can erase intermediate states.
Only the seven ADR §3 states participate in this global vocabulary; future
states and queue-only formation failures remain in per-queue status/records.

Each app pass drains at most four completions and 64 provider pages per agent.
Shutdown may abandon notification cuts under backpressure, but committed
transition facts remain owned by the terminal client until drained, in queue
order even when a later notification finds channel capacity. Completed handles
outlive registry-row removal until their committed facts drain; empty
notifications do not retain a removed worker. Server shutdown and handoff drain
the fixed channel capacity plus its one peek and terminal tail before
relinquishing the global log. Provider
status preserves reservation and S4/S6 gate Pending, read-only errors and explicit
lagged closure; it does
not auto-retry mutations or start cold recovery. A new generation discards the
previous provider observation. Operator admission acknowledgements use this same
completion consumer (#1957); authenticated message custody remains #1958, and
the external subscription adapter remains #1830.

The queue and global log are separate durability domains: facts are ordered
after their queue commit, not in a shared transaction. Global append failures
use EventHub's existing logging/retention policy. Queue replay remains the
source of item state; loading/replaying a checkpoint never invents historical
state transitions or new global facts.
