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

Every journal begins with a synced `{"kind":"queue_identity","agent_id":"..."}`
header containing the original, case-preserved ID. Open verifies it against the
requested agent and refuses missing, mismatched or repeated complete identity
headers. An unterminated first line with no records following it is rewritten
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

On reopen, the journal is synced unconditionally; an unterminated final
record tail after a verified identity is truncated and synced again. An
unterminated first identity line is repaired as described above. Malformed
complete records, oversized lines, non-dense sequences, interior checkpoint
headers and incomplete snapshots are corruption and fail without repair.
Unknown string event kinds spend their cursors and remain available in replay,
but do not change the known materialized state (ADR-0028 §3a). A writer that
encounters such a kind refuses checkpointing rather than discard state it cannot
interpret; it can still append known changes. Unknown enum variant names inside known upserts are retained as typed `Unknown(raw)`
variants on the item itself, at its recorded position, without failing open.
They replace an older item with the same ID and preserve positions for later
upserts, removes and updates. Serialization retains the original raw JSON value;
rewriting is blocked by detecting `Unknown` variants, never by matching an error
message. Appending such typed values also blocks rewriting. Unknown fields
on known events survive in replay and block rewriting rather than being discarded.
Deserialization inspects the variant name before decoding a known payload.
Known names with malformed payloads (such as a numeric `system` producer or a
`channel` without its user), values without a variant name (such as numeric
state), missing required fields and invalid known changes cause open to fail
with a corruption error. While opaque
future kinds, fields or enum values prevent rewrites, appends can grow the journal
without bound; bounded handling of that condition is tracked in [#1820](https://github.com/gerchowl/flock/issues/1820). Disk operations are blocking worker operations and
must never run on the server event loop or during render.
