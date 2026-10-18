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
Publication uses an advisory lock on the pinned directory and rechecks the current
object under that lock before rename. Racing puts emit one repair event: only the
writer that actually replaces corrupt bytes reports it. Direct filesystem writes
that do not take the lock are outside this coordination.
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
