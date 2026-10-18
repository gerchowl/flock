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
file; corruption is reported rather than repaired. Read/write/sync failures remain
explicit errors. A failed directory sync may leave a complete object published;
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
reads at most a record-limit tail; the writer alone repairs torn tails.

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

Each page reads at most 32 KiB and emits at most one record, gap or barrier. A
record is capped at 16 MiB. `Progress` schedules another replay page after
yielding; it must not wait for a live-publication hint. `Pending` means no live
record is available yet. Live
continuation queues at most 16 records and 256 KiB of encoded envelopes. Overflow
clears its queue and yields a terminal Lagged cut; resubscribe from the last
applied cursor, never the skipped-to head. Consumer errors, corrupt/unterminated
records and producer failure are explicit. Committed facts remain replayable
before producer failure is reported. A normal producer end drains its queue.

Publication follows file sync and precedes notification delivery. Permission
open/closed audits therefore remain available even if cancellation drops an S4
notification. S4 still owns the sole Runtime event receiver; replay never races
it. Partial deltas remain live-only outside this durable source. WebSocket
framing and consumers are S7-min slice 4 (#1830), rather than a second endpoint
or reader here. Connection/subscription admission limits also belong to that
consumer; the bounds above are per cut. Retention/GC remains #1712/S16 and may not rewrite an active
cut's pinned file in place.

Retention must preserve a recoverable high-water. An empty file denotes a
genuinely empty stream, never pruning an established stream back to zero.
