# ADR-0038 artifact amendment: bounded hosting and custody

- Status: **Proposed; one owner decision pending** (#1598, #1590).
- Date: 2026-10-29.
- Parent: [ADR-0038 §§5, 7–7f, 9 and decision 17](../0038-gui-workbench.md).
- Replaces the unresolved Q2–Q9 in the [G6 plan](https://github.com/gerchowl/flock/issues/1598#issuecomment-6482890895). These are concrete recommendations, not implemented behavior.

This annex is normative **only after acceptance**. Where the parent's artifact sketches differ, this annex takes precedence. No other ADR-0038 section is accepted by implication. The authenticated artifact/shared-blob caching rule in §6 also explicitly amends ADR-0036 §5's generic public immutable-cache recommendation for responses governed by this reference authorization policy. Existing scope authority follows [the #2585 ruling](https://github.com/gerchowl/flock/issues/2585#issuecomment-6471599727); roles/resource grants are future work in [#2691](https://github.com/gerchowl/flock/issues/2691).

## 1. The single owner decision (Q1)

**Accept (Recommended)** the artifact contracts in ADR-0038 §§5 (artifact entries only), 7–7f and 9 (artifact boundary only), as amended here, including these exact defaults:

| Setting | Default |
|---|---|
| Publish lifetime | `session` |
| `artifacts.max_bytes` | 2 MiB decoded content per version |
| `artifacts.max_versions` | 50 non-purged versions per artifact |
| `artifacts.quota_bytes_per_agent` | 32 MiB logical content bytes |
| `artifacts.quota_bytes_total` | 2 GiB logical content bytes |
| `artifacts.idle_ttl` | 14 days |
| `blobs.session_backend` | `session_tmp` |
| `artifacts.allow_stream_grants` | `false`; enabling it does not implement grants |
| Viewing | 8 leases/principal, 30 s heartbeat, 90 s lapse, 8 h per lease; non-operator total budget 8 h/version |
| Future RPC limits | 20 requests/s/frame, 4 subscriptions/frame, 16 KiB/request, 64 KiB/result |
| Orphan grace | 24 h after a complete successful reference scan |
| List / comments page | 50 default, 100 maximum records; 256 KiB maximum encoded page |
| Snapshot comments | Latest 20 retained comments per artifact, within the same bounded paging envelope |
| Annotation budget | 16 MiB/publishing agent, 256 MiB/server; 1,000 retained comments/artifact |
| Artifact catalog budget | 10,000 artifact identities/server, including retained tombstones |
| Retention | 90 days for terminal unheld comments; 30 days for a fully purged artifact's tombstone after its last retained relation closes |
| Pressure reset | Re-arm 80% notice below 75%; re-arm 100% notice below 95% |

MiB and GiB are binary units. Positive numeric limits are validated on reload; lowering a limit blocks new growth and schedules eligible cleanup, never deletes held content. These defaults are operational configuration under ADR-0041 except fixed request-shape bounds in §2. Viewing and RPC defaults above remain the parent's specified bounds. **Cost:** shared reference tracking, bounded persistent metadata, migration, durable comment custody and clock-driven maintenance; G6 is ten small implementation slices, not one 400-line change. Reject accepting only the easy publish path: it would expose unbounded storage and ambiguous delivery before lifecycle/authority are implemented. The owner can accept this whole artifact amendment in one decision; until then the implementation gate stays closed.

## 2. Records, commands and outcomes (Q2)

**Recommended:** schema version 1, typed Rust records/params/results generated to TS. The durable log envelope retains its existing event identity/time/sequence; the artifact payload adds `schema: 1`. No caller supplies an author, node, session, timestamp, version number or queue producer. They are derived from authenticated authority. Unknown record kinds, newer schemas and unknown enum states are skipped and preserved raw under ADR-0028; a load never drops their blob references (see §3).

Common types and wire limits:

```text
ArtifactId = { origin: NodeId, id: UUID }       // server-chosen UUID, immutable
VersionRef = { artifact_id: ArtifactId, version: u32 }
BlobRef = { sha256: lower_hex_64, size: u64 }
ArtifactScope = { origin?: NodeId, agent_id?: AgentId, about?: EntityRef }
Lifetime = session | persistent
Kind = html | markdown | svg | image { mime: image/png | image/jpeg | image/webp } | json
Capability = { kind: flock_stream, stream: StreamId, access: read }
Author = existing attributed principal, attested by the server
Publisher = { agent_id: AgentId, session: existing stable agent identity }
Anchor = Block { block_id } | Text { block_id, quote: { exact, prefix?, suffix? }, position?: { start, end } }
       | Question { question_id, option_id? }
```

NodeId is the stable authenticated mesh node identity, never a hostname. EntityRef and StreamId reuse their existing typed vocabulary. Offsets count Unicode scalar values, end-exclusive, and are hints only. UTF-8 byte bounds: title 512, about's encoded form 2,048, block/option/set-local id 128, comment 16 KiB, exact quote 4 KiB, prefix/suffix 32 Unicode scalars and at most 128 bytes each. At most 16 capability declarations, no duplicate declarations, at most 8 KiB encoded declarations. Total encoded annotation payload at most 24 KiB. Strings must be valid UTF-8; escape terminal controls in text presentation. JSON content must parse, markdown/html/svg are untrusted source, image signature must match the declared MIME. No executable MIME fallback or external resource fetch. Inline content is UTF-8 for text kinds and strict base64 for images; the decoded size is capped before allocating it, and the outer request cap allows base64 overhead plus bounded metadata.

### 2.1 Durable records

Every payload below includes `schema: 1` and `artifact_id`. Unknown enum values have an `Unknown` decoding path; they do not mutate the projection. The parent dotted names here are logical event names; implementation maps them to the existing typed durable-log vocabulary and versions its readers before emit sites. Version numbers start at 1, increase without reuse and refuse overflow. Only the origin node authors artifact mutations.

| Logical event | Payload beyond the common fields |
|---|---|
| `artifact.published` | `version, publisher: Publisher, title, kind: Kind, content: BlobRef, interactive: bool, capabilities: [Capability], about?: EntityRef, lifetime: Lifetime, request_id: UUID` |
| `artifact.lifetime_changed` | `version, from: Lifetime, to: Lifetime, by: Author, request_id` |
| `artifact.pinned` / `artifact.unpinned` | `version, by: Author, request_id` (pin is idempotent, not a counter) |
| `artifact.activity` | `version, principal: Author, cause: view | comment | answer | hold_ended, at_ms`; compactable per-version/principal clock and non-operator viewing-budget summary |
| `artifact.comment` | `version, comment_id: UUID, author: Author, text: String, anchor_payload?: AnnotationRef, revision: sha256, delivery: pending, lifecycle_id: UUID, request_id` |
| `artifact.comment_delivery` | `comment_id, lifecycle_id, state: admitted | refused, error?: {code: ErrorCode, retry: Retry}` |
| `artifact.purged` | `version, reason: idle | session_end | quota | operator | blob_missing, by?: Author, request_id?: UUID, redactions?: [{original_annotation_id: UUID, replacement: AnnotationRef}]` |
| `artifact.metadata_expired` | `comment_ids: [UUID]` (at most 100), or `catalog_expired: true`; releases payloads and makes older read cursors explicitly expired |

Transactions use the log owner's complete-transaction framing: `{schema: 1, tx_id: UUID, records: [ArtifactEvent], receipt?: Receipt}`, capped at 512 KiB encoded and 100 records. `Receipt = {principal: Author, verb: ArtifactVerb, request_id: UUID, params_sha256, result: MutationResult, committed_sequence}`. Canonical params digest includes decoded content hash/size instead of a transient path spelling; a path replay must resolve/read the same bytes. A duplicate is matched only after the same validation/auth boundary. `MutationResult` is the tagged union of the mutation results in §2.2, not an arbitrary JSON success. A purge batch uses the same request receipt plus `{operation_id: UUID, next_version?: u32, complete: bool}` progress; its terminal receipt is retained. Pin/unpin/lifetime of a missing/expired version refuse, while repeat of the same committed operation returns its receipt. Whole-artifact purge is at most 50 retained versions, so its retained result fits the bound. If a lower transaction bound prevents the full operation, it uses explicit progress batches, never a false all-complete result.

`artifact.metadata_expired` uses exactly one tagged operation: `comments {ids}` or `catalog`; the comment-id array is at most 100. No metadata-expiry event deletes a pending outcome or receipt it still needs. `Author` uses the existing typed attributed principal representation, including token identity instead of raw bearer text. Enum definitions require future/unknown handling on persisted reads; unknown command input is a refusal rather than a persisted Unknown value.

`AnnotationRef = { id: UUID, version: VersionRef, size: u32, sha256 }` points to a version-private annotation sidecar containing the original Anchor. It is **not** shared by content hash, and contains the excerpt fields instead of placing excerpts in the immutable global log. Comments retain the original anchor/version; re-anchoring is a G7 view. This explicitly replaces §5's persisted `anchor_state`/re-anchored quote sketch. The logical record resolves the sidecar; after operator purge its redactions mapping selects a new sidecar/hash that returns `[purged]` for quote fields while preserving block/position association. Never verify redacted bytes against the original digest. A mapping contains at most 1,000 entries and must fit the transaction byte cap. See §5 for deletion semantics.

Question holds are derived from the existing question log's `about: {artifact_id, version, set_id, question_id, anchor_payload?}` and its close records. Do not write a second question truth. Pin, lifetime, purge and activity are mutable projection state over immutable versions; they never edit published content. Viewing leases themselves are in-memory and end at disconnect/restart; their consumed non-operator budgets and idle-clock advancement are durable before acknowledging renewal. No open lease survives a process that cannot renew it.

**Cost:** new typed log variants and durable activity summaries. Reject a single loosely typed JSON payload: it prevents safe evolution, bound checks and reference enumeration.

### 2.2 Public command schemas

Each mutation carries a required `request_id: UUID` in its params, including MCP. The wire `cmd_id` remains the transport command identity; the artifact request identity survives transport changes/restart. Reuse the existing command-id parsing/type, not a new credential. Deduplication is scoped to `(authenticated principal, verb, request_id)`; an equal request returns its original result, different canonical params give `artifact_request_conflict`. Authorization is checked even for replay. Persist a receipt with the mutation; definite refusals write no mutation/receipt. A response with unknown commit effect carries request_id, which callers must retry unchanged. Successful receipts survive at least as long as their subject's retained record; after catalog expiry a receipt high-water/tombstone entry rejects old request IDs with `artifact_request_expired` for 90 days. Beyond 90 days replay protection is not promised: clients must never automatically replay older requests. Persisted pending custody never ages out this way.

| Command / MCP name | Params (beyond request_id for mutations) | Result |
|---|---|---|
| `artifact.publish` / `flock_artifact_publish` | `title, kind, content?: String, path?: String, supersedes?: ArtifactId, about?: EntityRef, interactive?: bool=false, lifetime?: Lifetime=session, capabilities?: [Capability]=[]` | `{artifact_id, version, sha256}` |
| `artifact.list` / `flock_artifact_list` | `scope?: ArtifactScope, about?: EntityRef, mine?: bool=false, since?: Cursor, limit?: u16=50` | `{records: [ArtifactView], next?: Cursor, through: Cursor}` |
| `artifact.comments` / `flock_artifact_comments` | `artifact_id, since?: Cursor, limit?: u16=50` | `{comments: [CommentView], next?: Cursor, through: Cursor}` |
| `artifact.comment` | `artifact_id, version, anchor?: Anchor, text` | `{comment_id, lifecycle_id, delivery: pending | admitted}` |
| `artifact.lifetime` | `artifact_id, version, lifetime: Lifetime` | `{version: VersionRef, lifetime}` |
| `artifact.pin` / `artifact.unpin` | `artifact_id, version` | `{version: VersionRef, pinned: bool}` |
| `artifact.purge` / `flk artifact purge` | `artifact_id, version?: u32` (absent: all retained versions) | `{purged: [VersionRef]}` with bounded paged progress for all-version purge |
| `artifact.viewing` | `artifact_id, version, open: bool` | `{held: bool, lease_expires_ms?, budget_remaining_ms}` |

Viewing uses the authenticated connection identity, not an arbitrary caller lease id; close is idempotent, renewal uses the same pair. It does not require request_id or a permanent receipt. Whole-artifact purge reserves a durable operation id and resumes bounded batches; partial progress is returned explicitly until the last version's redaction completes. All params are closed typed objects: reject unknown mutation fields with invalid_params. Publish requires exactly one content/path. Supersedes reuses the artifact identity and publisher; only that same stable agent identity can add a version. Concurrent publishes serialize into distinct versions. Lifetime cannot transfer an artifact to another agent. Capability declarations require interactive=true, support only the declared v1 read-stream form, and grant no access.

Read shapes are explicit: `ArtifactView = {published: PublishedPayload, pinned: bool, effective_lifetime, state: available | expired {reason}, last_activity_ms}`. `CommentView = {comment: CommentPayload, anchor?: Anchor | redacted, delivery: pending | admitted | refused, error?}`. Read endpoints never expose backend paths, keys, raw unknown-record bytes or unauthenticated principals.

### 2.3 Outcome / retry matrix

All proposed names must be registered and pinned in `src/api/error_code.rs` before an emit site ships. Existing names keep their existing class. Error data contains the request/version/cursor identity when safe to disclose; unauthorized callers receive only the common authorization refusal. No error is an invitation to allocate a new request_id.

| Code | Retry | Defined boundary / recovery |
|---|---|---|
| `invalid_params`, `scope`, `not_authorized` | never | Shape, absent named scope or unattested authority; no mutation |
| `artifact_not_found` | never | Authorized read of unknown/expired catalog identity; no existence leak before auth |
| `artifact_owner_mismatch`, `artifact_request_conflict`, `artifact_request_expired` | never | Wrong publisher, altered replay, or obsolete replay receipt |
| `artifact_too_large`, `artifact_path_refused`, `artifact_kind_invalid` | never | Decoded bound, worktree/regular-file safety, invalid content/MIME |
| `artifact_version_limit` | after_human | No eligible version can be removed to stay within max_versions |
| `artifact_version_exhausted` | never | u32 version counter cannot advance; publish a new identity |
| `artifact_quota_exceeded` | after_human | Publisher content quota cannot be met without deleting held bytes |
| `artifact_total_quota_exceeded` | later | Total content quota cannot be met by publisher-only eligible eviction |
| `artifact_metadata_limit` | after_human | Annotation/catalog budget or 1,000-comment bound; operator raises the validated budget, or closes relations and waits for eligible retention |
| `artifact_expired` | never | Version's content removed; record remains inspectable |
| `artifact_cursor_expired` | never | Retained metadata no longer covers cursor; resnapshot using current reads |
| `cursor_ahead` | never | Cursor beyond current owner head |
| `artifact_recipient_gone` | never | Publishing identity ended/replaced before a new comment's custody commit |
| `artifact_storage_busy` | later | Bounded worker/GC maintenance capacity; no artifact commit |
| `artifact_storage_failed` | maybe | I/O may have committed; retry the same request_id and inspect receipt |
| `blob_migration_failed` | maybe | Triggering control did not commit; preserve source and retry same request |
| `artifact_remote_unavailable` | later | Origin node not local; no local mutation or local blob_missing purge |
| `artifact_grant_operator_only` | never | A non-operator attempted a future grant control |
| `capability_not_declared`, `capability_unsupported`, `rpc_too_large` | never | §7e; G6/G7 always answer RPC with capability_unsupported |
| `capability_not_granted` | after_human | Future grant absent for this viewer/version |
| `rpc_rate_limited` | later | Future broker capacity/rate bound |

Queue transient errors remain their registered classes **inside** custody recovery. They do not turn a committed comment into a synchronous refusal. After comment commit the response is a successful pending receipt or artifact_storage_failed with the request_id if the response's effect is unknown. A permanent later admission refusal is visible in CommentView and the existing operator notification surface, not silently logged. Pin/lifetime controls that fail migration have no acknowledged control effect.

## 3. Shared storage, reference accounting and crash recovery (Q3)

**Recommended:** one server-owned blob coordinator for every blob user, using existing content-only BlobStore operations behind a backend interface. No artifact-only unlink path. Durable records and retained stream/history checkpoints are the truth; a persisted reference index is a rebuildable acceleration and includes a scan generation plus durable source cursors. It is not an independently authoritative refcount.

A reference key is `(source-kind, origin, durable-record-id, field)` with hash, size and storage class. The maintenance journal also has schema 1 records: `MigrationIntent {operation_id, hash, size, source: {backend, session}, destination: data_dir, triggering_request_id}`, `MigrationComplete {operation_id}`, and `GcCandidate {hash, scan_generation, first_zero_ms}` / `GcCancelled {hash, scan_generation}`. The owner manifest is `{schema: 1, stable_session_id, backend, relative_root}`; the path must resolve within the trusted server-managed scratch root. Index entries carry `{reference_key, hash, size, storage_class, source_cursor}` and index checkpoints carry `{schema: 1, generation, complete_source_cursors}`. All of these are server-private recovery formats, not new mesh custody markers or authoritative artifact records. Unknown maintenance schemas disable destructive work until supported. The backend supplies bounded put/read/copy/remove operations; the coordinator serializes reference transitions and GC under the same ownership fence. Any persisted consumer must provide a reference enumerator before deletion is enabled. Unknown/newer records, incomplete scans, corrupt log segments, unregistered consumers and unverified checkpoints make collection fail closed for the affected namespace; do not guess that no references exist. Retry after inspection, report pressure to the operator. Index rebuilding cannot serve new destructive maintenance until all registered sources reach the same captured barrier.

Layout: durable content remains `<data dir>/blobs/<sha256>`. Session scratch resides under the server-managed `<data dir>/artifact-sessions/<stable-session-id>/`, mode 0700, with blobs by hash and private staging files. The name session_tmp means session-lifetime scratch, **not** an OS process-temp directory or an RAII auto-delete directory. Its root and owner manifest survive process restart and handoff. Backend selection is restart-only and cannot strand existing references: recovery opens each manifest's recorded backend until migration completes. No backend path is trusted from an agent record. Session manifests map stable logical session identities to locations; never infer identity from a recycled PID or pane placement.

Commit protocol:

1. Reserve quota/catalog space and an operation under the single owner; no irreversible eviction until input validation/auth succeeds. Pin opened input handles, bound reads, hash and fsync complete staged bytes; publish by atomic rename then fsync directory. Publication and later record commit both re-check the reserved owner generation.
2. Append/fsync the immutable published record and its request receipt, then expose it through the read model/stream and acknowledge. A pre-record blob is an orphan, not a published artifact. Multiple records for identical content have separate logical references.
3. Adding a durable reference to a temp blob first writes/fsyncs a migration intent in the coordinator maintenance journal. Copy to durable staging, fsync, verify hash, publish/fsync durable location. Append the triggering pin/lifetime/question transaction only after durable bytes exist; register its durable reference, then release the temp reference. The intent names operation, hash, source backend and destination, not a credential. Recovery checks the triggering durable record: finish committed migration or retain the intact source and discard the uncommitted intent. There is no auto-commit of a failed pin. Question publication uses this same prerequisite so an open question never points only at sweepable bytes.
4. Deletion requires a complete reference scan and zero committed references, zero staged write/migration references and zero active read handles. Mark candidate with generation, hash and first-zero time. Wait 24 h after that successful scan, then recheck under the same coordinator ownership immediately before unlink/fsync directory. Adding a reference cancels candidacy. Recent staged files and unknown ownership never become eligible merely because a filename looks old.
5. Purge first commits its tombstone/ref release, then schedules removal. Recovery can repeat ref release without decrementing twice because reference keys are sets. For operator excerpt redaction, acknowledgment additionally waits for the redaction deletion fence in §5. A failure is typed and its request receipt reports partial/pending completion; it does not claim erasure.

Opened blob reads hold a coordinator read lease until the response is finished/cancelled. Expiry may publish a tombstone while a read already in progress completes, but physical deletion waits. New reads after tombstone refuse. Handoff passes the existing durable fence, drains or explicitly cancels old read leases, and installs one coordinator; no parallel GC owner. All disk work is bounded on an I/O worker; generation-fenced installation also rechecks the current owner. A stopped producer or stale worker cannot install an obsolete index. A logical session restart preserves its scratch; identity end writes session_end tombstones only for unheld versions. A held ended-session blob stays until hold end plus idle_ttl, or migrates for pin/question/persistent reference.

**Cost:** consumer enumeration, a rebuildable index, tiny recoverable maintenance intents, serialized read/delete leases and delayed reclaim. Reject raw filesystem age scans and artifact-local refcounts: both can delete bytes held by streams or an in-progress migration. Reject OS temp cleanup: it destroys live logical sessions during process restarts. A 24 h grace is not a promise that unknown references become safe after 24 h.

## 4. Version cap and quota eligibility (Q4)

**Recommended:** max_versions counts non-purged versions, including the current version. Before committing version 51, select the oldest eligible version by `(last_activity_ms, version)`, within this artifact and this publisher only. A version is eligible if it is unpinned, has no active question/view/read/migration hold, and no pending comment custody. Old superseded versions are idle candidates immediately when all holds end; the new/current version of a still-running session is not automatically quota-evictable. Persistent or ended-session versions are idle candidates after the hold-ending activity timestamp; quota eviction need not wait 14 days. TTL expiration still waits the full idle_ttl. A superseding publish may reserve the old current version as its candidate, but purges it only in the same successful owner transaction as the new version. Failed publishes never remove it.

If 50 versions are held or otherwise ineligible, refuse with artifact_version_limit/after_human, do not make version 51 or silently unpin. Version numbers never reset after purge. Then enforce publisher content quota and total quota using only that publisher's eligible versions. Preflight all three constraints before committing the selected purge set: a total-quota refusal must not leave opportunistic deletions from a failed publish. The durable transaction envelope records bounded purge choices and publish/receipt atomically. Recovery either replays that complete transaction or skips a torn final transaction.

Count **logical bytes per non-purged version**, even when bytes deduplicate physically or a stream also references them. Holds consume quota. Shared stream bytes do not consume artifact quota but still prevent physical deletion. This predictable conservative accounting deliberately does not promise a physical-disk ceiling: streams, orphan grace and staging also use disk. Reserve bounded staging capacity and refuse before exhausting the disk reserve.

**Cost:** durable composite mutation and conservative quota consumption; at most 50 versions inspected per artifact, publisher eviction uses an index with bounded worker pages. Reject treating dedup as free quota: one agent could otherwise evade its limit. Reject forced eviction of held versions or another publisher's content: it violates the owner's keep/hold rules.

## 5. Bounded reads, retention and purge privacy (Q5)

**Recommended:** use opaque typed cursors bound to endpoint, canonical filter, principal, origin, scan generation and captured through sequence. No time-only since cursor and no offset paging. First page captures the finite through barrier; continuation never expands it. Subsequent fresh reads with since=that terminal cursor return strictly later records up to a new barrier. Return records ordered by durable artifact stream sequence, then stable record id for snapshot ties. Comments are ordered by their creation sequence; delivery changes appear as upserts in the stream. Artifact list is a paged current-state snapshot at its barrier, followed by changes from its returned terminal cursor. Changing filters/principal requires a fresh cursor. Cursors beyond the head give cursor_ahead; invalid binding gives invalid_params; expired generations give artifact_cursor_expired, never an empty success that hides a gap.

Default page 50, max 100 records and 256 KiB encoded bytes, stopping at whichever bound comes first. A single record must fit the page cap. Snapshot pages use the same caps, and at most 20 latest comments/artifact; comments outside that window are discoverable by the comments endpoint while retained. Use an indexed/seeked iterator with buffered continuation; never reread/discard pages or rescan from the start. Replay/live registration and capture happen in one critical section; the wire uses existing explicit gap/snapshot_to/synced/lagged semantics. Keep an immutable cut via shared segments, not a whole-catalog copy for every new event. Hold at most four live update cuts and 1 MiB queued encoded data per subscriber; the initial replay cut does not consume those four slots. If either live bound is exceeded, discard queued replay/live work and close explicitly lagged; client resumes from its last applied cursor.

The opaque cursor is a tagged union internally: `Position {filter_digest, principal_id, origin, sequence, retention_generation}` for a completed read, and `Page {position_binding, cut_id, through, next_key, expires_ms}` for a continuation. Server-issued cursor integrity prevents caller alterations; it grants no authority and current auth is still required. Reusing Position captures a fresh finite cut; Page continues only its original cut. A page cut has a 5 minute maximum lifetime. On expiry release its read pins and return artifact_cursor_expired on continuation. A metadata mutation that must redact content invalidates any cut referencing it; operator purge waits for active page encoding/read leases, so a newly served page cannot leak a stale excerpt after purge ack. A connected client may already hold text; the server cannot erase that client memory, transcripts, screenshots, backups or separately copied content. This is not a global secure-erasure feature.

The annotation quotas and catalog/comment-count caps are reloadable operator configuration (`artifacts.quota_annotation_bytes_per_agent`, `artifacts.quota_annotation_bytes_total`, `artifacts.max_comments`, `artifacts.max_catalog_entries`), with the defaults in §1. Raising these is explicit capacity allocation; never disguise deletion of retained comments as content purge. Annotation accounting includes comment text, original anchor sidecar, about metadata, request receipts and lifecycle/delivery overhead at encoded size. Reserve worst-case terminal-outcome space with every comment. Per-agent 16 MiB, total 256 MiB, max 1,000 retained comments/artifact and max 10,000 catalog identities/server bound growth; a pending delivery/hold/pin cannot be evicted for room. Comments become retention-eligible 90 days after terminal custody, only when no open question/thread references them. Fully purged artifact catalog entries expire 30 days after their last retained relation closes; pins and open relations prevent expiry. Expiry records advance the retention floor before payload compaction; old read cursors then fail explicitly. Durable segment compaction follows the log owner's existing crash-safe checkpoint/rename/fsync protocol; never truncate an unrelated global-log user. If shared-log compaction cannot reclaim space yet, physical storage remains visible and admission refuses at its disk budget. Counts/byte limits are enforced even when a relation remains open forever: new growth refuses rather than silently discarding it.

An operator purge first writes/fsyncs new redacted sidecars and commits their original-id→replacement mappings in the purge transaction; only then may it remove the old sidecars. It acknowledges redaction only after syncing those replacements/directories, removing old sidecars/staging copies and fencing existing readers. The mapping preserves the new digest, so a redacted payload never masquerades as its old hash. The log contains AnnotationRef, **not** original quoted text, so redaction does not require rewriting global log history. Sidecars are never deduplicated across versions. Crash recovery consults committed purge tombstones before serving any sidecar and completes redaction before reporting the purge complete. Missing/redacted sidecar yields the explicit redacted anchor; it cannot resurrect an excerpt from a log. Automatic purge keeps sidecars until normal annotation retention. Free-form comment text is not classified as an anchor excerpt; copied prose in a comment, an agent transcript or another artifact is outside this targeted purge. This narrows the parent's “no text survives” sentence to server-held anchor excerpt fields, not all copies of the content.

Pressure notices use one existing operator notification identity per threshold/storage class: update the largest-holder summary, do not enqueue a notice per failed publish. Emit at upward crossing 80% and 100% (including would-exceed refusal); re-arm only below 75% and 95% respectively. Persist armed/disarmed state across restart. The notice action previews eligible idle unpinned candidates and requires explicit authorized purge; it never removes held versions in bulk.

**Cost:** paged cuts, annotated sidecars, deletion fences and catalog/receipt compaction. Reject unbounded list/comments results, lossy since timestamps, and log-only excerpt storage: the first two hide loss or exhaust memory, the last cannot honor targeted excerpt purge without rewriting unrelated history. Durable log retention alone is not an admission budget.

## 6. Visibility, scopes and blob reads (Q6)

**Recommended:** local artifact visibility is node-wide for a credential explicitly holding read; ArtifactScope is a **filter**, never a permission grant. G6 does not implement project-private artifacts, roles, per-agent client-token restrictions or resource delegation. Those require #2691. The UI and docs must warn publishers that artifacts are visible to readers authorized on that node. origin is local-only in G6; another origin returns artifact_remote_unavailable, not a fake empty list.

| Action | Required existing authority |
|---|---|
| Agent publish/supersede/lifetime | Existing pty/structured attestation for that same stable publishing identity; no token-origin publishing as an agent |
| Agent MCP list/comments | Attested agent, its own artifacts only; mine=false cannot widen this grant |
| Client list/comments/artifacts stream/blob content | read, independent of send/answer/operate; no scope is inferred from another |
| Client artifact.comment | send only, attributed to that credential's own principal under #2585; no read requirement for admission, and response exposes only that command's result |
| Client pin/unpin/lifetime | answer **or** operate; a pin is a keep/disposition action, not a permission answer or agent-lifecycle action |
| Local CLI pin/unpin/lifetime | Kernel-attested local operator; exclude pane ancestry and relay edges |
| Explicit operator purge | operate or kernel-attested local operator |
| Client viewing | read; operator exception to the 8 h aggregate budget only for the already-defined operate principal |
| Future live-data grant | operate; G6/G7 refuse as capability_unsupported before allocating a grant |

Scopes never imply one another: operate alone cannot read/comment; send alone cannot pin, answer a question or operate; answer alone cannot read/comment or purge. A bearer with answer may pin even when its process is an agent: **there is no reliable role distinction in current credentials**. The parent's “agents cannot pin” means attested agent/MCP authority alone does not authorize pin; explicitly delegating answer or operate grants that named capability. This is an explicit amendment of that ambiguous sentence, not a speculative role check. An operate token remains the existing operator authority for its named controls; #2691 may later attenuate it. Do not silently add operate to send, or require operator identity to use answer.

Authenticated read to GET /v1/blob/hash must prove at least one currently visible live reference through the blob coordinator's reverse index, including ordinary stream references authorized by the existing read policy. Knowing a hash, a tombstoned reference or an uncommitted staging file is insufficient. Use the same policy for range/HEAD/conditional requests, and recheck on every request; no credential enters a frame or blob URL. Preserve existing immutable ETag/content semantics but replace public caching with **private, no-store** for authenticated artifact delivery: logout/purge must not hand another principal a shared cached body. Range and conditional requests do not bypass the tombstone/auth fence. G6 must integrate this check with the shared blob route before exposing artifacts; it must not accidentally revoke legitimate stream-line references. HTTP denial follows the existing common authorization envelope, not a hash-existence oracle.

New comments require an available retained version and the same live publishing identity at the durable custody boundary. Identity end/replacement gives artifact_recipient_gone before commit; restart/handoff of the same identity does not. Permission/read checks occur before any artifact existence detail; an authenticated send-only action can still receive its action-specific typed refusal. Supersession never transfers ownership, even with operate. Remote-control routing is deferred to #1380, not inferred from an origin id.

**Cost:** reference-aware shared blob authorization and an attributed token queue principal (depends on #2585 implementation). Reject guessing agent roles from ancestry of a bearer request or silently upgrading it to operator. Reject project ACLs now: no accepted resource-constraint model exists. Reject public blob caching: a content hash is not a bearer authorization grant.

## 7. Comment custody and recovery (Q7)

**Recommended:** durable outbox, not a two-writer best-effort call. Before committing a comment, validate principal/send authority, version/recipient identity, payload and metadata budgets; reserve its lifecycle/outcome space. The single artifact owner appends the comment plus request receipt atomically, with server-assigned comment_id and a stable derived lifecycle_id in the existing queue UUID namespace. Its delivery begins pending. A reply acknowledges **durable custody**, not stdin delivery: pending is a successful explicit state. The same request id returns the same identities and current delivery state. A sender must not generate a new request id to retry a lost reply.

The existing single queue owner receives that exact lifecycle id and the canonical content digest/version/publisher from the outbox. Digest canonicalization excludes the randomized framing delimiter; persist/reuse the admitted rendered payload rather than generating a changed request on recovery. Frame every untrusted field with the established unforgeable per-item delimiter; a token's slash text is always a framed message, never an operator command. If the queue owner reports its previously committed identity/digest, record admitted rather than enqueueing again. A key with different content is a real conflict and must not overwrite committed content. A matching committed admission wins over any later transient/refusal result. The artifact owner consults the queue's durable committed-key lookup **before** writing refused, including after a conflict.

Crash cases:

| Boundary | Recovery |
|---|---|
| Before comment transaction commits | No custody, no delivery; same request may safely retry |
| After commit, before enqueue | Replay pending outbox and retry exact lifecycle id |
| Queue committed, outcome record absent | Owner's committed lookup returns same item; write admitted, no new enqueue |
| Unknown write effect | Keep pending; query exact committed key then retry on bounded backoff, never generate a new key |
| Deterministic refusal with no committed admission | Write refused with the registered code, settle outbox and publish existing operator-visible refusal notification |
| Outcome committed | Restart reads admitted/refused and never re-enqueues |

Queue fullness/storage unavailable stays pending and retries at the existing bounded backoff (initial 100 ms, doubling to max 30 s for this outbox, no finite attempt limit while custody remains valid). Earliest due retry participates in the real event-loop deadline, even on a headless quiet server. Retry worker capacity is bounded (32 active operations, 128 pending in-memory heads; remaining durable entries are cursor-paged); stop/cancel and unexpected worker completion must not hang recovery. A permanent identity replacement becomes refused only after committed-key reconciliation. No pending entry expires automatically. Existing admission-side input_write_unknown never becomes a false refused custody outcome. Once admitted, later session failure is queue lifecycle state, not a reason to re-deliver the comment.

**Cost:** durable replay index, exact committed-key lookup and deadline integration. Reject enqueue-before-log (orphan delivery), log-then-return-session_busy (sender retries a hidden custody item), and optimistic refused settlement (it can revoke an admission already committed). This reuses queue ownership and lifecycle ids, not the message mesh custody format or a second stdin writer.

## 8. Origin identity and G11 boundary (Q8)

**Recommended:** origin-qualified ArtifactId from first publication, including local calls. NodeId is the mesh's stable authenticated identity, not an advertised hostname. VersionRef, about/anchor references and request receipts preserve origin. Reinstalling under a new node identity creates a new namespace; importing a backup does not impersonate the old node without the existing identity recovery procedure.

G6 serves locally authored records/blobs only and refuses another origin with artifact_remote_unavailable/later. It neither ingests unverified remote records nor interprets their missing bytes as blob_missing. **Carriage is deferred to the existing issue [#1602 (G11)](https://github.com/gerchowl/flock/issues/1602)**, with remote commands still dependent on #1380. No mesh VERSION change, new message format or marker is authorized by this amendment.

G11 must specify authenticated origin/custody, origin-only mutation authority, duplicate/version ordering, pending-transfer presentation, digest verification and bounded fetch, remote hold/pin propagation, purge precedence and GC ownership. Until both record and verified bytes have arrived, a future received version is transfer-pending, not available or locally purged. That future state must not leak into G6 as an empty success. Accepting origin-qualified ids now does not accept a carriage protocol.

**Cost:** qualified ids in all generated contracts from day one. Reject retrofitting origin later: ids already persisted in comments/questions/cursors would become ambiguous. Reject hostname prefixes and treating content hashes as publisher identity.

## 9. Ownership and release boundary (Q9)

**Recommended:** G6/#1598 owns the artifact owner, durable records/receipts, backend coordination/reference accounting, bounds/configuration, lifecycle/leases, pin/lifetime/purge CLI/socket/wire controls, metadata/pressure/refusal notifications, question-hold observer, comment custody, MCP publish/list/comments, artifact stream and TUI text fallback. It provides one typed service, and all callers use it. No GUI-private files or second input path.

G7/#1599 owns iframe sandbox/CSP and shell security tests, trusted untrusted-content chrome, safe rendering/MIME conversion, selection/quote generation and cross-version re-anchoring views, viewing renewal integration, pin/purge buttons, question sets/request_feedback tools, feedback confirmation and fork/merge-back UX. It calls G6 for version existence/migration/hold prerequisites before publishing a question. A purge invalidates G7's retained rendering and removes its frame; the kept question remains answerable with expired content.

Both ship **default deny** for live data. G6 accepts bounded immutable flock_stream declarations but issues no grants. G7 answers every flock.rpc with capability_unsupported and never supplies a token; allow_stream_grants=true alone cannot bypass this. Actual grants, revocations and broker limits are reserved shapes in the parent §7e and land only with a separately reviewed live-data implementation; external origins and declarative bindings are not defined here. Interactive scripts still have the disclosed navigation/WebRTC exfiltration residual; no test claims those channels are closed. Configuration inventories remain ADR-0041 work, but implementing these new keys and reload semantics is G6's responsibility.

**Cost:** cross-component question/hold and viewing contracts must land before enabling the matching UI. Reject leaving purge/holds as a GUI-only concern: the server must enforce them without a connected GUI. Reject silently pulling G11 or role administration into G6.

## 10. Pre-mortem

| Failure we expect if the boundary is wrong | Prevention / fail-first evidence |
|---|---|
| Restart empties session scratch despite a live logical identity | Stable-session manifest survives real restart/handoff; temporary process directory is not the backend |
| A collector unlinks a tool-result blob shared with an artifact | Complete all-consumer scan, exact ref keys, read/migration fences; API test purges one ref and still reads the other |
| A crash acknowledges a pin whose bytes never reached durable storage | Inject failure at intent/copy/fsync/verify/record steps; pin never acknowledges until durable bytes and record agree |
| Quota refusal first destroys an older report | One bounded publish/eviction transaction and preflight; public refusal leaves the old version readable |
| A quiet server never delivers pending comments | Real-loop clock/deadline test with no external wake; unknown-effect recovery reconciles exact committed key |
| Retry/restart produces two comments or two queue items | Canonical request receipt + stable lifecycle id, crash-after-commit public test and changed-body conflict test |
| Purge appears successful but history pages reveal excerpts | No excerpts in global-log payloads, version-private sidecars, cut invalidation/read fences; resume old cursor after purge cannot retrieve an excerpt |
| An agent keeps data alive by renewing read leases forever | Durable per-principal/version lifetime budget; injected-clock re-lease past 8 h neither holds nor resets idle clock |
| Read-only or send-only token becomes operator | Independent scope matrix via dispatcher, token-origin framing, no role inference; answer can pin only because that named disposition is explicitly granted |
| Ring overflow or paging silently loses a comment | Captured cursor bounds, atomic capture/registration mutation witness, explicit cursor-expired/lagged and snapshot recovery |
| An artifact hash becomes a public download capability | Auth on GET/HEAD/range/conditional paths and private no-store; unknown hash and denied reference have common auth behavior |
| Metadata fills disk although content is under 2 GiB | Separate annotation/catalog limits, disk staging reserve, retained pending custody blocks growth instead of being deleted |
| Remote transfer delay becomes an irreversible purge | G6 rejects nonlocal origin; G11 owns pending-transfer semantics in #1602 |

## 11. Ordered G6 slices and public-path acceptance

Retain the ten-slice plan; this section replaces its open Q2–Q9 references. Every estimate is **at most 400 production lines**, Rust plus TS; tests/docs excluded. If a slice exceeds the budget, propose a smaller ordered split before a PR. No dead public command or weakened default may ship between slices: new artifact capability advertisement remains off until its prerequisites are present, with explicit unsupported refusal. Shared infrastructure slices must exercise existing live blob/stream paths. Owner acceptance of this annex is a prerequisite, not an implementation permission inferred from this draft.

| Order / budget | Scope and normative contract | Fail-first public-path acceptance |
|---|---|---|
| 1 / 300–390 | Shared backend/ref coordinator, §3 | Existing blob/stream API shares bytes, one ref removal preserves another, unknown consumer fails GC closed, restart recovers references and bounded read failure is typed |
| 2 / 300–390 | Durable artifact owner/publish ingress, §2 | Attested MCP/socket publish returns stable id/version/hash; altered replay, forged author, bad MIME, overflow, symlink escape and pinned-handle race refuse without mutation; inject record/fsync failure |
| 3 / 280–380 | Session backend/migration, §3 | Real pin/question/lifetime transaction migrates before ack; fail copy/hash/fsync and crash replay, source stays whole, concurrent sweep skips intent |
| 4 / 300–390 | Lifecycle/viewing holds, §§1/3 | Stop purges, same-identity restart/handoff survives, open question/view/pin holds, injected loop clock checks post-hold TTL, 90 s lapse, eight-lease cap and non-operator budget |
| 5 / 280–380 | Quotas/version/config, §§1/4 | Publish exact boundaries, version 51 held refusal, publisher-only oldest eligible eviction, atomic failed preflight, reload lowering, quota classes, 80/100% notice reset across restart |
| 6 / 260–370 | Pin/purge/redaction/missing-content, §§3/5/6 | Dispatcher scope matrix, live read held across purge, redaction after restart, old cut cannot reveal excerpts, automatic purge preserves them, shared hash remains for another live ref |
| 7 / 180–300 | MCP list/comments and metadata retention, §§2/5/6 | Real MCP mine/filter/cursor pages retain order, byte/read bounds and no rescan; unauthorized read and expired cursor refuse, unknown persisted payload preserved, catalog/annotation budget blocks growth |
| 8 / 300–390 | Durable comment custody, §7 | Actual artifact.comment at queue gate, exact-key replay after crash-before/after-enqueue, unknown-effect quiet-loop retry, permanent refusal operator-visible, changed-body request conflict never duplicates |
| 9 / 280–390 | Artifacts provider/generated wire, §§2/5/6 | Real WS atomic replay/live registration with deterministic split-lock mutation failure, sparse filtered sequences, gap snapshot and quiet reconnect, lagged close, scope denial and TS drift |
| 10 / 180–300 | TUI fallback/default deny, §§8/9 | Isolated TUI renders escaped title/version/size/open hint/comments/expired status; capability metadata bounded, no grant allocation. G7 supplies frame source/token/CSP/RPC-unsupported browser tests |

Each new acceptance behavior must be shown red once against the missing change or a precise regression mutation, then green through the real public ingress. Use focused nextest, retries 0, bounded condition waits and producer liveness; no sleeps or widened timeouts. Clock-driven tests advance the existing injected clock and real deadline. Mutation proofs must fail deterministically without racing test threads or product-only test hooks. Slice 9 also bounds bytes read and verifies resume does not rescan. These are future implementation tests; this PR changes design documentation only.
