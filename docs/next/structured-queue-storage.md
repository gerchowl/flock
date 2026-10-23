# Structured input queue storage

ADR-0036 §3/§4's storage core is available to dedicated workers as
`structured::queue::Journal`. Scheduling and harness lifecycle are #1802;
operator/CLI/message producers, the configured rewrite threshold and server
publication are #1803. This storage slice does not expose a new send route.

`Journal::open(data, agent)` acquires an exclusive OS lock on the stable
`sessions/<encoded-agent>/.queue.lock` file. It keeps that lease across atomic replacement
of `queue.jsonl`. Drop the old journal before handing ownership to a successor;
a cooperating second writer receives `WouldBlock` while the old lease exists. Agent
path components permit ASCII letters of either case, digits, `-` and `_`.
Each uppercase letter is encoded as `=` followed by its lowercase form; every
other character stays unchanged. Thus `Foo` uses `=foo`, while `foo` uses `foo`.
All encoded components are lowercase and `=` is outside the input charset,
so the mapping is injective even on case-insensitive filesystems. The original
agent ID stays unchanged in memory and in the journal identity header. Namespace directories
are private and files are owner-only, with existing symlinks/nonregular files
refused. The data root itself may be an alias such as a canonicalized temp root.

Every journal begins with a synced
`{"kind":"queue_identity","agent_id":"...","format_version":2}` header containing the original, case-preserved ID. Open verifies it against the
requested agent and refuses missing or mismatched first identity headers.
A repeated identity header opens read-only as structural corruption. An unterminated first line with no records following it is rewritten
and synced: identity creation only writes into an empty file, so no records
can be lost. Checkpoints preserve it. A new empty file receives its identity before
open returns; this header consumes no sequence and is not a replay event. Earlier
unreleased prototypes without this header are refused rather than adopted under
an unverified identity.

The journal persists dense per-agent sequences independently of the global
event log. An `upsert` record contains the full typed item plus its final
zero-based position; a `remove` record removes retained item state. The same
upserts form checkpoint snapshots. Producer, kind, state, delivery and priority
are typed. Optional metadata retains anchors, gate decisions, edit leases and
future item fields without interpreting them; it cannot override typed fields.
The scheduler supplies and interprets that metadata. These are durable storage
types, not a substitute for the frontend wire contract.

`append` validates and serializes before I/O, writes and syncs before folding the
change into memory, and only then returns the committed record. No observation
or server effect may precede that return. Complete lines are limited to 2 MiB,
including their newline. Invalid positions, oversized records and exhausted
sequences fail without spending a cursor. A write/sync/rename failure poisons the
handle: its live state stays at the last published cut and replay or further
mutations fail. Drop and reopen to discover the actual durable outcome; never
blindly retry an uncertain append. A successful write followed by failed sync
can be present after recovery even though the call did not acknowledge it. Reopen
unconditionally syncs the journal before publishing any recovered sequence or
state; a failed recovery sync fails open. Thus complete unacknowledged lines can
never be served as committed while remaining vulnerable to cursor reuse after
a power cut.

`items()` exposes the pure materialized queue. `bytes()` lets its owning worker
check the configured rewrite threshold. `checkpoint()` and appends both require
exclusive mutable ownership. For last committed sequence H and N retained items,
the replacement contains the identity header, then a checkpoint header
`{"kind":"queue_checkpoint","before":H,"high_water":H+N}` followed by
snapshot records H+1 through H+N. `before` also lets recovery detect an incomplete
snapshot. The header consumes no sequence and is never a queue event. The worker
syncs the temporary file, renames it, syncs the directory, then publishes the new
cut. Open removes stale `.queue-*.next` files left by failed/crashed replacements
under the exclusive lease and syncs the directory; they are not committed state. A checkpoint after removing all items
still records H, so restart cannot reuse a cursor.

`replay(after)` returns an owned, immutable cut and its `through` barrier. A
cursor at or before the checkpoint's H receives a gap with `from:after+1`, `to:H`
and `snapshot_to:H+N`, followed by the snapshot and later records. For active
subscribers, capture `replay(last_applied_cursor)` immediately after checkpointing
under the same worker serialization, so each gap starts at that client's cursor
plus one. `checkpoint()` itself returns the cut for a subscriber already at H.
Use each cut's barrier for `synced`. An empty snapshot clears the client's entities
immediately; no event is needed to complete it. An initial empty queue returns
`from:1,to:0,snapshot_to:0`. A cursor ahead of the journal is refused. No cut
shares a mutable record buffer with future appends or rewrites.

