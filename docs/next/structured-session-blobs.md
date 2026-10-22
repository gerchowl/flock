# Structured-session blob storage

ADR-0036 §4 stores complete tool-result, attachment, image/document and thinking
message blocks strictly larger than `[sessions] blob_min_bytes` (default 2048)
under `<data dir>/blobs/<sha256>`. The persisted message replaces each block with
`{blob,size,preview}`; the live event retains its original content. Partial
`stream_event` deltas remain live-only and do not consume durable sequence numbers.
Consumers must check `Received.persisted` before advancing a durable cursor.

The shared content store stores bytes only, without content type or other metadata.
`put(reader,max_bytes)` hashes exactly the bytes it writes and enforces a per-call
cap independently of the persistence threshold. `get(hash)` verifies the digest
and distinguishes Missing from Corrupt; `stat(hash)` returns size without reading
content. References resolve to the encoded JSON block, preserving its values.
Only projected messages are reserialized; unmodified lines retain exact spelling.
Previews are lossy UTF-8, bounded to 80 characters, and are not complete content.

Directories require mode 0700 and files 0600, owned by the current user. Operations
pin the blob directory and reject symlinks, invalid addresses and unsafe entries.
Opening the namespace syncs its parent so a newly created directory is durable.
Puts publish a synced temporary file by same-directory atomic rename and sync the
directory, including deduplicated puts. A failed put removes only its own temporary
file. If an existing object's bytes fail hash verification, the next put of the
same content replaces it with the verified, synced temp file by atomic rename,
then syncs the directory. Only after that sync does the typed logging helper emit
`structured.blob.repaired` with `blob` (SHA-256) and `size` (bytes), never content.
Only corrupt-object repairs attempt an advisory lock on the pinned directory.
First writes never lock: racing Missing writers rename identical verified bytes
and emit no repair event. On contention, nonblocking acquisition sleeps on the
supplied monotonic clock: 1 ms initially, doubling thereafter, with each request
clipped to the remaining 20 ms budget. Normal contention is bounded by elapsed
time; an eight-attempt cap only guards against a clock/sleeper that does not
advance. `TimedOut` means the clock budget expired; `WouldBlock` means the attempt
guard fired. Requested sleeps are bounded; filesystem and OS scheduling latency
can exceed the budget. Unsupported locking, other lock errors, or exhaustion emit one
`structured.blob.repair_lock_unavailable` warning carrying the error kind and
blob hash, never content or OS error text. The put then proceeds without the lock,
so these diagnostic-coordination failures cannot lose content or stop the pump.
The current object is reverified before rename. Coordinated writers emit one
repair event; fallback can emit duplicate repair diagnostics. Direct filesystem
writes that do not take the lock are outside this coordination.
The transport continues streaming after a successful repair. A read still returns
Corrupt for damaged content; unsafe entries and read/write/sync failures remain
explicit errors. A crash before rename leaves the old entry and an owned temp
file, never a partially overwritten blob. Crash-temp sweeping is tracked in #1824.
A failed directory sync may leave a complete object published;
retrying the same content verifies it and syncs the directory again.

Projection applies only to complete assistant/user message content blocks, never
arbitrary tool inputs or unknown diagnostic payloads. Oversized or invalid-UTF-8
partial events retain the transport's bounded diagnostic summary for live delivery;
the transport recognizes their top-level type header in its bounded prefix.
This slice does not enable public structured-session start. Retention, shared roots
and concurrent GC are undecided in §4 and tracked in #1712; there is no GC or
public delete-by-hash API.

## Stored history and publication

New JSONL envelopes add optional `known: boolean` and `shape: string` fields.
The receive-time adapter supplies the classification and, for unknown events,
the original version-scoped ShapeKey. These additive fields do not change the
mesh version. Session envelope readers tolerate unknown fields. Replay never
re-runs the current parser: a known event remains known, and an unknown retains
its exact raw text and original shape even after a parser upgrade.

A record lacking classification, or an unknown record lacking its shape, yields
an explicit `provenance_missing` gap for that sequence. Later records continue.
Missing retained prefixes use `retained_prefix` on the same gap path. Neither
implies an entity snapshot. New records retain blob references unchanged.

`Workers::history` exposes the current registry generation's runtime-owned
publication source without placing files in pure AppState. It survives process
exit. For cold history, recover a quiescent file with `History::open_retained`
on an I/O worker and install it with `Workers::install_retained`; changed epochs,
active producers, mismatched agent/session namespaces and an existing source
refuse stale installation. The retained reader validates the private session
namespace and permissions before opening it. Recovery
reads a geometrically grown tail starting at one page, never an eagerly
allocated record-limit buffer; the writer alone repairs torn tails.

`History::waker` supplies coalesced readiness/publication/end hints. Enable its
notification future before checking readiness or paging, then recheck state after
every wake; a wake never supplies cursor authority. This also covers empty streams
whose asynchronous file setup finishes after the runtime-ready notification.

Subscribe atomically captures the head and registers continuation. An absent
cursor means retained beginning, a valid cursor resumes after it, and an ahead
cursor is refused. A cut emits bounded pages, exactly one captured-head barrier,
then durable live continuation. It pins the file descriptor and uses positional
reads, so replay cannot move the writer's append offset. Paging and cold recovery
perform filesystem I/O and belong off the heartbeat loop.

Each page reads at most 32 KiB and emits at most one record, gap or barrier.
Unread bytes stay buffered rather than being fetched again for each line;
cursor-only scans skip all complete buffered records within that page budget.
`Cut::read_bytes` accounts for bytes actually read, independently of cursors. A
record is capped at 16 MiB. `Progress` schedules another replay page after
yielding; it must not wait for a live-publication hint. `Pending` means no live
record is available yet. Live
continuation queues at most 16 records and 256 KiB of encoded envelopes. Overflow
clears its queue and yields a terminal Lagged cut on the next page, including
during unfinished replay. A single encoded record over 256 KiB lags every
subscriber, even one caught up before that publication. Resubscribe from the last
applied cursor, never the skipped-to head.

After applying a record or gap, retain `Cut::checkpoint` for that cursor and use
`History::resume(after, hint)` after lag. The opaque hint identifies the pinned
source and exact byte boundary, so resume reads only the suffix instead of
rescanning a long prefix. No hint is issued for an unlocated cursor-only scan;
there is no fabricated byte offset. A mismatched cursor, source or offset is refused.
Hints cannot cross source recovery/restart; a cursor-only cold subscription
scans with bounded pages. The consumer (#1830) owns its applied-cursor hint and
must not use an emitted cursor before application. Prefix gaps retain the next
record's start boundary; live records retain their fsynced end boundary.

Consumer errors, corrupt/unterminated records and producer failure are explicit. Committed facts remain replayable
before producer failure is reported. A normal producer end drains its queue.

Head capture and live registration share one mutable locked-book operation;
an owned registration is already live before file-cut construction can begin.
Publication follows file sync and precedes notification delivery. The projected
record is placed in one Arc; without queued subscribers, live delivery unwraps
it without cloning its raw payload. Permission
open/closed audits therefore remain available even if cancellation drops an S4
notification. S4 still owns the sole Runtime event receiver; replay never races
it. Partial deltas remain live-only outside this durable source. WebSocket
framing and consumers are S7-min slice 4 (#1830), rather than a second endpoint
or reader here. Connection/subscription admission limits also belong to that
consumer; the bounds above are per cut. Retention/GC remains #1712/S16 and may not rewrite an active
cut's pinned file in place.

Retention must preserve a recoverable high-water. An empty file denotes a
genuinely empty stream, never pruning an established stream back to zero.
