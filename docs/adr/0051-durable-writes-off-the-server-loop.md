# ADR 0051 — Durable writes leave the server loop through bounded, ordered owners

- Status: Proposed
- Date: 2026-11-08
- Issues: [#3013](https://github.com/gerchowl/flock/issues/3013)
- Decision owner: operator
- Implemented: none. This PR contains a measurement script and its test-only
  interposer, an index entry and this proposal. It changes no production code.

## Evidence and scope

The incident was on a Linux host with a contended HDD. The issue reports
7.6 s and 13.3 s event-log fsyncs, a main thread sampled in
`jbd2_log_wait_commit` 20/20 times, and attach failing after about 5.2 s.
Those are incident observations, separate from the local experiment below.

The inventory is pinned to current `origin/dev` at investigation time,
`c51352842848367047292b4efa263f82c9c8e039`. All `src/...:line` citations in
this record refer to that revision. In particular,
[`PersistentLog::append`, src/api/event_hub.rs:116](https://github.com/gerchowl/flock/blob/c51352842848367047292b4efa263f82c9c8e039/src/api/event_hub.rs#L116)
writes and calls `sync_data()` for each persisted event. `EventHub::push`
holds `inner` across that call (`src/api/event_hub.rs:420–456`). It advances
`durable_sequence` before the sync and logs append errors without returning
them. Today a successful request is therefore not universally proof of a
successful durable event append.

Thin-client acceptance runs on that same server loop:
`HeadlessServer::accept_client_connections` (`src/server/headless.rs:1935`)
calls `accept_pending_client_connections` (`src/server/client_accept.rs:10`).
Only after accept does it spawn a handshake thread. The client's Welcome read
has a five-second socket timeout (`src/client/mod.rs:1013–1019`). Moving only
the handshake reader cannot help a connection still in the listener backlog.

### Inventory of synchronous persistence

Frequency below means calls per trigger, not an inferred events-per-second
rate. Requests executing on the main loop all wait indirectly behind a main
loop sink, including attach acceptance; the request column names direct
callers. A write on another thread can also block the loop through a shared
mutex. File names are relative to the configured session/state root unless
an integration profile is named. Registries that fold the event log or live
inside `session.json` are listed with their actual sink, not invented files.

| File / sink and function at pinned dev | Frequency and sync discipline | Request / loop dependency | Recovery role |
| --- | --- | --- | --- |
| `event-log.jsonl`, `PersistentLog::append`, `src/api/event_hub.rs:116`; `EventHub::push:420` | One `sync_data` per `EventKind::is_persisted()` event (`src/api/schema.rs:2903`), possibly several per request | Workspace/tab/pane mutations, lifecycle ticks, mail receipts, goal/work/owner/delegate facts and rotation reissues; all main-loop callers wait. Other hub users wait on `inner`. | Audit stream for workspace/tab/pane lifecycle; recovery source for the explicit goal/owner/delegation folds (see class contracts below). Not workspace recovery. |
| `event-log.1.jsonl` through `.4.jsonl`, `PersistentLog::rotate`, `src/api/event_hub.rs:131` | At 32 MiB or 100,000 appended events; rename/delete/open; **no directory sync** | The append crossing either threshold and its callers. New log creation also has no explicit directory sync. | Same retained event history and folds as the active log; not an independent recovery authority. |
| `session.json`, `session-history.json`, `save_json_with_directory_sync`, `src/persist/io.rs:56`; `save_to_paths:88` | Temp file `sync_all`, rename, directory `sync_all` for each saved file; directory failure logged but save returns success; clears (`clear_path:103`) unlink without directory sync | `App::save_session_now`, `src/app/session.rs:50`, after dirty debounce of 5 s (`src/app/mod.rs:87`), hibernation, finish/shutdown and explicit saves. Main-loop deadline at `src/server/headless.rs:4101`. Structured registry, aliases and workspace metadata are in this snapshot; optional history is a second replacement, not an atomic pair. Ordinary dirty mutation replies may precede this save. | Recovery source: session is authoritative for workspace/layout/structured registry; history is optional terminal history, recovered independently. |
| `mesh-mail.sqlite`, WAL and checkpoint, `Store::open_with`, `src/mesh/store.rs:385`, `handoff_generation:457`, `checkpoint:1446` | SQLite WAL, `synchronous=FULL` (`:427`), transaction/autocommit durability through SQLite; first open also file and directory `sync_all` (`:443`). One or more commits per custody/clock/routing mutation, checkpoints during maintenance | `runtime_store::with_store` (`src/mesh/runtime_store.rs:97`) holds the process writer mutex. Main-loop message handlers (`src/app/api/messages.rs:507,1464,1738`), receipts (`src/app/api/mesh_receipts.rs:161–398`), routing/collection ticks and enrollment may call it; worker calls contend with them. Handoff `suspend` writes the generation before dropping the store (`src/server/headless.rs:1439`). | Recovery source of truth for custody, routing, pins, deadlines and handoff generation, including committed WAL. |
| Same SQLite database, `Store::accept*`, `import`, `finish`, `settle_queue_admission`, `read_inbox`, `schedule_retry`, pin/reset/migration/quarantine/maintenance methods, `src/mesh/store.rs:527–1446` | Transaction commits or autocommit statements, including logical-clock updates; cannot treat apparent reads taking `&mut Store` as sync-free | Mail send/reply/read, peer enrollment/reset, queue custody settlement and maintenance. Subsidiary transaction sites: `src/mesh/store/routing.rs:8,167`, `collection.rs:110,402`, `status.rs:391,508`, `tombstones.rs:40`, `native_inbox.rs:156`, `queue_custody.rs:39`, `schema.rs:38,214`. These are one store authority, not independent queues. | Same authoritative SQLite state; per-method transaction semantics apply. |
| `mesh-quarantine.jsonl` and rotated sidecar, `quarantine::append`, `src/mesh/store/quarantine.rs:98–135` | On invalid row recovery: append `sync_all`, directory `sync_all`, before destructive SQLite update | First-open/maintenance/store callers above wait while the SQLite write transaction is held. Failure must retain the original row. | Recovery evidence preserving raw rejected rows; not the active custody source. |
| `mesh/identity.json` and identity directory, `NodeIdentity::load_at_with_lock`, `src/mesh/identity.rs:114–199`; `create_private_dir:215` | First generation: temp sync, rename, directory sync; reopening syncs the directory too. Newly created ancestor directories sync their parent | Lazy store bootstrap (`src/mesh/runtime_store.rs:48`), enrollment/startup. The identity is process-cached after validation. | Authoritative node key/identity binding; must never regenerate on a load fault. |
| `client-tokens/*.token`, `TokenStore::save`, `src/structured/permissions/auth/tokens.rs:264`; `Namespace::open/publish/unlink`, `src/platform/client_tokens.rs:32,188,203` | Open syncs parent; issue/replacement temp `sync_all` then link/rename and directory sync; revoke/unlink directory sync, including temp cleanup | Synchronous Unix management handler (`src/app/api/client_tokens.rs:94`) opens the store for token add/remove/list/browser handoff. Auth workers also use this namespace and its fences. Ephemeral app minting itself is memory-only after store initialization. | Authoritative persistent credentials/revocation namespace; ephemeral app credentials are not recovered. |
| Channel token files, `issue_token`, `src/channels.rs:242–309` | Per absent/invalid binding token: temp `sync_all`, publish, directory `sync_all` | Startup and config reload call `issue_tokens` (`src/app/mod.rs:603,2026`). Reload/input handling can delay accepts. | Authoritative channel credential files, regenerated only under existing trust/validation rules. |
| Agent integration profile configs/hooks/trust, `atomic_write::replace`, `src/integration/atomic_write.rs:9–32` | Per changed integration file: temp `sync_all`, rename, directory `sync_all` | Profile preparation / installation / reconciliation: `src/integration/mcp_config.rs:157`, `claude_trust.rs:354`, `sync.rs:312`, `mod.rs:1397,1499,1861,1863,2127`. Terminal-backed launches may prepare profiles synchronously; structured launch preparation already runs in its launch worker (`src/structured/workers.rs:262`). Preserve per-profile locks. | Authoritative integration configuration for the harness; not a source for workspace restoration. |
| Config overrides/editor backups, fleet pause, announcement/release caches and resume hints | **No fsync**: direct write or temp/rename in `src/app/config_io.rs:17,47,91`, `src/app/fleet_pause.rs:69`, `src/product_announcements.rs:212`, `src/release_notes.rs:60`, `src/agent_resume.rs:378`; API digest/export and config restore also directly write (`src/app/api/digest.rs:73`, `src/app/api.rs:1011`) | Settings/input, pause, reload, resume and export callers can still block on filesystem writes. These are persistent files, but no current crash-durable acknowledgement should be inferred. Keep them off the loop in follow-ups; do not silently upgrade their contract in this PR. | Mixed: config and pause files are configuration recovery sources with existing best-effort durability; caches/exports/hints are derived data. No durable success contract. |

The following durable sinks are already owned off the main loop. They remain
part of the audit because shutdown can join them or a main-loop call can
accidentally reclaim their locks:

| Existing owner and files | Frequency / sync / waiters | Recovery role |
| --- | --- | --- |
| Queue journal, `Journal::open/append/checkpoint`, `src/structured/queue/mod.rs:130,305,395,472,640,655` | Worker-owned (`:1`), append syncs data; recovery, rewrite, stamp and lease/file lifecycle sync files/directories. `Service::open`, `src/structured/workers/queue.rs:236`, runs on the launch worker, then the event pump owns it. Admission/terminal receipt waits for journal completion; do not move this onto the app thread. | Authoritative queue admission and terminal-state recovery journal, with existing opaque-row rules. |
| Structured stream and permission audit records, `transport::persist`, `src/structured/transport/mod.rs:993–1079`, setup at `:861` | Per persisted stream record `sync_data`; setup `sync_all`; stream owner controls permission-close and input acknowledgement order. Reads use a separate blocking budget (`src/structured/history/owner.rs:25–34`). | Authoritative structured stream/permission evidence and history; not workspace recovery. |
| Blob objects and namespace, `Store::put`, `src/structured/blobs/mod.rs:188–229`; `Namespace::open/rename/sync`, `src/platform/blobs.rs:13,92,108` | File sync before publication; directory sync on namespace creation/publication; stream/history workers wait. A reference must never precede its blob's durability. | Authoritative immutable blob bytes addressed by committed references. |
| Structured process ownership journal `owner.json`, `Owner::write`, `src/structured/supervision/mod.rs:172–184` | Temp sync, rename, directory sync on prepare/arm/child bind/refresh/cleanup. Structured launch/supervision workers own it (`src/structured/workers.rs:262`, `src/structured/launch.rs:440,544`); startup sweep can perform recovery before serving. | Authoritative process ownership/cleanup evidence; not a resumable workspace snapshot. |
| Fleet projection snapshot and cursor, `replace_with_parent_sync` / `Store::commit`, `src/structured/fleet_lite/mod.rs:173,452` | Temp sync/rename/directory sync for both records under one lock, on changed projection/snapshot/repair. Serialized bounded off-executor owner (`src/web/wire/fleet.rs:192,227`). | Derived fleet projection plus authoritative cursor continuity; reconstruct projection only under its existing cursor recovery rules. |
| CLI delegate registry/startup-block records, `src/cli/delegate.rs:939–950`, `src/cli/delegate_lock.rs:10–12` | File and directory sync in a CLI process. Not a server accept-loop write and must not be moved into its queue. | CLI-owned recovery records for delegate startup/block state; distinct from server session state. |

Audit method: search all production Rust for `sync_all`, `sync_data`, `fsync`,
SQLite `commit`/`synchronous`, file create/write/rename, then trace callers to
main, worker, startup or CLI ownership. This includes implicit SQLite syncs
and excludes test fixtures. Owner bindings use `record_owner_event`
(`src/app/owners.rs:80`); there is no separate owner registry file. Any new
durable store must name its owner and receipt boundary in its slice review.

### Reproducible measurement

[`scripts/probe_durable_attach.py`](../../scripts/probe_durable_attach.py)
compiles [`slow_fsync.c`](../../scripts/slow_fsync.c) as a test-only interposer.
It matches the private event-log inode, consumes a one-shot arm file, reports
whether the sync caller is the process main thread, sleeps eight seconds,
then performs the real sync. Linux intercepts `fsync`/`fdatasync`; macOS also
intercepts `fcntl(F_FULLFSYNC/F_BARRIERFSYNC)`. No FUSE mount, disk pressure,
privileges, production hook or patched production source is needed. macOS
requires an unsigned dynamic debug binary that permits DYLD insertion.
Linux is supported by the script but was not measured in this macOS session.

```sh
export CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 RUSTC_WRAPPER=
export CARGO_TARGET_DIR=/private/tmp/claude-502/fleet-targets/r3013
nix develop --command cargo build --locked --bin flk
nix develop --command python3 scripts/probe_durable_attach.py \
  --bin "$CARGO_TARGET_DIR/debug/flk"
# Future acceptance: this command MUST fail against the unmodified baseline.
nix develop --command python3 scripts/probe_durable_attach.py \
  --bin "$CARGO_TARGET_DIR/debug/flk" --assert-responsive
```

The script refuses load ≥10 or free disk <15 GiB, clears inherited `FLOCK_*`
and loader variables, sets private HOME/XDG roots, uses a short `/tmp/da-*`
socket base, seeds onboarding off, launches an owned foreground server and
creates an actual workspace through its private API. The client runs in a
40×120 PTY. It detects terminal setup after Welcome, not a successful socket
connect. Readiness has one 20 s monotonic budget covering connect, send and
complete NDJSON response. The trigger has a separate 20 s monotonic budget
covering send, marker wait, the attach observation and its complete response;
partial bytes never reset either budget. The PTY observation has its own 7 s
absolute ceiling. The arm-entry handshake precedes the stalled attach; failure to
enter is an error, not a skipped measurement. All waits have ceilings and
marker waits check server liveness; socket responses share absolute deadlines.
`finally` terminates/reaps only owned processes.
The script does not call a server-stop/discovery command in the live environment.

Measured on macOS, debug binary built from pinned dev, 2026-11-08:

| Run | Control attach | Attach during injected 8 s sync | Result |
| --- | --- | --- | --- |
| Initial reproduction | 0.658 s | 5.359 s | Exit 1, raw `protocol error: I/O error ... (os error 35)` |
| Regression assertion | 0.663 s | 5.353 s | Same error; sync marker says `main`; trigger completed in 8.048 s; assertion exits 1 as required |
| Final script rerun | 0.675 s | 5.347 s | Same error, sync on `main`; trigger completed in 8.227 s |
| Revision with absolute response deadlines | 0.399 s | 5.350 s | Same error, sync on `main`; trigger completed in 8.102 s; responsiveness assertion fails as required |

The revised socket reader has a live-trickle test with a 100 ms whole-response
deadline and a 500 ms failure ceiling. Injecting the old inactivity-reader
behavior makes that test fail after 0.745 s; the real absolute-deadline reader
passes. Additional tests cover already-expired deadlines with readable bytes,
EOF before newline and bounded response size. These validate the probe; the
production client remains unchanged and needs slice A's equivalent frame tests.

The future gate requires an attach within 2 s while the writer is stalled.
These measurements establish the timeout failure; they are not a throughput
benchmark or a claim about Linux latency. macOS's EAGAIN is 35; the incident's
Linux EAGAIN is 11. No implementation speedup is claimed.

## Proposed decision

Use a dedicated durable-writer thread with a bounded queue, ordered commands,
group commit and completion receipts for the event log and session snapshots.
The main loop submits immutable owned bytes/operations with `try_send`, retains
bounded continuations and handles completions in later turns. It must never
wait, join, acquire a disk-held mutex, serialize an unbounded history, or
`.await` the receipt inline in its accept/event-dispatch turn. A runtime task
that awaits a receipt independently is fine. Reads/attach/render consult the
committed in-memory projection without acquiring the writer's lock.

Keep already isolated journal/stream/blob/auth/fleet owners and their fences.
Move synchronous main-loop SQLite and credential/profile operations behind
their own bounded serialized owners, using the same admission/completion
contract. Do not make one slow event log freeze unrelated credential or mail
I/O. SQLite retains its single connection authority and transactional ordering;
do not split transactions into ad hoc file writes or change `FULL` to `NORMAL`.

### Durability, ordering and publication

1. A receipt means the operation's durability boundary succeeded. Event-log
   receipts require append and data sync, plus parent-directory sync on
   create/rotation. Snapshot receipts require temp sync, rename, directory
   sync. A failed directory sync is a failed receipt, even if rename happened.
   The guarantee assumes the filesystem/device honors its sync contract; no
   new claim about hardware power-loss behavior is introduced.
2. Reserve sequence numbers in submission order. The writer owns file handles,
   rotation and the last committed high-water mark. Publish durable events to
   readers and release replies/effects that promise that durable fact only
   after its receipt. A separate session receipt is required for an explicit
   recoverable-state save; an event receipt cannot substitute for it.
   Allocation is not commitment. Failed/uncertain
   syncs fence dependent operations; do not recycle sequences in-process or
   publish them as durable. Restart follows the preserved log's highest sequence,
   including unknown kinds, with the existing gap rules.
3. Group adjacent log appends: at most 64 operations, 256 KiB encoded bytes,
   or 5 ms oldest-item age before starting sync. Under a backlog, drain an
   available batch immediately. One sync may acknowledge the batch only after
   every included write succeeded. A partial append/sync failure gives no
   success receipt for an unproved suffix; fence and recover the store.
   Never coalesce durable events. Coalesce only unsubmitted, replaceable
   session snapshots; a flush/explicit save pins its generation.
4. The shared event/snapshot queue is FIFO across files. Explicit barriers
   connect the in-memory snapshot generation N to event watermark S for drain
   accounting only. Neither is a new persisted recovery field. Two renames
   are not an atomic cross-file transaction: retain independent snapshot/history
   recovery under the existing format (algorithm below).
   Across owners, use explicit receipt dependencies (blob before reference,
   queue custody before delivered receipt, quarantine archive before row clear).
   Never claim a global transaction across SQLite, the log and snapshots.
5. An acknowledged durable fact survives in its named recovery/audit source,
   according to the class contracts below. This does not promise that an
   audited workspace is recreated on restart. A disconnect or timeout after
   submission has an **unknown outcome**, since
   an in-flight fsync cannot be cancelled. Inspect state or deduplicate with
   the request's stable identity before retrying a mutation. Queue acceptance
   alone is never a durable success acknowledgement. Existing eventual session
   autosave remains eventual: ordinary UI edits are not suddenly synchronous
   saves. Explicit flush/save waits for the relevant snapshot receipt;
   event-backed state APIs wait for their event receipts.
6. Startup/recovery remains lenient (ADR-0028 §3a): skip and log unknown/newer
   or invalid persisted list/session entries and preserve their raw bytes in
   subsequent saves. Keep unknown fields and sequence high-water marks. Moving
   serialization must not round-trip opaque registry rows through a stricter
   type. Existing event-log corruption/gap rules remain distinct from list
   leniency. Test temp sync → rename → directory sync at every crash cut.

### Class contracts and recovery algorithm

The recovery-role column above is part of each sink's acknowledgement contract:

- **Audit facts:** workspace/tab/pane lifecycle events describe actions. A
  receipt proves the fact is in the durable event log and available under its
  retention policy. It does not restore the object. Workspace create emits
  three events (`src/app/api/workspaces.rs:90–105`), but restart loads
  `session.json` (`src/app/mod.rs:680–706`). Preserve eventual autosave for
  create and ordinary layout edits: after an audit-acknowledged create, a crash
  before session save may lose the workspace while retaining its audit fact.
  Tests must demonstrate that distinction, not expect log replay to recreate it.
- **Event-fold state:** goals, owner bindings and delegations already fold
  retained events on startup (`src/app/delegation.rs:244–247`,
  `src/app/goals.rs:121`, `src/app/owners.rs:87`). For these APIs the successful
  event receipt is the recovery boundary. Preserve their existing rotation
  reissues and fold rules. Any work-state family must identify its actual
  persisted fold before activation; the presence of a lifecycle event alone
  is not evidence of a recovery reducer.
- **Snapshot state:** explicit session save/flush promises recovery from the
  saved session, so its reply waits for session temp sync, replacement and
  directory sync. Optional history has a separate receipt. A flush asking for
  both succeeds only after both complete, but failure between them has an
  unknown/partial outcome. It never promises an atomic pair or PTY process
  resurrection. Ordinary mutation replies retain their documented eventual
  session-save behavior rather than acquiring an unstated restart guarantee.
- **Authoritative stores/records:** custody, queue admission, credentials,
  identity, blobs and ownership records use their existing transaction/file
  recovery sources named in the inventory. Reply only after the receipt for
  that source and any documented dependency (for example blob before reference).
  Derived projections/caches do not become authoritative through offloading.

Snapshot generations and event watermarks are process-local queue/barrier
tokens, not a new on-disk consistency scheme. Recovery is concrete:

1. Acquire writer ownership first. Parse `session.json` using the existing
   lenient entry-preserving loader; restore workspace/layout/structured registry
   from that file alone. Ignore incomplete temp files as recovery authorities.
2. If history persistence is enabled, load `session-history.json` independently
   using its existing matching/restore rules. Missing/stale history must never
   discard a valid session or reconstruct a workspace absent from it. A crash
   between replacements can therefore yield new session state and old history;
   this is an explicitly allowed partial save, not a cross-file transaction.
3. Read retained event logs with existing corruption/gap/unknown-kind rules.
   Initialize sequence allocation from their highest spent sequence, and fold
   only the existing event-backed state classes. Reissue surviving folded
   state under the single writer after ownership is established. Do not replay
   workspace audit events or infer a snapshot checkpoint from event sequence.
4. Open SQLite/journals/credentials under their existing fences and recovery
   rules. Reconcile cross-owner references only using their current committed
   records; a log/snapshot pair does not override custody authority.

No persisted generation field or new loader format is required for this plan.
Old/new binaries keep the current file formats and raw unknown-entry rules;
an older binary ignores orphan temp files as today. Mixed-version handoff must
refuse before transfer if it cannot negotiate the new drain/ownership protocol,
or use a compatibility path that proves both sides quiesced under the same
single-writer fence. It must never silently fall back to the old writable
pre-commit importer. A future atomic session/history checkpoint or workspace
event-replay design needs its own recovery algorithm and independently sized
slices; it is outside this proposal.

### Back-pressure and errors

Admission starts at ingress, not at the writer. Today startup constructs an
unbounded API channel (`src/server/headless.rs:4335,4447`) and dispatch moves
owned requests into it (`src/api/server.rs:1048`). Replace that boundary before
activating asynchronous callers: reserve a request slot and its worst-case
bounded frame/decoding footprint before reading/decoding/cloning an owned
request or enqueueing it. Cap admitted request count at 256 and combined
buffered/decoded/queued request bytes at 4 MiB. Keep the existing frame cap or
use the smaller remaining admission allowance; reject excess before allocating
the payload. Decode expansion must fit the reservation, not be accounted after
allocation. Limit pre-admission readers to 64, each with a bounded prefix
buffer and an absolute read deadline; reject excess connections promptly.
Read-only/control/attach traffic has a separately bounded reserved lane, so
write saturation cannot starve liveness checks or handoff refusals. Permits
remain charged through dispatch, pending durable completion and reply. Release
only resources actually freed on refusal/completion; disconnect may detach the
reply but retains charges for any admitted operation and resident payload until
completion. Disconnect must neither cancel admitted effects nor bypass limits.
The same limits must cover the handoff-import startup path and every transport
that dispatches app requests. Per-connection limits alone are insufficient.

Initial limits per owner: 256 queued operations and 4 MiB total owned payload,
including the active batch; 256 waiting request continuations. Enforce bytes
before cloning/serialization expansion. A single oversized snapshot goes
through a separately bounded streaming/encoding slot (one, 32 MiB ceiling);
oversize is an explicit error, not a limit bypass. Validate these starting
limits against realistic history sizes before acceptance.

When full, `try_send` returns `persistence_busy` before applying the requested
mutation or side effect: CLI `REFUSED` (10), retry `later`. Pending auto-saves
retain only the newest dirty generation. Internal event producers must reserve
capacity before committing their transition; if capacity is absent, retain a
bounded pending transition and pause that producer. Never silently drop a
durable lifecycle fact, spin-retry or allocate an overflow queue. Already
durable/read-only state and attach stay available. Reserved control capacity
admits a flush barrier even when ordinary admission is closed.

An I/O error after admission returns `persistence_failed` with
`outcome=unknown`, CLI `FAILURE` (1), retry only after inspection. Reject new
dependent mutations until recovery; retain old files/raw records. Expose queue
bytes/age, committed watermark, failures and last successful sync time without
payloads. Clients awaiting durable changes may time out while attach remains
healthy. Timeout does not imply rollback.

### Shutdown and live handoff

This is a prerequisite to activating **any** asynchronous caller, including
autosave, internal events and existing synchronous adapters routed through the
new owner. Today's importer opens a writable event log before committed
transfer (`src/server/headless.rs:4449`) and shutdown invokes `finish_session()`
without a new-writer drain (`:4213`). Those boundaries must be fixed in G0a/G0b
before C/E/D can activate a writer. Dormant infrastructure may land earlier;
there must be no intermediate dev revision with asynchronous writes and the
old import/shutdown path. Every later owner migration extends this fence before
enabling its first caller, in the same PR if necessary.

Close mutation admission, quiesce producers, enqueue a barrier with the final
snapshot generation and event watermark, and drain every affected owner.
Continue accepting clients long enough to send the existing handoff refusal
and render progress. Barrier waiting is a state machine, not a blocking join
on the loop. Normal shutdown allows a 30 s drain ceiling; failure reports
unflushed/uncertain work and a non-success exit, never a clean-flush claim.
A stuck kernel sync cannot be safely killed by dropping a task or writer.

Before exporting handoff ownership, all accepted operations must reach their
barriers and the SQLite generation fence must complete. Preserve current
worker stop/permission-close semantics. The exporter retains sole writer
ownership until release; `--handoff-import` must validate transferred committed
watermarks/generations and acquire the existing file/store fences before
opening writers. No writer-thread handle crosses the manifest. Import starts
fresh owners only after ownership transfer. On a 30 s pre-transfer drain
timeout, abort handoff without starting a competing writer, report
`persistence_busy`, and restore old admission only when its ownership is
certain. After committed transfer, never revive the exporter on importer
failure. A drain failure cannot be disguised as a compatible handoff.
The actual handoff phase budget must be reconciled with the current bounded
protocol (`src/server/handoff.rs:73`) before adding these fields.

G0a's failing-first test pauses the real import protocol before committed
transfer and observes writable event-log opens and startup reissues: current
`:4449` opens too early, so the test must fail on baseline. The candidate must
perform validation in a read-only mode, with no truncation, rotation or reissue,
and defer all writer opening until committed ownership. G0b's staged-owner
test holds an accepted write before sync while driving actual shutdown/handoff
entry: transfer/clean shutdown must remain pending, attach refusal must stay
responsive, and releasing sync must complete the drain before import opens.
Inject a missing drain to prove failure, with a bounded silent-writer ceiling.
Also test importer death and pre-transfer drain timeout. These tests land and
pass before first asynchronous activation, not in the final integration slice.

### Alternatives

| Option | Crash acknowledgement / ordering | Stalled disk and bounds | Shutdown / handoff / leniency | Verdict |
| --- | --- | --- | --- | --- |
| Dedicated thread, bounded FIFO and group commit | Receipt after required syncs; one owner orders appends, rotation and replacements; cross-owner receipt dependencies | Count and byte admission bounds, nonblocking rejection and reserved barrier; no loop waits | Explicit drain/watermark/fence; preserve raw snapshot entries in owned bytes | **Chosen**: fits the synchronous filesystem API and makes order, overload and ownership explicit. |
| `spawn_blocking` per write | Awaiting the task only guarantees its own sync. Concurrent temp names, appends and renames can reorder; needs keyed serialization and receipt publication | Tokio's blocking-task submission queue alone supplies no application payload bound; semaphore plus byte accounting required. Awaiting on the app turn still freezes dispatch | Running blocking syscalls cannot be cancelled; must track/drain all handles and transfer fences; same lenient loader required | Rejected as the default. With bounded admission, serialization and grouped receipts it recreates an owner less directly. Existing properly bounded blocking owners stay. |
| Keep synchronous writes, move accept to a separate thread | Existing write/ack semantics remain, including swallowed errors; one state thread keeps order | Can send Welcome during fsync, but first usable frame and state-changing requests still wait; hub/store mutex stalls remain; bounded accepted-client queue still required | State-thread shutdown/handoff still waits on disk; accept thread must coordinate refusal and ownership; no leniency improvement | Rejected as sufficient repair. A separate acceptor may be useful independently, but Welcome without a usable state projection does not satisfy the attach regression. |

Moving the entire state/request executor to a separate thread would free an
acceptor but keep mutations and frames serial behind fsync. Making that option
responsive requires independently scheduled durable completions, bringing it
back to the chosen ownership model. Merely increasing the five-second timeout
does not bound stalls or resolve durability acknowledgement.

## Attach client contract

Start a single monotonic five-second Welcome deadline immediately after
sending Hello. It covers the entire length prefix and payload, not each
`read_exact` or each byte. Poll/read nonblocking under the remaining budget,
rechecking the clock after partial progress and interrupted syscalls. Neither
a trickled header nor trickled payload may reset it. Current
`set_read_timeout` plus `read_exact` (`src/client/mod.rs:1013`,
`src/protocol/wire.rs:1023–1035`) is an inactivity bound, so it does not yet
implement this contract.

Map expiry of that whole-operation deadline to typed
`attach_timeout`: `server did not answer attach within 5s (server busy?)`.
CLI exit is existing `TIMEOUT` (124), retry `yes`; JSON-capable entry points
use that code and include `phase=welcome`, `timeout_ms=5000`. A platform
`WouldBlock`/`TimedOut` before expiry means re-poll with the remaining budget;
it is not an early deadline. Never match platform-specific errno numbers or
formatted text. Other I/O failures,
EOF, malformed frames, incompatible versions and explicit handoff refusals
keep their distinct paths. Restore terminal state before emitting the error.

No automatic general timeout retry in the first slice. The user can retry
attach safely: close the failed stream and create a fresh one, never replay
terminal input. Keep existing bounded live-handoff retry for explicit handoff
refusals. Add a separate first-frame progress/deadline assertion in the server
regression so an early Welcome cannot hide a still-frozen UI. This is a local
client error classification and needs no wire change; any later manifest/wire
extension follows the released-version migration rule. Mesh VERSION stays 5.

## Ordered implementation slices

Each slice has a hard ceiling of 400 production lines (tests/doc comments
excluded). Budgets are estimates, not permission to omit the acceptance gate.
If the inventory's caller migration exceeds a budget, split by handler family
before implementation. No slice may advertise the complete repair until the
real-socket regression and handoff gates pass.

Before implementation begins, record an isolated **Linux baseline** with
verified interposer entry, thread identity, complete timeout diagnostic and
first-frame timing (or explicitly no frame before timeout). Extend the probe's
current post-Welcome terminal-setup marker to identify an actual usable frame;
terminal setup alone is not frame evidence. This macOS-only design receipt
does not satisfy that gate. Record both macOS and Linux baseline/candidate
runs for each activated family, including normal start and handoff import.

Land ingress bounds and dormant owner infrastructure first. G0a/G0b must pass
before C routes the first caller through an active writer. During mixed
migration, synchronous and asynchronous adapters share **one sequence allocator,
one writer and one FIFO**, including rotation and snapshot commands. The
synchronous compatibility adapter may wait for that owner's receipt only while
its caller remains unmigrated; it may never append directly or own another file
handle. Replace all adapters atomically at that ownership boundary. Remove the
blocking adapter once all callers migrate. If retaining a legacy path requires
a second writer, the activation must not ship.

When unmigrated synchronous callers would prevent the family gates from passing,
land that family's code dormant and enable it only after those callers migrate.
Tests may activate the staged candidate in isolation; production activation
requires the complete affected call graph and G0 fences. The activation PR must
carry the real-path evidence, even if enabling the already-reviewed code costs
one line. This permits small preparatory slices without claiming a partial
repair is responsive.

For **each** activated family (C, E, D1/D2, F1/F2), that PR must include its
own real-entry crash test against the declared recovery source, upstream and
writer saturation tests, stalled-write shutdown/handoff/import-fence tests,
and stalled-disk attach **and first usable frame** tests. Run the regression
against its pre-change build and show failure before candidate success. H is
an aggregate audit and metrics slice, never permission to postpone those gates.
Split any family further if production code would exceed 400 lines.

| Order | Scope / estimated production lines | Acceptance tests |
| --- | --- | --- |
| A | Whole-Welcome deadline, timeout classification and exit mapping, 150–250 | Fake peer accepts Hello and stays silent: friendly diagnostic, exit 124, restored PTY. Trickle both length prefix and payload below the inactivity timeout: total Welcome still expires at 5 s. EOF/malformed/version/handoff paths stay distinct; no input replay. |
| B1 | Bounded ingress before allocation/dispatch on both startups and all app transports, 250–350 | Saturate slow requests and large frames: resident raw/decoded/queued bytes and request count stay bounded before writer admission; malformed/oversize/refused/disconnected requests release permits; read-only/control/attach remain responsive. |
| B2 | Dormant bounded writer commands, byte reservations, receipts and barriers, 250–350 | Stalled sink: saturation rejects before mutation, active bytes counted, dead writer resolves waiters, silent writer fails at a ceiling; no disk-held projection lock. No production caller activation. |
| G0a | Import ownership before writable open/reissue, compatible negotiation/refusal, 200–350 | Failing-first real import test pauses before committed transfer: baseline writable log open is observed and fails; candidate performs read-only validation and opens/reissues only after ownership. Incompatible import refuses before transfer. |
| G0b | Shutdown/handoff admission close, asynchronous drain and rollback with staged owner, 250–350 | Staged accepted write held before sync prevents transfer/clean exit, while refusal stays responsive. Release completes barrier before import opens. Inject absent drain to demonstrate failure; stuck writer, full queue, importer death and phase-budget tests. Must land before C/E/D activation. |
| C | Event-log owner activation, one legacy adapter/allocator/FIFO and group commit, 250–350 | Batch thresholds and sync count, rotation order, mixed synchronous/asynchronous submissions, failed/partial sync and directory failure, unknown-kind highest sequence. Mandatory family crash/saturation/handoff/attach+frame gates; all event-hub groups. |
| E | Session/history owner and process-local receipt generations, 250–350 | Explicit save reply → kill → restore from session, not events. Autosave latest-only coalescing and pinned save barrier; crash between replacements allows new session/old history; raw unknown rows and older-format load preserved; family gates and session group. |
| D1 | Deferred workspace/pane/tab audit handlers and publication, ≤350 per family | Real create → audit receipt → reply → crash before autosave: event remains, workspace restoration uses previous snapshot. After explicit save receipt, restart restores the new workspace from session. Fault cuts, uncertain retry, family gates and full affected handler groups. |
| D2 | Goal/work/owner/delegate/message continuations and lifecycle producers, ≤400 per family | Declare each family's actual recovery reducer/store; event-fold classes replay acknowledged state, audit-only classes retain facts without invented reconstruction. Drive real requests/ticks; family gates, bounded pending facts and every delegate integration binary when affected. |
| F1 | SQLite custody/receipt/routing adapters, ≤400 per family | Stalled WAL cannot hold a main-loop mutex; recover from committed DB/WAL; quarantine-before-delete, native inbox and custody receipts remain ordered. Family gates and relevant mesh groups, including extension of G0 fences before activation. |
| F2 | Credentials, reload tokens and terminal profiles, ≤350 per family | Recover authoritative files; no success before directory sync; revoke/admission fences and real launch/reload paths. Family gates and extension of G0 fences before activation. Existing structured owners unchanged. |
| H | Aggregate coverage audit, metrics and limits, 100–200 | Inventory has no unfenced/loop-blocking writer left; collect already-required Linux/macOS baseline/candidate, first-frame, crash, saturation and handoff evidence for every family. Targeted groups locally, full CI. |

## Revision decision log (round 1)

All required changes in the pre-mortem are accepted: handoff/shutdown safety
precedes activation with failing-first tests; recovery and audit contracts are
separate; whole-operation deadlines replace inactivity assumptions; ingress is
bounded before owned request allocation; mixed adapters share one owner and
allocator; recovery/mixed-version behavior is explicit; Linux baseline and
per-family integration evidence are implementation prerequisites.

The earlier persisted snapshot/event generation idea is withdrawn. It implied
a joint recovery checkpoint that does not exist and would need a larger format
migration. Process-local receipt tokens and independent existing recovery
sources suffice for this proposal's audit and eventual-autosave contracts.
Atomic snapshot pairs and workspace replay are deliberately outside scope for
that reason. No requested review change is rejected. Linux measurement and
first-frame enhancement remain explicit gates for the implementation work,
not evidence claimed by this docs/test-script revision.

## Pre-mortem

- **Lost acknowledged fact:** enqueue returns success or a completion is sent
  before sync. Kill immediately after a real success reply and verify the
  promised source: audit event in the log, event-fold state from replay, or
  explicit saved session state from the snapshot. Do not assert workspace
  restoration from its audit event; inject file and directory errors separately.
- **Cross-file reordering:** snapshot N lands after N+1 or references an unsynced
  blob. FIFO generations and explicit receipt dependencies prevent publication;
  crash-cut tests must not pretend two renames are atomic.
- **Unbounded memory during a stuck disk:** a bounded channel hides unbounded
  serialized bytes, pending replies, active batches or rejected retries. Account
  all four, reserve before copying, and test oversized history plus full queues.
- **Handoff has two writers:** exporter still has a blocked append when import
  reopens the log. Refuse transfer until every owner barrier/fence succeeds;
  test pre-transfer failure and committed-transfer importer death separately.
- **False durability after a sync failure:** current logging-only error handling
  leaks into receipts or `durable_sequence`. Failed sync must fence publication
  and return unknown outcome, even when the bytes happen to be readable.
- **Hidden main-loop lock:** worker fsync holds the hub/SQLite/auth lock while
  read-only attach state takes it. Split committed projections from writer
  state; probe first usable frame, not just Welcome.
- **Leniency regression:** moving serialization drops an unknown structured row
  or strict-loads a newer session. Round-trip opaque raw entries across failed
  writes, restarts and a mixed-version handoff.
- **Flaky/hanging evidence:** fixed sleeps race entry into fsync, a missing hook
  passes as a fast disk, or a silent server hangs CI. Require entry marker and
  control attach, monotonic ceilings and producer liveness; the script's unit
  tests explicitly inject a live silent producer and require failure. Keep
  short sockets and private env on both platforms.