## Format versions and downgrade refusal

The ADR-0036 §4 owner decision in #1820 adds a journal version stamp. The
current writer supports version 2. A missing `format_version` means version 1;
opening a v1 journal does not stamp it. Its next checkpoint writes version 2.
Before appending to an older format, the writer checkpoints it into its own
format, so no newer-format record can enter a journal with an older stamp.
Sequence and encoded-size validation include the snapshot's sequences and happen
before this upgrade. Callers publish `replay(previous_cursor)` after an append,
so an upgrade's gap/snapshot is published together with the new record. An
older journal containing opaque records cannot be checkpointed or upgraded
automatically. It opens read-only as `QueueJournalCorrupt`, with reason
`unknown_data_prevents_checkpoint`, before admission or runtime recovery can
attempt a write. It never reports the uncertain `input_write_unknown` code for
this deterministic refusal.

**Bump the format version for every new record kind, new top-level field or
new enum variant name.** A writer may append or checkpoint only when the
journal version is no greater than its supported version. Checkpoints always
write the writer's version. Version 2 introduces the identity-header stamp;
it does not introduce a queue event or consume a sequence. The initial identity
bootstrap keeps `kind`, `agent_id` and `format_version` names/types stable across
versions so every reader can check identity and downgrade safety first.

A newer-stamped journal opens read-only under the same exclusive lease. Load
still succeeds: unknown or invalid records are logged, skipped when folding
known state, and retained in the original file. That materialized state is for
inspection only and may be incomplete. Open never truncates a newer journal's
tail or cleans up its replacement files. The bytes remain unchanged, including
malformed complete records and torn tails. Read/replay/status remain available.
Every mutation returns the registered `queue_journal_newer` refusal with typed
`found` and `supported` versions, including admission, idempotent enqueue,
replacement/take-back, batch formation, lifecycle transitions and checkpoint.
The scheduler exposes the versions as the queue's blocking status. This is an
`after_handoff` refusal: use a compatible writer, never retry the mutation in a
loop. The read-only status has no batch or reservation time (`since_ms: 0`).
Server publication remains the separate #1803 slice.

This forbids serving a valid newer journal with an older writer. Unknown
records cannot be passed through a checkpoint: their interleaving semantics
cannot be reconstructed by an older reader. A present stamp whose value is not
a positive integer opens read-only as typed `QueueJournalCorrupt`; it never
invents a writable version. Identity mismatches remain namespace violations
that refuse open.

## Unknown data, corruption and torn tails

The ADR-0036 §4 owner decisions in #1820 and #1922 separate three cases:

