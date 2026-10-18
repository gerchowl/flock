# Structured-session blob storage

ADR-0036 §4 stores complete tool-result, attachment, image/document and thinking
message blocks strictly larger than `[sessions] blob_min_bytes` (default 2048)
under `<data dir>/blobs/<sha256>`. The persisted message replaces each block with
`{blob,size,preview}`; the live event retains its original content. Partial
`stream_event` deltas remain live-only and do not consume durable sequence numbers.
Consumers must check `Received.persisted` before advancing a durable cursor.

Registry-worker admission must supply the `[sessions]` storage settings explicitly
in `Launch.storage` (ADR-0036 §4). Each launch owns its policy snapshot; config
changes affect subsequent launches.

**Production admission tracking (#1923; owned by #1439 / #1843).** The first
production caller of `Workers::start` must pass the loaded `config.sessions`
(`StorageConfig`) through `Launch.storage`, with a test covering that production
admission path. The test must load a non-default `blob_min_bytes` and assert that
a complete content block whose encoded size lies between that threshold and
2048 bytes is persisted through a blob reference. The existing registry-worker
fixture covers the worker boundary, not production admission. A required storage
field prevents omission, but cannot prevent a caller supplying `Default::default()`.
This tracking item closes with #1843.

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
