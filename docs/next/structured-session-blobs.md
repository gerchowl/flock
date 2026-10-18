# Structured-session blob storage

ADR-0036 §4 stores complete tool-result, attachment, image/document and thinking
message blocks strictly larger than `[sessions] blob_min_bytes` (default 2048)
under `<data dir>/blobs/<sha256>`. The persisted message replaces each block with
`{blob,size,preview}`; the live event retains its original content. Partial
`stream_event` deltas remain live-only and do not consume durable sequence numbers.
Consumers must check `Received.persisted` before advancing a durable cursor.

The shared content store stores bytes only, without content type or other metadata.
`put(reader,max_bytes)` hashes exactly the bytes it writes and enforces a per-call
cap independently of the persistence threshold. `get(hash,max_bytes)` stats the
opened descriptor first and refuses an oversized object before reading content.
Its bounded read also handles growth after stat, reading at most the cap plus one
detection byte. `TooLarge` takes precedence over digest verification; within the
cap, `get` verifies the digest and distinguishes Missing from Corrupt.
`stat(hash)` returns size without reading content. References resolve to the encoded
JSON block, preserving its values.
Only projected messages are reserialized; unmodified lines retain exact spelling.
Previews are lossy UTF-8, bounded to 80 characters, and are not complete content.

Directories require mode 0700 and files 0600, owned by the current user. Operations
pin the blob directory and reject symlinks, invalid addresses and unsafe entries.
Opening the namespace syncs its parent so a newly created directory is durable.
Puts publish a synced temporary file by same-directory atomic rename and sync the
directory, including deduplicated puts. A failed put removes only its own temporary
file; corruption is reported rather than repaired. Read/write/sync failures remain
explicit errors. A real OS permission denial is `Io`; owner, mode and entry-type
validation failures are `UnsafePath`. A failed directory sync may leave a complete
object published;
retrying the same content verifies it and syncs the directory again.

Projection applies only to complete assistant/user message content blocks, never
arbitrary tool inputs or unknown diagnostic payloads. Oversized or invalid-UTF-8
partial events retain the transport's bounded diagnostic summary for live delivery;
the transport recognizes their top-level type header in its bounded prefix.
This slice does not enable public structured-session start. Retention, shared roots
and concurrent GC are undecided in §4 and tracked in #1712; there is no GC or
public delete-by-hash API.

Crash-leftover `.tmp-*` files are kept. Opening the store does not sweep them:
another writer may still be publishing. Coordinated cleanup requires the
shared-writer/exclusive-cleanup fence deferred to #1712 (ADR-0036 §4), which owns
cleanup. This store does not implement that fence or automatic temp-file cleanup.
