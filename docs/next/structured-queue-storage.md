# Structured input queue storage

ADR-0036 §3/§4's storage core is available to dedicated workers as
`structured::queue::Journal`. Scheduling and harness lifecycle are #1802;
operator/CLI/message producers, the configured rewrite threshold and server
publication are #1803. This storage slice does not expose a new send route.

`Journal::open(data, agent)` acquires an exclusive OS lock on the stable
`sessions/<agent>/.queue.lock` file. It keeps that lease across atomic replacement
of `queue.jsonl`. Drop the old journal before handing ownership to a successor;
a cooperating second writer is refused while the old lease exists. Agent path
components permit only ASCII letters, digits, `-` and `_`. Namespace directories
are private and files are owner-only, with existing symlinks/nonregular files
refused. The data root itself may be an alias such as a canonicalized temp root.

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
can be present after recovery even though the call did not acknowledge it.

`items()` exposes the pure materialized queue. `bytes()` lets its owning worker
check the configured rewrite threshold. `checkpoint()` and appends both require
exclusive mutable ownership. For last committed sequence H and N retained items,
the replacement contains a header
`{"kind":"queue_checkpoint","before":H,"high_water":H+N}` followed by
snapshot records H+1 through H+N. `before` also lets recovery detect an incomplete
snapshot. The header consumes no sequence and is never a queue event. The worker
syncs the temporary file, renames it, syncs the directory, then publishes the new
cut. Unique `.queue-*.next` files left by failed/crashed replacements are ignored
by recovery; they are not committed state. A checkpoint after removing all items
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

On reopen, only an unterminated final tail is truncated and synced. Malformed
complete records, oversized lines, non-dense sequences, interior checkpoint
headers and incomplete snapshots are corruption and fail without repair.
Unknown string event kinds spend their cursors and remain available in replay,
but do not change the known materialized state (ADR-0028 §3a). A writer that
encounters such a kind refuses checkpointing rather than discard state it cannot
interpret; it can still append known changes. Future incompatible shapes inside
a known event are refused. Disk operations are blocking worker operations and
must never run on the server event loop or during render.