1. **Unknown data are forward compatibility.** Unknown kinds are logged and
   skipped when folding state; their cursors, parsed replay values and original
   file bytes are retained. Known records with unknown top-level fields or enum
   names retain their known projection and queue position for inspection, as before. Unknown
   enum names remain `Unknown(raw)`; uninterpreted items cannot be scheduled.
   Here `raw` is a parsed `serde_json::Value`, not an exact byte representation:
   serialization normalizes whitespace and escapes, and object keys come back
   sorted without `preserve_order`. Original complete records keep their exact
   bytes because unknown data prevent checkpoint replacement, as required by
   [#1820](https://github.com/gerchowl/flock/issues/1820) and
   [#1922](https://github.com/gerchowl/flock/issues/1922) under ADR-0028 §3a and
   ADR-0036 §4. This preserves JSON meaning in memory without risking a
   normalized rewrite of unknown on-disk semantics. The retained projections
   preserve positions for later known updates/removals without executing
   unknown semantics. A same-version journal remains writable for known inputs
   (unknown contents imply a hand-edit or writer bug), while its existing
   checkpoint protection prevents discarding raw state. Once its worker requires
   a checkpoint, that refusal latches read-only `QueueJournalCorrupt` with reason
   `unknown_data_prevents_checkpoint`. All later mutations refuse through the
   same guard and status for that open handle. The location describes the last
   recovered cursor and the file length at refusal, including any torn suffix;
   it is not necessarily a record boundary. Known updates/removals cannot erase
   the raw unknown history.

   Slice 1 of [#1803](https://github.com/gerchowl/flock/issues/1803),
   [#1954](https://github.com/gerchowl/flock/issues/1954), adds the opt-in journal
   policy and configuration. `[sessions] queue_rewrite_bytes` is a positive
   integer byte count, with named default `DEFAULT_QUEUE_REWRITE_BYTES` of
   8 MiB (8,388,608 bytes). Zero is rejected, including by live config loading.
   A checkpoint is required when the existing file is **strictly larger** than
   this value, before another append. An append may cross the threshold; the
   next mutation checks it. This is a rewrite trigger, not a maximum snapshot
   size: a materialized queue can itself be larger than the configured value.

   `Journal::open_with_policy` installs the same policy for append and recovery.
   Recovery re-derives the typed unknown-data refusal above the threshold before
   repairing a torn suffix, leaving refused bytes unchanged. Known supported
   crash-torn tails keep their existing repair behavior. Known data are rewritten
   on the next append or explicit `checkpoint_if_required()` call, which returns
   the complete atomic cut after fsync, rename and directory fsync. A caller must
   publish that cut, or replay from its pre-operation cursor to include an
   automatic append-time rewrite; publishing just the appended record loses the
   checkpoint gap and snapshot. Refusal never returns a successful cut.

   Production enforcement remains a requirement on
   [#1955](https://github.com/gerchowl/flock/issues/1955), slice 2: the worker must
   use this policy with its configured byte threshold on every open, preserve
   serialized mutation ownership and publish the resulting cuts. The legacy
   `Journal::open` does not install a policy. Thus production same-version growth
   is not bounded by this slice alone, and
   [#1928](https://github.com/gerchowl/flock/issues/1928) remains open until worker
   integration. Reopening through the policy re-establishes the refusal without
   adding any persistent marker. A newer-stamped journal instead uses
   `QueueJournalNewer` read-only mode. There is no opaque checkpoint passthrough
   and no inference of a version from unknown contents.
2. **Malformed known records are corruption.** A supported-version journal still
   loads, but opens read-only with `QueueJournalCorrupt { seq, offset, reason }`.
   The cursor is optional when it cannot be parsed; the byte offset always
   identifies the first corruption. Reasons are stable diagnostic identifiers
   without input payload text. Recovery stops folding at that record: it never
   skips a corrupted enqueue or delivery-state transition and then serves later
   state. Known enum names with invalid payloads remain errors, never `Unknown`.
   A complete header with an invalid `format_version` (zero, non-integer,
   string, null or a missing JSON value) is also known-record corruption at byte
   zero. An absent key remains a valid unstamped v1 header; it is distinct from
   a stamped header with an invalid value. Invalid JSON, invalid known changes
   and structural sequence/checkpoint damage also fail closed this way. All mutations and checkpoints return the registered
   `queue_journal_corrupt` refusal, including empty replacements and idempotent
   admission. Original bytes, subsequent records, torn suffixes and temporary
   replacement files remain unchanged. Inspection/status are available, but the
   recovered prefix is not schedulable. Status includes the same location/reason
   with no reservation clock (`since_ms: 0`). This is an `after_human` refusal;
   recovery requires an explicit operator action, tracked in [#1915](https://github.com/gerchowl/flock/issues/1915),
   and is outside this PR. No
   automatic rewrite, resend or retry can resolve corruption.
3. **An unterminated final line is a crash tail, not corruption.** For supported,
   otherwise healthy journals, the existing recovery path truncates that tail
   and syncs before another append. A torn first identity with no later records
   is repaired as described above. Newer or corrupt read-only journals are never
   repaired. The pre-existing 2 MiB fail-ceiling still refuses an oversized torn
   line rather than repairing it. A complete oversized supported record opens
   read-only as corrupt. Recovery sync failures and missing/mismatched identity
   bootstrap failures still refuse open; they cannot invent a trusted journal.
   Invalid complete JSON, including a damaged stamped header, is inspectable
   read-only; an unterminated first line retains the existing crash repair.

Persistence remains fsync of the temporary file, rename, then fsync of the
directory. Disk operations are blocking worker operations and must never run
on the server event loop or during render.
