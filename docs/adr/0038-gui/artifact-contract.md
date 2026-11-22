# ADR-0038 artifact amendment: bounded hosting and custody

- Status: **Accepted, 2026-10-29** (#1598, #1590, PR #2729).
- Date: 2026-10-29; owner acceptance includes the two-tier size model.
- Parent: [ADR-0038 §§5, 7–7f, 9 and decision 17](../0038-gui-workbench.md).
- Replaces the unresolved Q2–Q9 in the [G6
  plan](https://github.com/gerchowl/flock/issues/1598#issuecomment-6482890895).
  These are accepted decisions, not implemented behavior.

This annex is normative. The owner accepted all recommended artifact options on
2026-10-29 with the typed decision “accept all rec with star”, recorded on PR
#2729. It includes the two-tier size model below. Where the parent artifact
sketches differ, this annex takes precedence. Acceptance records the design, not
delivery. The authenticated artifact/shared-blob caching rule in §6 also amends
ADR-0036 §5's generic public immutable-cache recommendation for responses
governed by this reference policy. Scope authority follows #2585; roles/resource
grants remain future work in #2691.

## 1. Owner decision: accepted on 2026-10-29 (Q1)

**Decided, 2026-10-29: accept** the artifact contracts in ADR-0038 §§5 (artifact
entries only), 7–7f and 9 (artifact boundary only), as amended here, including
the shared-GC decision tracked by
[#1712](https://github.com/gerchowl/flock/issues/1712), and these exact
defaults:

| Setting | Default |
|---|---|
| Publish lifetime | `session` |
| `artifacts.max_bytes` | 8 MiB decoded content per version; standard |
| `artifacts.max_bytes_escalated` | 64 MiB decoded content per version |
| `artifacts.escalations_per_agent_per_hour` | 2 committed escalated publishes |
| `artifacts.outer_request_max_bytes` | 1 MiB encoded JSON request, within the existing socket limit |
| `artifacts.inline_max_bytes` | 64 KiB decoded content; larger content requires path |
| `artifacts.inflight_upload_bytes` | 64 MiB global active-upload memory reservations |
| `artifacts.upload_reservation_bytes` | 8 MiB fixed accounted memory per active publish, independent of file size |
| `artifacts.upload_buffer_bytes` | 64 KiB maximum streaming copy/hash buffer |
| `artifacts.max_oversize_reason_bytes` | 280 UTF-8 bytes, nonempty |
| `artifacts.max_versions` | 50 non-purged versions per artifact |
| `artifacts.quota_bytes_per_agent` | 256 MiB logical content bytes |
| `artifacts.quota_bytes_total` | 4 GiB logical content bytes |
| `artifacts.quota_annotation_bytes_per_principal` | 16 MiB per authenticated client principal, including operator-authored annotations |
| `artifacts.idle_ttl` | 14 days |
| `blobs.session_backend` | `session_tmp` |
| `artifacts.allow_stream_grants` | `false`; enabling it does not implement grants |
| `artifacts.viewing_max_leases` | 8 per principal |
| `artifacts.viewing_heartbeat` / `artifacts.viewing_lapse` | 30 s / 90 s |
| `artifacts.viewing_lease_max` / `artifacts.viewing_non_operator_budget` | 8 h / 8 h per version |
| `artifacts.rpc_rate_per_second` / `artifacts.rpc_max_subscriptions` | 20 / 4 per frame, reserved for the future broker |
| `artifacts.rpc_max_request_bytes` / `artifacts.rpc_max_result_bytes` | 16 KiB / 64 KiB |
| `blobs.gc_orphan_grace` | 24 h after a complete successful reference scan |
| `blobs.fence_wait` | 100 ms maximum wait for a request writer to acquire the publication lock; stream pumps use the nonfatal inline path below |
| `blobs.gc_exclusive_budget` / `blobs.gc_batch_candidates` | 5 ms / 1 candidate per exclusive claim section |
| `blobs.gc_yield_interval` | 100 ms minimum between exclusive claim attempts; no batch-long lock |
| `artifacts.page_default_records` / `artifacts.page_max_records` | 50 / 100 |
| `artifacts.page_max_bytes` / `artifacts.snapshot_comments` | 256 KiB / latest 20 per artifact |
| `artifacts.live_max_cuts` / `artifacts.live_max_bytes` | 4 cuts / 1 MiB queued encoded data per subscriber |
| `artifacts.quota_annotation_bytes_per_agent` / `artifacts.quota_annotation_bytes_total` | 16 MiB / 256 MiB |
| `artifacts.max_comments` / `artifacts.max_catalog_entries` | 1,000 per artifact / 10,000 per server including retained tombstones |
| `artifacts.comment_retention` / `artifacts.tombstone_retention` | 90 d after terminal unheld custody / 30 d after a fully purged artifact's last relation closes |
| `artifacts.receipt_retention` / `artifacts.page_cut_ttl` | 90 d after catalog expiry / 5 min |
| `artifacts.pressure_warn_percent` / `artifacts.pressure_full_percent` | 80 / 100 |
| `artifacts.pressure_warn_rearm_percent` / `artifacts.pressure_full_rearm_percent` | 75 / 95 |
| `artifacts.max_title_bytes` / `artifacts.max_about_bytes` / `artifacts.max_anchor_id_bytes` | 512 / 2,048 / 128 |
| `artifacts.max_comment_bytes` / `artifacts.max_quote_bytes` | 16 KiB / 4 KiB |
| `artifacts.max_quote_context_chars` / `artifacts.max_quote_context_bytes` | 32 Unicode scalars / 128 UTF-8 bytes per prefix or suffix |
| `artifacts.max_capabilities` / `artifacts.max_capabilities_bytes` | 16 / 8 KiB |
| `artifacts.max_annotation_bytes` | 24 KiB |
| `artifacts.max_transaction_bytes` / `artifacts.max_transaction_records` | 512 KiB / 100 |
| `artifacts.max_expiry_ids` | 100 per metadata-expiry event |
| `artifacts.comment_retry_initial` / `artifacts.comment_retry_max` | 100 ms / 30 s |
| `artifacts.comment_max_active` / `artifacts.comment_max_heads` | 32 / 128 |

MiB and GiB are binary units. These keys join ADR-0041's inventory and
[ADR-0042's scoped schema proposal
(#2724)](https://github.com/gerchowl/flock/pull/2724). **Every `artifacts.*` key
and the `blobs.*` keys above is operator-owned, global/node scope only. None is
settable from project `.flk.toml`, agent configuration, a repository overlay or
agent-supplied env/flags.** Reject such an assignment with the configuration
surface's typed scope refusal; do not silently accept or merge it.
Operator-controlled file/env/flags/runtime layers retain their documented
precedence and audit rules. No repository may raise its own quota, extend
retention, enlarge RPC bounds or change the GC fence. `blobs.session_backend` is
restart-only; the numeric operational keys are server-reloadable. Changes to
wire shape bounds apply to new admissions without invalidating already committed
records; advertised negotiated limits must describe the active values. Lowering
budgets never deletes held content, and a cut keeps its captured expiry.
Positive limits, max_oversize_reason_bytes ≤ 280, inline_max_bytes ≤ max_bytes ≤
max_bytes_escalated, and encoded inline content plus bounded metadata fitting
outer_request_max_bytes are validated atomically. outer_request_max_bytes may
not exceed the transport's existing 1 MiB MAX_INITIAL_REQUEST_BYTES: increasing
the decoded file cap does not increase the JSON limit. Validate
upload_buffer_bytes ≤ upload_reservation_bytes ≤ inflight_upload_bytes; the
reservation must cover the implementation's measured maximum live
request/parse/metadata/staging allocations, not just the copy buffer. Reject a
reload that makes that bound untrue. gc_batch_candidates stays 1; default-page ≤
max-page, initial retry ≤ max retry, warning < full and re-arm < trigger are
cross-field invariants. Durations above use the common typed duration syntax,
not a free-form unit parser.

**Cost:** shared reference tracking across all writer paths/processes,
authenticated blob hosting, bounded persistent metadata, migration, durable
custody and maintenance. The revised plan has twelve slices: the old shared
foundation is split into 1a/1b and the missing HTTP route is explicit 1c. Every
slice remains ≤400 production lines. Reject accepting only the easy publish
path: it exposes storage before lifecycle/authority are safe. The owner accepted
this amendment and its defaults on 2026-10-29; implementation still follows the
named dependency gates and slice acceptance tests.

## 2. Records, commands and outcomes (Q2)

**Decided, 2026-10-29:** schema version 1, typed Rust records/params/results
generated to TS. The durable log envelope retains its existing event
identity/time/sequence; the artifact payload adds `schema: 1`. No caller
supplies an author, node, session, timestamp, version number or queue producer.
They are derived from authenticated authority. Unknown record kinds, newer
schemas and unknown enum states are skipped and preserved raw under ADR-0028; a
load never drops their blob references (see §3).

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

NodeId is the stable authenticated mesh node identity, never a hostname.
EntityRef and StreamId reuse their existing typed vocabulary. Offsets count
Unicode scalar values, end-exclusive, and are hints only. UTF-8 byte bounds:
title 512, about's encoded form 2,048, block/option/set-local id 128, comment 16
KiB, exact quote 4 KiB, prefix/suffix 32 Unicode scalars and at most 128 bytes
each. At most 16 capability declarations, no duplicate declarations, at most 8
KiB encoded declarations. Total encoded annotation payload at most 24 KiB.
Strings must be valid UTF-8; escape terminal controls in text presentation. JSON
content must parse, markdown/html/svg are untrusted source, image signature must
match the declared MIME. No executable MIME fallback or external resource fetch.
Inline content is UTF-8 for text kinds and strict base64 for images, limited to
inline_max_bytes (64 KiB decoded). Check encoded length before decoding into a
bounded buffer. Larger content uses path, never a larger JSON String; the outer
metadata/request cap stays 1 MiB.

### 2.1 Durable records

Every payload below includes `schema: 1` and `artifact_id`. Unknown enum values
have an `Unknown` decoding path; they do not mutate the projection. The parent
dotted names here are logical event names; implementation maps them to the
existing typed durable-log vocabulary and versions its readers before emit
sites. Version numbers start at 1, increase without reuse and refuse overflow.
Only the origin node authors artifact mutations.

| Logical event | Payload beyond the common fields |
|---|---|
| `artifact.published` | `version, publisher: Publisher, title, kind: Kind, content: BlobRef, oversize_reason?: OversizeReason, escalated: bool, interactive: bool, capabilities: [Capability], about?: EntityRef, lifetime: Lifetime, request_id: UUID` |
| `artifact.lifetime_changed` | `version, from: Lifetime, to: Lifetime, by: Author, request_id` |
| `artifact.pinned` / `artifact.unpinned` | `version, by: Author, request_id` (pin is idempotent, not a counter) |
| `artifact.activity` | `version, principal: Author, cause: view | comment | answer | hold_ended, at_ms`; compactable per-version/principal clock and non-operator viewing-budget summary |
| `artifact.comment` | `version, comment_id: UUID, author: Author, text: String, anchor_payload?: AnnotationRef, revision: sha256, delivery: pending, lifecycle_id: UUID, request_id` |
| `artifact.comment_delivery` | `comment_id, lifecycle_id, state: admitted | refused, error?: {code: ErrorCode, retry: Retry}` |
| `artifact.purged` | `version, reason: idle | session_end | quota | operator | blob_missing, by?: Author, request_id?: UUID, redactions?: [{original_annotation_id: UUID, replacement: AnnotationRef}]` |
| `artifact.metadata_expired` | `comment_ids: [UUID]` (at most 100), or `catalog_expired: true`; releases payloads and makes older read cursors explicitly expired |

Transactions use the log owner's complete-transaction framing: `{schema: 1,
tx_id: UUID, records: [ArtifactEvent], receipt?: Receipt}`, capped at 512 KiB
encoded and 100 records. `Receipt = {principal: Author, verb: ArtifactVerb,
request_id: UUID, params_sha256, result: MutationResult, committed_sequence}`.
Canonical params digest includes decoded content hash/size instead of a
transient path spelling; a path replay must resolve/read the same bytes. A
duplicate is matched only after the same validation/auth boundary.
`MutationResult` is the tagged union of the mutation results in §2.2, not an
arbitrary JSON success. A purge batch uses the same request receipt plus
`{operation_id: UUID, next_version?: u32, complete: bool}` progress; its
terminal receipt is retained. Pin/unpin/lifetime of a missing/expired version
refuse, while repeat of the same committed operation returns its receipt.
Whole-artifact purge is at most 50 retained versions, so its retained result
fits the bound. If a lower transaction bound prevents the full operation, it
uses explicit progress batches, never a false all-complete result.

`artifact.metadata_expired` uses exactly one tagged operation: `comments {ids}`
or `catalog`; the comment-id array is at most 100. No metadata-expiry event
deletes a pending outcome or receipt it still needs. `Author` uses the existing
typed attributed principal representation, including token identity instead of
raw bearer text. Enum definitions require future/unknown handling on persisted
reads; unknown command input is a refusal rather than a persisted Unknown value.

`AnnotationRef = { id: UUID, version: VersionRef, size: u32, sha256 }` points to
a version-private annotation sidecar containing the original Anchor. It is
**not** shared by content hash, and contains the excerpt fields instead of
placing excerpts in the immutable global log. Comments retain the original
anchor/version; re-anchoring is a G7 view. This explicitly replaces §5's
persisted `anchor_state`/re-anchored quote sketch. The logical record resolves
the sidecar; after operator purge its redactions mapping selects a new
sidecar/hash that returns `[purged]` for quote fields while preserving
block/position association. Never verify redacted bytes against the original
digest. A mapping contains at most 1,000 entries and must fit the transaction
byte cap. See §5 for deletion semantics.

Question holds are derived from the existing question log's `about:
{artifact_id, version, set_id, question_id, anchor_payload?}` and its close
records. Do not write a second question truth. Pin, lifetime, purge and activity
are mutable projection state over immutable versions; they never edit published
content. Viewing leases themselves are in-memory and end at disconnect/restart;
their consumed non-operator budgets and idle-clock advancement are durable
before acknowledging renewal. No open lease survives a process that cannot renew
it.

**Cost:** new typed log variants and durable activity summaries. Reject a single
loosely typed JSON payload: it prevents safe evolution, bound checks and
reference enumeration.

### 2.2 Public command schemas

Each mutation carries a required `request_id: UUID` in its params, including
MCP. The wire `cmd_id` remains the transport command identity; the artifact
request identity survives transport changes/restart. Reuse the existing
command-id parsing/type, not a new credential. Deduplication is scoped to
`(authenticated principal, verb, request_id)`; an equal request returns its
original result, different canonical params give `artifact_request_conflict`.
Authorization is checked even for replay. Persist a receipt with the mutation;
definite refusals write no mutation/receipt. A response with unknown commit
effect carries request_id, which callers must retry unchanged. Successful
receipts survive at least as long as their subject's retained record; after
catalog expiry a receipt high-water/tombstone entry rejects old request IDs with
`artifact_request_expired` for 90 days. Beyond 90 days replay protection is not
promised: clients must never automatically replay older requests. Persisted
pending custody never ages out this way.

| Command / MCP name | Params (beyond request_id for mutations) | Result |
|---|---|---|
| `artifact.publish` / `flock_artifact_publish` | `title, kind, content?: String, path?: String, supersedes?: ArtifactId, about?: EntityRef, interactive?: bool=false, lifetime?: Lifetime=session, capabilities?: [Capability]=[], oversize_reason?: OversizeReason` | `{artifact_id, version, sha256, size, quota_remaining: {agent_bytes, total_bytes}, escalated: bool, hints: [LeanHint]}` |
| `artifact.list` / `flock_artifact_list` | `scope?: ArtifactScope, about?: EntityRef, mine?: bool=false, since?: Cursor, limit?: u16=50` | `{records: [ArtifactView], next?: Cursor, through: Cursor}` |
| `artifact.comments` / `flock_artifact_comments` | `artifact_id, since?: Cursor, limit?: u16=50` | `{comments: [CommentView], next?: Cursor, through: Cursor}` |
| `artifact.comment` | `artifact_id, version, anchor?: Anchor, text` | `{comment_id, lifecycle_id, delivery: pending | admitted}` |
| `artifact.lifetime` | `artifact_id, version, lifetime: Lifetime` | `{version: VersionRef, lifetime}` |
| `artifact.pin` / `artifact.unpin` | `artifact_id, version` | `{version: VersionRef, pinned: bool}` |
| `artifact.purge` / `flk artifact purge` | `artifact_id, version?: u32` (absent: all retained versions) | `{purged: [VersionRef]}` with bounded paged progress for all-version purge |
| `artifact.viewing` | `artifact_id, version, open: bool` | `{held: bool, lease_expires_ms?, budget_remaining_ms}` |

Viewing uses the authenticated connection identity, not an arbitrary caller
lease id; close is idempotent, renewal uses the same pair. It does not require
request_id or a permanent receipt. Whole-artifact purge reserves a durable
operation id and resumes bounded batches; partial progress is returned
explicitly until the last version's redaction completes. All params are closed
typed objects: reject unknown mutation fields with invalid_params. Publish
requires exactly one content/path. Supersedes reuses the artifact identity and
publisher; only that same stable agent identity can add a version. Concurrent
publishes serialize into distinct versions. Lifetime cannot transfer an artifact
to another agent. Capability declarations require interactive=true, support only
the declared v1 read-stream form, and grant no access.

Read shapes are explicit: `ArtifactView = {published: PublishedPayload, pinned:
bool, effective_lifetime, state: available | expired {reason},
last_activity_ms}`. `CommentView = {comment: CommentPayload, anchor?: Anchor |
redacted, delivery: pending | admitted | refused, error?}`. Read endpoints never
expose backend paths, keys, raw unknown-record bytes or unauthenticated
principals.

### 2.3 Two-tier publish admission and lean feedback

**Decided, 2026-10-29:** decoded content up to 8 MiB is standard, with no
ceremony. Above 8 MiB and through 64 MiB is escalated and requires
`OversizeReason`, a typed UTF-8 string of at most 280 bytes whose trimmed value
is nonempty. No approval is needed. A supplied reason is validated at either
size; supplying one for a standard publish does not consume an escalation slot.
Above 64 MiB refuses with artifact_too_large/after_human; only the operator can
raise the global/node keys, never the repository. The small encoded transport
cap and inline threshold are checked before decode; path content uses the
preflight and bounded stream below.

**Transport choice: require path above 64 KiB inline (decided by this
correctness amendment).** This fits the current MCP/socket publish contract and
MAX_INITIAL_REQUEST_BYTES = 1 MiB in src/api/server.rs. Do not raise that limit
to carry 64 MiB JSON or base64 strings. No new chunked-upload verb or transport
is introduced. A caller with large inline content gets
artifact_inline_too_large/never and a use-path hint; this does not mean the same
bytes exceed max_bytes_escalated. A remote caller must arrange a file in its
attested worktree through its existing execution surface; arbitrary remote
paths, blob URLs and network fetches are not accepted.

Admission order for path publishes is mandatory:

1. Receive only the bounded metadata request under the transport's existing
   pre-auth request/connection limits. Authenticate and attest the caller,
   validate params/reason/identity and check its existing escalation window.
2. Pin and stat the safe regular file without reading its content. Check its
   decoded length, reserve byte quota and any escalation slot, and atomically
   acquire upload_reservation_bytes from the **one server-wide**
   inflight_upload_bytes budget shared across agents and writer workers. If any
   reservation fails, return the typed refusal before reading the file body,
   creating staging content or dispatching a copy worker. Budget exhaustion is
   artifact_storage_busy/later. No per-agent worker pool may bypass the global
   reservation. Recheck rate/quota atomically at this reservation boundary; the
   early check alone is not authoritative.
3. Only then stream file bytes through upload_buffer_bytes (64 KiB) into private
   staging while incrementally computing sha256 and enforcing the reserved
   length and escalated cap. No read_to_end, whole-file String/Vec, mmap
   retained as payload, JSON parse of the entire artifact, or second decoded
   copy is permitted. Validate JSON/image kind incrementally or with bounded
   header/state inspection; the artifact is a blob, not a request JSON document.
   File shrink/growth or cap violation refuses, cleans staging safely and
   releases definite-abort reservations. Never reserve based on stat and then
   accept unbounded growth.
4. Commit the artifact using the existing staged-reference/fsync protocol. Hold
   the upload-memory reservation through worker buffers, validation,
   response/receipt encoding and all retained request copies; release only when
   these allocations are dropped. Request/parse storage stays bounded by the
   small metadata envelope. Inline publishes acquire the same global reservation
   before decoding their small content; their already-received bounded JSON
   envelope remains covered by transport pre-auth admission.

The constant-memory rule includes duplicate-hash repair/verification: the
existing whole-blob get helper cannot be used on the large publication path;
verify any existing bytes incrementally through the bounded read interface.
Incremental JSON validation uses the existing bounded nesting discipline and
rejects excess nesting with artifact_kind_invalid, without building a
whole-document Value. No library helper may silently collect the stream.

The global budget is an operator-owned global/node key, never a project or agent
override. At the defaults it permits at most eight active publishes, not
unbounded concurrency across agent identities. Its charge is fixed memory, **not
file length**: a 64 MiB file uses the same at-most-8 MiB live publish-memory
allowance as a small path file, including metadata and buffer copies. Content
byte quotas separately reserve disk liability. Blocked uploads are refused, not
queued with bodies resident in memory. An unknown commit effect keeps quota/rate
custody until reconciled, but must release upload buffers once the worker has
actually dropped them. This bounds process-owned upload allocations
independently of payload size; it does not claim to bound unrelated server state
or OS file-cache pages. Reject incremental JSON/base64 upload for this release:
the current request parser is whole-message and has no pre-body artifact
handshake. Retrofitting it would widen the transport and memory-admission
surface unnecessarily.

At most artifacts.escalations_per_agent_per_hour escalated publishes commit per
attested publishing agent in a rolling 60-minute window. Reserve the slot
atomically with byte quota before committing. A duplicate request receipt
consumes no new slot; a definite refusal consumes none. An unknown commit
outcome keeps its reservation until receipt recovery settles it. Count committed
timestamps durably across restart/handoff; backward clock movement never grants
extra slots. Refuse a full window with artifact_escalation_rate_limited/later
and retry_after_ms. Identity and quota attribution use §4, never a claimed agent
id.

Every publish result, including idempotent replay, includes the committed size
and escalated flag, plus quota_remaining with agent_bytes and total_bytes after
that commit. Replays return the original receipt's budget observation rather
than pretending to describe current free space. The immutable artifact.published
record keeps oversize_reason and escalated. ArtifactView carries both, and the
trusted artifact card shows the reason beside size as **plain text only**, with
HTML/Markdown/control sequences escaped; it is never rendered as HTML, Markdown
or executable markup, and is not hidden inside the untrusted frame. Decoded
content size, including escalated bytes, counts in the publisher's 256 MiB usage
and the 4 GiB total. The bounded reason metadata counts in that publisher's
annotation usage. No escalation bypasses byte quotas, version caps,
authentication or record-size bounds.

LeanHint is the closed vocabulary prefer_text_or_svg, compress_images,
link_large_data; hints contains those three short guidance items. The tool
description says “Stay lean by default: prefer text or SVG over raster images,
compress images, and link to large data rather than embedding it.” Expose that
description in MCP onboarding under #2725. Hints are guidance, not another
approval step. G7 renders the reason and hints as escaped plain text in trusted
chrome; G6 persists and returns the fields and tool description.

**Cost:** durable per-agent rate reservations, larger bounded uploads and usage
feedback. The owner chose this over the former small single cap; standard
publishing remains frictionless while larger content is explained.

### 2.4 Outcome / retry matrix

All accepted outcome names below must be registered and pinned in
`src/api/error_code.rs` before an emit site ships. Existing names keep their
existing class. Error data contains the request/version/cursor identity when
safe to disclose; unauthorized callers receive only the common authorization
refusal. No error is an invitation to allocate a new request_id.

| Code | Retry | Defined boundary / recovery |
|---|---|---|
| `invalid_params`, `scope`, `not_authorized` | never | Shape, absent named scope or unattested authority; no mutation |
| `artifact_not_found` | never | Authorized read of unknown/expired catalog identity; no existence leak before auth |
| `artifact_owner_mismatch`, `artifact_request_conflict`, `artifact_request_expired` | never | Wrong publisher, altered replay, or obsolete replay receipt |
| `artifact_inline_too_large` | never | Above inline_max_bytes; use an attested worktree path, not a larger JSON body |
| `artifact_too_large` | after_human | Above max_bytes_escalated; only the operator may raise global/node limits |
| `artifact_oversize_reason_required`, `artifact_oversize_reason_invalid` | never | Escalation needs a nonempty UTF-8 reason of at most 280 bytes |
| `artifact_escalation_rate_limited` | later | artifacts.escalations_per_agent_per_hour exhausted for the attested agent in the rolling hour; retry_after_ms names the next eligibility |
| `artifact_path_refused`, `artifact_kind_invalid` | never | Worktree/regular-file safety or invalid content/MIME |
| `artifact_version_limit` | after_human | No eligible version can be removed to stay within max_versions |
| `artifact_version_exhausted` | never | u32 version counter cannot advance; publish a new identity |
| `artifact_quota_exceeded` | after_human | Publisher content quota cannot be met without deleting held bytes |
| `artifact_total_quota_exceeded` | later | Total content quota cannot be met by publisher-only eligible eviction |
| `artifact_metadata_limit` | after_human | Annotation/catalog budget or 1,000-comment bound; operator raises the validated budget, or closes relations and waits for eligible retention |
| `artifact_expired` | never | Version's content removed; record remains inspectable |
| `artifact_cursor_expired` | never | Retained metadata no longer covers cursor; resnapshot using current reads |
| `cursor_ahead` | never | Cursor beyond current owner head |
| `artifact_recipient_gone` | never | Publishing identity ended/replaced before a new comment's custody commit |
| `blob_publication_fence_unavailable` | after_human | Filesystem cannot provide the required interprocess locking; no put/GC until the operator supplies a supported store |
| `artifact_storage_busy` | later | Bounded worker/GC maintenance capacity; no artifact commit |
| `artifact_storage_failed` | maybe | I/O may have committed; retry the same request_id and inspect receipt |
| `blob_migration_failed` | maybe | Triggering control did not commit; preserve source and retry same request |
| `artifact_remote_unavailable` | later | Origin node not local; no local mutation or local blob_missing purge |
| `artifact_grant_operator_only` | never | A non-operator attempted a future grant control |
| `capability_not_declared`, `capability_unsupported`, `rpc_too_large` | never | §7e; G6/G7 always answer RPC with capability_unsupported |
| `capability_not_granted` | after_human | Future grant absent for this viewer/version |
| `rpc_rate_limited` | later | Future broker capacity/rate bound |

Queue transient errors remain their registered classes **inside** custody
recovery. They do not turn a committed comment into a synchronous refusal. After
comment commit the response is a successful pending receipt or
artifact_storage_failed with the request_id if the response's effect is unknown.
A permanent later admission refusal is visible in CommentView and the existing
operator notification surface, not silently logged. Pin/lifetime controls that
fail migration have no acknowledged control effect.

## 3. Shared storage, reference accounting and crash recovery (Q3)

**Decided, 2026-10-29: this amendment decides #1712.** This includes retained
root enumeration, stream/thinking retention, shared-root preservation,
concurrent publication/collection safety, crash recovery and stale `.tmp-*`
cleanup requested in #1712's owner comment. G6 owns implementing that issue; it
remains open until its acceptance tests pass, not closed by accepting the design
alone. Retained session/stream/thinking records keep all their references until
their existing retention owner (including S16) durably removes them; GC
introduces no independent thinking-content expiry. Unreferenced content and
abandoned staging use the grace and fencing below.

The **serving flk server's BlobCoordinator, elected for the canonical data root
under the existing server ownership lease plus an exclusive coordinator lock**,
is the sole owner of reference accounting and destructive collection. Separate
pump-local `BlobStore::open` handles are clients of this protocol, not
independent accounting owners. Every writer process using that root must
participate, including structured transport pumps, control/result recorders,
artifact writers, import/recovery paths and future external helpers. There is no
raw successful-put API that can release its publication guard before its
referencing durable append. No artifact-only unlink path. Durable records and
retained stream/history checkpoints are the truth; a persisted reference index
is a rebuildable acceleration and includes a scan generation plus durable source
cursors. It is not an independently authoritative refcount.

A reference key is `(source-kind, origin, durable-record-id, field)` with hash,
size and storage class. Staging registrations carry `{operation_id,
writer_generation, hash, target_source, state: staged | committed | aborted}` in
the coordinator; aborted/process-dead entries do not release protection until
the final source scan proves the writer cannot still append. The maintenance
journal also has schema 1 records: `DeleteIntent {operation_id, hash,
candidate_generation, owner_generation}`, `DeleteComplete {operation_id}`,
`MigrationIntent {operation_id, hash, size, source: {backend, session},
destination: data_dir, triggering_request_id}`, `MigrationComplete
{operation_id}`, and `GcCandidate {hash, scan_generation, first_zero_ms}` /
`GcCancelled {hash, scan_generation}`. The owner manifest is `{schema: 1,
stable_session_id, backend, relative_root}`; the path must resolve within the
trusted server-managed scratch root. Index entries carry `{reference_key, hash,
size, storage_class, source_cursor}` and index checkpoints carry `{schema: 1,
generation, complete_source_cursors}`. All of these are server-private recovery
formats, not new mesh custody markers or authoritative artifact records. Unknown
maintenance schemas disable destructive work until supported. The backend
supplies bounded put/read/copy/remove operations; the coordinator serializes
reference transitions and GC under the same ownership fence. Any persisted
consumer must provide a reference enumerator before deletion is enabled.
Unknown/newer records, incomplete scans, corrupt log segments, unregistered
consumers and unverified checkpoints make collection fail closed for the
affected namespace; do not guess that no references exist. Retry after
inspection, report pressure to the operator. Index rebuilding cannot serve new
destructive maintenance until all registered sources reach the same captured
barrier.

**All-writer publication fence, including duplicate hashes.** Each writer
acquires a shared interprocess publication lock on one stable lock file under
the canonical data root **before the first blob lookup, staging-file creation or
deduplication fast path**. Every successful put returns `StagedBlob {ref,
publication_guard, operation_id}`. This is a staged reference even if identical
bytes already existed and no write occurred. For multiple blobs in one
line/transaction, keep every staged reference (a transaction may share the same
lock guard) until that line/record and its directory creation, if applicable,
are durably fsynced. The recorder then promotes the exact reference keys to
committed accounting and releases the stage. Publishing to the in-memory stream
or closing the blob file is not a commit boundary. Abort releases only after the
line cannot still be appended by that writer. No implicit conversion to an
unguarded BlobRef may outlive this obligation. Locks are **per-open flock/OFD
locks** on a pinned canonical-root lock inode: each independent guard uses a
separately opened descriptor, so pump-local stores and the collector conflict
even in the same process. A cloned descriptor shares its original guard
lifetime; process-wide POSIX fcntl record locks are forbidden because they can
merge same-process ownership and release protection unexpectedly.

**Bounded collector claim, not a locked root scan.** The collector scans all
durable sources, reconstructs readable complete lines, prepares a DeleteIntent
and catches its reference index up **outside** the exclusive publication lock.
It never performs root enumeration, log replay, allocation, worker/IPC waits or
data-filesystem operations while holding exclusive. Under exclusive it
revalidates exactly **one** prepared candidate against the caught-up index's
source watermarks, root/stage/read counts and owner generation, and installs an
in-memory `DeleteClaim {hash, operation_id, owner_generation}`. A changed
watermark or stale index aborts that attempt and releases immediately for
another outside-lock scan; it never scans the delta while locked. All source
appends and their index promotions participate in the all-writer protocol, so no
new unindexed line can be committed across that check. This constant-work
section has `blobs.gc_exclusive_budget` of 5 ms and at most one candidate: check
the deadline before each preallocated index step and abandon/release on
exhaustion. No fsync/unlink call can extend the exclusive section. The
implementation tests both the candidate/work bound and deadline release with the
injected clock. OS scheduling is not a real-time guarantee; an arbitrarily
stalled kernel/process cannot be given a wall-clock lock-release guarantee. The
inline pump path remains lossless even if that exceptional stall exceeds the
operational 5 ms ceiling.

Release exclusive **before** dispatching unlink and directory fsync to the I/O
worker. A valid DeleteClaim is a per-hash exclusion enforced by the sole
coordinator for every process: while it exists, no new stage, migration, read or
reference to that hash may begin. Writer promotion still requires the
coordinator and a valid generation; a writer cannot bypass the claim with its
own store handle. Request writers receive artifact_storage_busy/later; pumps
store the complete line inline. The claim stays until deletion/fsync finishes or
its result is reconciled, so delayed I/O cannot unlink a later writer's newly
published object at the same hash path. Unrelated hashes proceed. If deletion
won, a later put must recreate/verify the missing object under a fresh stage. A
stale worker may not initiate deletion after its generation loses ownership.
Handoff drains/reconciles in-flight claims before transferring the root;
process-crash recovery consults DeleteIntent, rescans roots and resolves whether
bytes still exist before accepting writes to an uncertain hash. Unknown outcome
retains exclusion and reports capacity pressure; it never assumes deletion
finished. Index preparation and durable DeleteIntent writes do not themselves
authorize deletion: only the validated current-generation claim does, and the
executor must retain that claim through I/O completion.

Every writer checks claims under the coordinator after acquiring its shared
publication guard, including the duplicate-hash fast path. After a candidate is
claimed, a later complete inline line cannot create a reference to that hash, so
it does not undermine the delete. `.tmp-*` cleanup uses the same claimed-name
fence and one-candidate budget. Preparation, grace, index catch-up and disk
reclaim all run unlocked; the 24 h grace is not a publication fence.

**Pump behavior under contention (decided 2026-10-29: keep complete bytes
inline).** A structured pump uses a nonblocking publication-lock/claim attempt
for optional blob projection. On contention, collector admission pause or a
pending DeleteClaim, keep the **original complete harness line** inline and
append it through the ordinary log path. Do not return artifact_storage_busy
through `BlobStore::project`'s `?`, end the pump, drop a block, skip its
sequence or wait for unrelated traffic. If earlier blocks in that line already
acquired staged refs, discard the partial projected result and retain/release
those refs only after the original inline line commits or its append is
definitively aborted. No unguarded blob reference may remain in the inline
result. The existing complete-input bound remains in force; inline fallback does
not enlarge it or re-read an unbounded source. True log I/O/cancellation/input
errors retain their existing explicit behavior; only GC contention is nonfatal.
Request-driven artifact publication may still refuse after `blobs.fence_wait`;
unsupported locking still refuses blob_publication_fence_unavailable/after_human
rather than performing an unprotected put. The pump treats inability to obtain
this optional projection fence as inline projection, not as permission for an
unlocked put.

**Cost:** transiently larger stream records and fewer deduplicated/offloaded
blocks while GC claims bytes. This is lossless within the existing input/log
contract. Reject a timed fatal put error: it kills the stdout reader. Reject
indefinite pump blocking: an unexpectedly stalled collector would backpressure
the harness. Reject any fallback that keeps a reference but releases its stage
before append.

**Fairness/starvation.** Between exclusive claim attempts the collector releases
all publication locks and waits at least blobs.gc_yield_interval, servicing
writer admissions first. Once an eligible candidate is ready, the coordinator
may pause new shared-lock admissions for at most gc_exclusive_budget to acquire
one exclusive claim after existing staged writers drain; pumps use inline during
the pause and request writers may refuse/retry. Existing stages are never
stolen, timed out or revoked merely for GC. A stalled live staged writer may
therefore starve collection: this is explicitly accepted to preserve its future
append. Pressure remains visible and new growth refuses at the disk reserve; the
operator resolves the stuck writer rather than deleting its bytes. No continuous
stream of *new* stage admissions may bypass the bounded drain/claim opportunity.
No lock remains held while waiting for that opportunity.

On unsupported/unreliable interprocess locking, put, GC and stale-temp cleanup
refuse blob_publication_fence_unavailable/after_human; existing read-only access
and lossless pump inline recording may continue. **No grace-only or unlocked
blob write/cleanup fallback.** Process death releases its OFD/flock locks.
Recovery treats **every readable complete accepted record/stream line as a
root**, even if the dead process never observed or issued a successful fsync for
it: readable complete data may survive a crash. Only incomplete/invalid tails
follow the existing log-repair rules; they must not hide a later valid complete
entry. Before any destructive work, catch up the root index and reconcile dead
stages/claims. Handoff drains/fences old writers; old-generation writers cannot
append or promote after transfer.

Layout: durable content remains `<data dir>/blobs/<sha256>`. Session scratch
resides under the server-managed `<data
dir>/artifact-sessions/<stable-session-id>/`, mode 0700, with blobs by hash and
private staging files. The name session_tmp means session-lifetime scratch,
**not** an OS process-temp directory or an RAII auto-delete directory. Its root
and owner manifest survive process restart and handoff. Backend selection is
restart-only and cannot strand existing references: recovery opens each
manifest's recorded backend until migration completes. No backend path is
trusted from an agent record. Session manifests map stable logical session
identities to locations; never infer identity from a recycled PID or pane
placement.

Commit protocol:

1. Acquire the all-writer shared publication guard and reserve quota/catalog
   space and an operation under the single owner; no irreversible eviction until
   input validation/auth succeeds. Pin opened input handles, bound reads, hash
   and fsync complete staged bytes; publish by atomic rename then fsync
   directory. Publication and later record commit both re-check the reserved
   owner generation.
2. Retain every staged reference, including deduplicated puts, through
   append/fsync of the immutable record or stream line and its receipt. Promote
   to committed accounting before releasing the publication guard, then expose
   it through the read model/stream and acknowledge. A pre-record blob is an
   orphan, not a published artifact. Multiple records for identical content have
   separate logical references.
3. Adding a durable reference to a temp blob first writes/fsyncs a migration
   intent in the coordinator maintenance journal. Copy to durable staging,
   fsync, verify hash, publish/fsync durable location. Append the triggering
   pin/lifetime/question transaction only after durable bytes exist; register
   its durable reference, then release the temp reference; retain the shared
   publication guard through the triggering durable record fsync even if the
   durable bytes were deduplicated. The intent names operation, hash, source
   backend and destination, not a credential. Recovery checks the triggering
   durable record: finish committed migration or retain the intact source and
   discard the uncommitted intent. There is no auto-commit of a failed pin.
   Question publication uses this same prerequisite so an open question never
   points only at sweepable bytes.
4. Outside the publication lock, complete the root scan and prepare the
   candidate generation/hash/first-zero marker. Wait the 24 h grace unlocked,
   catch up the index and prepare DeleteIntent, then acquire exclusive only for
   the single-candidate claim section above. Revalidate zero root/stage/read
   counts and all captured watermarks; changed state aborts without collection.
   Release exclusive before filesystem unlink/fsync and keep the per-hash claim
   until I/O is reconciled. Never hold a publication lock during a root scan,
   grace period or filesystem operation. Adding a reference cancels candidacy.
   Recent staged files and unknown ownership never become eligible merely
   because a filename looks old.
5. Purge first commits its tombstone/ref release, then schedules removal.
   Recovery can repeat ref release without decrementing twice because reference
   keys are sets. For operator excerpt redaction, acknowledgment additionally
   waits for the redaction deletion fence in §5. A failure is typed and its
   request receipt reports partial/pending completion; it does not claim
   erasure.

Opened blob reads hold a coordinator read lease until the response is
finished/cancelled. Expiry may publish a tombstone while a read already in
progress completes, but physical deletion waits. New reads after tombstone
refuse. Handoff passes the existing durable fence, drains or explicitly cancels
old read leases, and installs one coordinator; no parallel GC owner. All disk
work is bounded on an I/O worker; generation-fenced installation also rechecks
the current owner. A stopped producer or stale worker cannot install an obsolete
index. A logical session restart preserves its scratch; identity end writes
session_end tombstones only for unheld versions. A held ended-session blob stays
until hold end plus idle_ttl, or migrates for pin/question/persistent reference.

**Cost:** retrofit every pump and store writer to a guard-bearing put result and
fsync-scoped lifetime, per-open interprocess shared/exclusive locking, nonfatal
inline pump fallback, bounded per-hash delete claims, consumer enumeration, a
rebuildable index, tiny recovery intents, read/delete leases and delayed
reclaim. This is explicitly the work of #1712; slice 1a lands the writer fence
before 1b enables any collection. Reject raw filesystem age scans and
artifact-local refcounts: both can delete bytes held by streams or an
in-progress migration. Reject OS temp cleanup: it destroys live logical sessions
during process restarts. A 24 h grace is not a promise that unknown references
become safe after 24 h.

## 4. Version cap and quota eligibility (Q4)

**Decided, 2026-10-29:** max_versions counts non-purged versions, including the
current version. Before committing version 51, select the oldest eligible
version by `(last_activity_ms, version)`, within this artifact and this
publisher only. A version is eligible if it is unpinned, has no active
question/view/read/migration hold, and no pending comment custody. Old
superseded versions are idle candidates immediately when all holds end; the
new/current version of a still-running session is not automatically
quota-evictable. Persistent or ended-session versions are idle candidates after
the hold-ending activity timestamp; quota eviction need not wait 14 days. TTL
expiration still waits the full idle_ttl. A superseding publish may reserve the
old current version as its candidate, but purges it only in the same successful
owner transaction as the new version. Failed publishes never remove it.

If 50 versions are held or otherwise ineligible, refuse with
artifact_version_limit/after_human, do not make version 51 or silently unpin.
Version numbers never reset after purge. Then enforce publisher content quota
and total quota using only that publisher's eligible versions. Preflight all
three constraints before committing the selected purge set: a total-quota
refusal must not leave opportunistic deletions from a failed publish. The
durable transaction envelope records bounded purge choices and publish/receipt
atomically. Recovery either replays that complete transaction or skips a torn
final transaction.

**Quota attribution is immutable and server-derived.** Only publish/supersede
can add content bytes to an agent's meter, and those commands require that same
attested publishing identity. They never accept a billing agent_id or
token-origin agent claim. Pin/unpin/lifetime do not publish extra copies or
increment another agent's content meter: pre-existing version bytes keep their
original publisher charge, including while held, as §7a requires. An operator
pin can deliberately keep that already-charged version from eviction, but cannot
add new billed content to it. Annotation/receipt/activity overhead charges the
authenticated authoring principal's own budget
(`artifacts.quota_annotation_bytes_per_principal`, default 16 MiB) plus
quota_annotation_bytes_total, never the target publisher's annotation quota.
Attested agent-origin annotations charge only that same agent's per-agent
annotation budget. Question holds use the asking identity's existing question
budget, not a claimed target agent. Server-generated automatic bookkeeping uses
a bounded server maintenance allocation within the total, not an arbitrary agent
meter. If the authenticated caller's allowance or the total is full, refuse
before committing: **no fallback to charging the artifact publisher**. All
controls and comments recheck these charges on replay under the original
principal, not a newly claimed principal. This explicitly overrides §5's
shorthand “per-publishing agent” for client-authored annotations. Shared stream
references retain their existing consumer accounting.

Count **logical bytes per non-purged version**, even when bytes deduplicate
physically or a stream also references them. Holds consume quota. Shared stream
bytes do not consume artifact quota but still prevent physical deletion. This
predictable conservative accounting deliberately does not promise a
physical-disk ceiling: streams, orphan grace and staging also use disk. Reserve
bounded staging capacity and refuse before exhausting the disk reserve.

**Cost:** durable composite mutation and conservative quota consumption; at most
50 versions inspected per artifact, publisher eviction uses an index with
bounded worker pages. Reject treating dedup as free quota: one agent could
otherwise evade its limit. Reject forced eviction of held versions or another
publisher's content: it violates the owner's keep/hold rules.

## 5. Bounded reads, retention and purge privacy (Q5)

**Decided, 2026-10-29:** use opaque typed cursors bound to endpoint, canonical
filter, principal, origin, scan generation and captured through sequence. No
time-only since cursor and no offset paging. First page captures the finite
through barrier; continuation never expands it. Subsequent fresh reads with
since=that terminal cursor return strictly later records up to a new barrier.
Return records ordered by durable artifact stream sequence, then stable record
id for snapshot ties. Comments are ordered by their creation sequence; delivery
changes appear as upserts in the stream. Artifact list is a paged current-state
snapshot at its barrier, followed by changes from its returned terminal cursor.
Changing filters/principal requires a fresh cursor. Cursors beyond the head give
cursor_ahead; invalid binding gives invalid_params; expired generations give
artifact_cursor_expired, never an empty success that hides a gap.

Default page 50, max 100 records and 256 KiB encoded bytes, stopping at
whichever bound comes first. A single record must fit the page cap. Snapshot
pages use the same caps, and at most 20 latest comments/artifact; comments
outside that window are discoverable by the comments endpoint while retained.
Use an indexed/seeked iterator with buffered continuation; never reread/discard
pages or rescan from the start. Replay/live registration and capture happen in
one critical section; the wire uses existing explicit
gap/snapshot_to/synced/lagged semantics. Keep an immutable cut via shared
segments, not a whole-catalog copy for every new event. Hold at most four live
update cuts and 1 MiB queued encoded data per subscriber; the initial replay cut
does not consume those four slots. If either live bound is exceeded, discard
queued replay/live work and close explicitly lagged; client resumes from its
last applied cursor.

The opaque cursor is a tagged union internally: `Position {filter_digest,
principal_id, origin, sequence, retention_generation}` for a completed read, and
`Page {position_binding, cut_id, through, next_key, expires_ms}` for a
continuation. Server-issued cursor integrity prevents caller alterations; it
grants no authority and current auth is still required. Reusing Position
captures a fresh finite cut; Page continues only its original cut. A page cut
has a 5 minute maximum lifetime. On expiry release its read pins and return
artifact_cursor_expired on continuation. A metadata mutation that must redact
content invalidates any cut referencing it; operator purge waits for active page
encoding/read leases, so a newly served page cannot leak a stale excerpt after
purge ack. A connected client may already hold text; the server cannot erase
that client memory, transcripts, screenshots, backups or separately copied
content. This is not a global secure-erasure feature.

The annotation quotas and catalog/comment-count caps are reloadable operator
configuration (`artifacts.quota_annotation_bytes_per_agent`,
`artifacts.quota_annotation_bytes_total`, `artifacts.max_comments`,
`artifacts.max_catalog_entries`), with the defaults in §1. Raising these is
explicit capacity allocation; never disguise deletion of retained comments as
content purge. Annotation accounting includes comment text, original anchor
sidecar, about metadata, request receipts and lifecycle/delivery overhead at
encoded size; §4 assigns each new charge to the authenticated author, not the
artifact target. Reserve worst-case terminal-outcome space with every comment.
Per-agent/client-principal 16 MiB, total 256 MiB, max 1,000 retained
comments/artifact and max 10,000 catalog identities/server bound growth; a
pending delivery/hold/pin cannot be evicted for room. Comments become
retention-eligible 90 days after terminal custody, only when no open
question/thread references them. Fully purged artifact catalog entries expire 30
days after their last retained relation closes; pins and open relations prevent
expiry. Expiry records advance the retention floor before payload compaction;
old read cursors then fail explicitly. Durable segment compaction follows the
log owner's existing crash-safe checkpoint/rename/fsync protocol; never truncate
an unrelated global-log user. If shared-log compaction cannot reclaim space yet,
physical storage remains visible and admission refuses at its disk budget.
Counts/byte limits are enforced even when a relation remains open forever: new
growth refuses rather than silently discarding it.

An operator purge first writes/fsyncs new redacted sidecars and commits their
original-id→replacement mappings in the purge transaction; only then may it
remove the old sidecars. It acknowledges redaction only after syncing those
replacements/directories, removing old sidecars/staging copies and fencing
existing readers. The mapping preserves the new digest, so a redacted payload
never masquerades as its old hash. The log contains AnnotationRef, **not**
original quoted text, so redaction does not require rewriting global log
history. Sidecars are never deduplicated across versions. Crash recovery
consults committed purge tombstones before serving any sidecar and completes
redaction before reporting the purge complete. Missing/redacted sidecar yields
the explicit redacted anchor; it cannot resurrect an excerpt from a log.
Automatic purge keeps sidecars until normal annotation retention. Free-form
comment text is not classified as an anchor excerpt; copied prose in a comment,
an agent transcript or another artifact is outside this targeted purge. This
narrows the parent's “no text survives” sentence to server-held anchor excerpt
fields, not all copies of the content.

Pressure notices use one existing operator notification identity per
threshold/storage class: update the largest-holder summary, do not enqueue a
notice per failed publish. Emit at upward crossing 80% and 100% (including
would-exceed refusal); re-arm only below 75% and 95% respectively. Persist
armed/disarmed state across restart. The notice action previews eligible idle
unpinned candidates and requires explicit authorized purge; it never removes
held versions in bulk.

**Cost:** paged cuts, annotated sidecars, deletion fences and catalog/receipt
compaction. Reject unbounded list/comments results, lossy since timestamps, and
log-only excerpt storage: the first two hide loss or exhaust memory, the last
cannot honor targeted excerpt purge without rewriting unrelated history. Durable
log retention alone is not an admission budget.

## 6. Visibility, scopes and blob reads (Q6)

**Decided, 2026-10-29:** local artifact visibility is node-wide for a credential
explicitly holding read; ArtifactScope is a **filter**, never a permission
grant. G6 does not implement project-private artifacts, roles, per-agent
client-token restrictions or resource delegation. Those require #2691. The UI
and docs must warn publishers that artifacts are visible to readers authorized
on that node. origin is local-only in G6; another origin returns
artifact_remote_unavailable, not a fake empty list.

| Action | Required existing authority |
|---|---|
| Agent publish/supersede/lifetime | Existing pty/structured attestation for that same stable publishing identity; no token-origin publishing as an agent |
| Agent MCP list/comments | Attested agent, its own artifacts only; mine=false cannot widen this grant |
| Client list/comments/artifacts stream/blob content | read, independent of send/answer/operate; no scope is inferred from another |
| Client artifact.comment | send only, attributed to that credential's own principal under #2585; no read requirement for admission, and response exposes only that command's result |
| Client pin/unpin/lifetime | **operate only (decided 2026-10-29)**; answer is restricted to answering scoped questions |
| Local CLI pin/unpin/lifetime | Kernel-attested local operator; exclude pane ancestry and relay edges |
| Explicit operator purge | operate or kernel-attested local operator |
| Client viewing | read; operator exception to the 8 h aggregate budget only for the already-defined operate principal |
| Future live-data grant | operate; G6/G7 refuse as capability_unsupported before allocating a grant |

Scopes never imply one another: operate alone cannot read/comment; send alone
cannot pin, answer a question or operate; **answer alone cannot pin, unpin,
change lifetime, read/comment or purge**. These retention controls are
explicitly operator actions and require operate, matching
`src/api/client_tokens.rs::Scope` and #2585. No new scope or role is introduced.
The parent's older answer-or-operate pin wording is superseded. An agent holding
only attested agent/MCP authority cannot pin; a bearer explicitly delegated
operate has that operator action, without guessing its process's role. An
operate token is not permission to add charges to another agent; §4 fixes
new-byte charging at the attested publisher and annotation/control overhead at
the authenticated author principal. **Cost:** less delegation granularity until
#2691. Reject broadening answer to mean arbitrary disposition: its named
authority is answering scoped questions. An explicitly named future
artifact-retention scope would require its own owner decision, not an
undocumented expansion here.

**New route, not an existing implementation:** `/v1/blob/<sha256>` is specified
in ADR-0036 but is absent from `src`. G6 slice 1c must build authenticated
GET/HEAD/range/conditional serving on the bounded blob coordinator before any
artifact/GUI slice relies on HTTP content access. A read to that route must
prove at least one currently visible live reference through the blob
coordinator's reverse index, including ordinary stream references authorized by
the existing read policy. Knowing a hash, a tombstoned reference or an
uncommitted staging file is insufficient. Use the same policy for
range/HEAD/conditional requests, and recheck on every request; no credential
enters a frame or blob URL. Implement the ADR-specified immutable ETag/content
semantics but replace public caching with **private, no-store** for
authenticated artifact delivery: logout/purge must not hand another principal a
shared cached body. Range and conditional requests do not bypass the
tombstone/auth fence. G6 must build this check into the new shared blob route
before exposing artifacts; it must not accidentally revoke legitimate
stream-line references. HTTP denial follows the existing common authorization
envelope, not a hash-existence oracle.

New comments require an available retained version and the same live publishing
identity at the durable custody boundary. Identity end/replacement gives
artifact_recipient_gone before commit; restart/handoff of the same identity does
not. **Existence probe decision, 2026-10-29: accept only for an authenticated
credential authorized for that action.** A send-only caller may distinguish
artifact_not_found, artifact_expired and artifact_recipient_gone for a guessed
target, just as it can observe admission/refusal, but receives no content,
title, hash or list access. This is an explicit limited existence oracle, not
read scope. Cost: a holder of send can enumerate guessed identifiers; random
origin-qualified UUIDs reduce guessing but are not authorization.
Unauthenticated callers or callers missing send get the same scope/authorization
refusal before lookup, with no target detail. Reject requiring read merely to
hide action refusal differences: it would make send imply a read prerequisite
contrary to #2585. Blob/read denials remain non-oracular as described above.
Supersession never transfers ownership, even with operate. Remote-control
routing is deferred to #1380, not inferred from an origin id.

**Cost:** reference-aware shared blob authorization and an attributed token
queue principal (depends on #2585 implementation). Reject guessing agent roles
from ancestry of a bearer request or silently upgrading it to operator. Reject
project ACLs now: no accepted resource-constraint model exists. Reject public
blob caching: a content hash is not a bearer authorization grant.

## 7. Comment custody and recovery (Q7)

**Decided, 2026-10-29:** durable outbox, not a two-writer best-effort call.
Before committing a comment, validate principal/send authority,
version/recipient identity, payload and metadata budgets; reserve its
lifecycle/outcome space. The single artifact owner appends the comment plus
request receipt atomically, with server-assigned comment_id and a stable derived
lifecycle_id in the existing queue UUID namespace. Its delivery begins pending.
A reply acknowledges **durable custody**, not stdin delivery: pending is a
successful explicit state. The same request id returns the same identities and
current delivery state. A sender must not generate a new request id to retry a
lost reply.

The existing single queue owner receives that exact lifecycle id and the
canonical content digest/version/publisher from the outbox. Digest
canonicalization excludes the randomized framing delimiter; persist/reuse the
admitted rendered payload rather than generating a changed request on recovery.
Frame every untrusted field with the established unforgeable per-item delimiter;
a token's slash text is always a framed message, never an operator command. If
the queue owner reports its previously committed identity/digest, record
admitted rather than enqueueing again. A key with different content is a real
conflict and must not overwrite committed content. A matching committed
admission wins over any later transient/refusal result. The artifact owner
consults the queue's durable committed-key lookup **before** writing refused,
including after a conflict.

Crash cases:

| Boundary | Recovery |
|---|---|
| Before comment transaction commits | No custody, no delivery; same request may safely retry |
| After commit, before enqueue | Replay pending outbox and retry exact lifecycle id |
| Queue committed, outcome record absent | Owner's committed lookup returns same item; write admitted, no new enqueue |
| Unknown write effect | Keep pending; query exact committed key then retry on bounded backoff, never generate a new key |
| Deterministic refusal with no committed admission | Write refused with the registered code, settle outbox and publish existing operator-visible refusal notification |
| Outcome committed | Restart reads admitted/refused and never re-enqueues |

Queue fullness/storage unavailable stays pending and retries at the existing
bounded backoff (initial 100 ms, doubling to max 30 s for this outbox, no finite
attempt limit while custody remains valid). Earliest due retry participates in
the real event-loop deadline, even on a headless quiet server. Retry worker
capacity is bounded (32 active operations, 128 pending in-memory heads;
remaining durable entries are cursor-paged); stop/cancel and unexpected worker
completion must not hang recovery. A permanent identity replacement becomes
refused only after committed-key reconciliation. No pending entry expires
automatically. Existing admission-side input_write_unknown never becomes a false
refused custody outcome. Once admitted, later session failure is queue lifecycle
state, not a reason to re-deliver the comment.

**Cost:** durable replay index, exact committed-key lookup and deadline
integration. Reject enqueue-before-log (orphan delivery),
log-then-return-session_busy (sender retries a hidden custody item), and
optimistic refused settlement (it can revoke an admission already committed).
This reuses queue ownership and lifecycle ids, not the message mesh custody
format or a second stdin writer.

## 8. Origin identity and G11 boundary (Q8)

**Decided, 2026-10-29:** origin-qualified ArtifactId from first publication,
including local calls. NodeId is the mesh's stable authenticated identity, not
an advertised hostname. VersionRef, about/anchor references and request receipts
preserve origin. Reinstalling under a new node identity creates a new namespace;
importing a backup does not impersonate the old node without the existing
identity recovery procedure.

G6 serves locally authored records/blobs only and refuses another origin with
artifact_remote_unavailable/later. It neither ingests unverified remote records
nor interprets their missing bytes as blob_missing. **Carriage is deferred to
the existing issue [#1602
(G11)](https://github.com/gerchowl/flock/issues/1602)**, with remote commands
still dependent on #1380. No mesh VERSION change, new message format or marker
is authorized by this amendment.

G11 must specify authenticated origin/custody, origin-only mutation authority,
duplicate/version ordering, pending-transfer presentation, digest verification
and bounded fetch, remote hold/pin propagation, purge precedence and GC
ownership. Until both record and verified bytes have arrived, a future received
version is transfer-pending, not available or locally purged. That future state
must not leak into G6 as an empty success. Accepting origin-qualified ids now
does not accept a carriage protocol.

**Cost:** qualified ids in all generated contracts from day one. Reject
retrofitting origin later: ids already persisted in comments/questions/cursors
would become ambiguous. Reject hostname prefixes and treating content hashes as
publisher identity.

## 9. Ownership and release boundary (Q9)

**Decided, 2026-10-29:** G6/#1598 owns the artifact owner, durable
records/receipts, **#1712 shared mark-from-roots GC and stale-temp cleanup
including all-writer publication fencing**, backend coordination/reference
accounting, **the new authenticated `/v1/blob` route**, bounds/configuration,
lifecycle/leases, pin/lifetime/purge CLI/socket/wire controls,
metadata/pressure/refusal notifications, question-hold observer, comment
custody, MCP publish/list/comments, artifact stream and TUI text fallback. It
provides one typed service, and all callers use it. **Q9 explicitly assigns
#1712's implementation and acceptance tests to G6, including the structured
pump/store retrofit and cross-process fence; it is not an unnamed follow-up
outside this plan.** No GUI-private files or second input path.

G7/#1599 owns iframe sandbox/CSP and shell security tests, trusted
untrusted-content chrome, safe rendering/MIME conversion, selection/quote
generation and cross-version re-anchoring views, viewing renewal integration,
pin/purge buttons, question sets/request_feedback tools, feedback confirmation
and fork/merge-back UX. It calls G6 for version existence/migration/hold
prerequisites before publishing a question. A purge invalidates G7's retained
rendering and removes its frame; the kept question remains answerable with
expired content.

Both ship **default deny** for live data. G6 accepts bounded immutable
flock_stream declarations but issues no grants. G7 answers every flock.rpc with
capability_unsupported and never supplies a token; allow_stream_grants=true
alone cannot bypass this. Actual grants, revocations and broker limits are
reserved shapes in the parent §7e and land only with a separately reviewed
live-data implementation; external origins and declarative bindings are not
defined here. Interactive scripts still have the disclosed navigation/WebRTC
exfiltration residual; no test claims those channels are closed. Configuration
inventories follow ADR-0041 and ADR-0042 (#2724), but implementing these new
keys and reload semantics is G6's responsibility.

**Cost:** cross-component question/hold and viewing contracts must land before
enabling the matching UI. Reject leaving purge/holds as a GUI-only concern: the
server must enforce them without a connected GUI. Reject silently pulling G11 or
role administration into G6.

## 10. Pre-mortem

| Failure we expect if the boundary is wrong | Prevention / fail-first evidence |
|---|---|
| Restart empties session scratch despite a live logical identity | Stable-session manifest survives real restart/handoff; temporary process directory is not the backend |
| Duplicate-hash pump put returns; GC runs before the new stream-line fsync | Pause the real pump after its duplicate put while its stage is held, request exclusive collection, then fsync the line and release: bytes remain readable throughout, including after writer-process crash/recovery; removing the staged-duplicate fence must fail deterministically |
| GC holds exclusive when the pump projects its next large complete line | Real pump nonblocking projection falls back to the original inline line, consumes the following line and stays live; no lost sequence/block, including a partially projected multi-block line |
| A complete line survives crash without an observed fsync | Recovery enumerates every readable complete line as a root before collection; no fsync-receipt assumption permits unlink |
| Process-wide lock lets the collector pass a same-process pump guard | Independent per-open flock/OFD guard conflict witness covers two BlobStore handles in one process plus another process |
| A collector unlinks a tool-result blob shared with an artifact | Complete all-consumer scan, exact ref keys, read/migration fences; API test purges one ref and still reads the other |
| A crash acknowledges a pin whose bytes never reached durable storage | Inject failure at intent/copy/fsync/verify/record steps; pin never acknowledges until durable bytes and record agree |
| Quota refusal first destroys an older report | One bounded publish/eviction transaction and preflight; public refusal leaves the old version readable |
| A quiet server never delivers pending comments | Real-loop clock/deadline test with no external wake; unknown-effect recovery reconciles exact committed key |
| Retry/restart produces two comments or two queue items | Canonical request receipt + stable lifecycle id, crash-after-commit public test and changed-body conflict test |
| Purge appears successful but history pages reveal excerpts | No excerpts in global-log payloads, version-private sidecars, cut invalidation/read fences; resume old cursor after purge cannot retrieve an excerpt |
| An agent keeps data alive by renewing read leases forever | Durable per-principal/version lifetime budget; injected-clock re-lease past 8 h neither holds nor resets idle clock |
| Read/send/answer credential changes retention or charges a victim | Dispatcher scope matrix refuses pin/unpin/lifetime without operate; server-derived accounting charges new content only to the attested publisher and new control/comment overhead only to its author, never a claimed agent |
| Ring overflow or paging silently loses a comment | Captured cursor bounds, atomic capture/registration mutation witness, explicit cursor-expired/lagged and snapshot recovery |
| An artifact hash becomes a public download capability | Auth on GET/HEAD/range/conditional paths and private no-store; unknown hash and denied reference have common auth behavior |
| A 64 MiB JSON upload or concurrent file readers exhaust memory | Keep JSON at 1 MiB and inline decoded content at 64 KiB; preflight auth/rate/quota and global fixed-memory reservation before path-body reads, stream/hash through a fixed buffer including dedup verification; slice 5 compares peak live allocations across content sizes and refuses excess concurrent readers |
| Metadata fills disk although content is under 4 GiB | Separate annotation/catalog limits, disk staging reserve, retained pending custody blocks growth instead of being deleted |
| Remote transfer delay becomes an irreversible purge | G6 rejects nonlocal origin; G11 owns pending-transfer semantics in #1602 |

## 11. Ordered G6 slices and public-path acceptance

Retain the original ten slice subjects, splitting the first into 1a/1b and
adding route prerequisite 1c (twelve PRs total); this section replaces its open
Q2–Q9 references. Every estimate is **at most 400 production lines**, Rust plus
TS; tests/docs excluded. If a slice exceeds the budget, propose a smaller
ordered split before a PR. No dead public command or weakened default may ship
between slices: new artifact capability advertisement remains off until its
prerequisites are present, with explicit unsupported refusal. Shared
infrastructure slices exercise the real pump/log path; slice 1c introduces and
tests the previously missing HTTP path. No test assumes that route already
exists. Owner acceptance was recorded on 2026-10-29. Dependencies,
implementation size caps and fail-first acceptance remain prerequisites; design
acceptance is not a claim that the code ships.

| Order / budget | Scope and normative contract | Fail-first public-path acceptance |
|---|---|---|
| 1a / 280–380 | All-writer staged publication fence, §3 / #1712; collection still disabled | **The pump survives GC holding the exclusive lock**: drive its public harness/recorder path, hold a real exclusive guard, send a large complete line then another line, assert original inline payloads/sequences and live continuation. Cover partial multi-block projection, typed request-writer contention and same-process per-open lock conflict. Separately drive a duplicate hash: hold after put before append/fsync, request collection's exclusive fence, prove no unlink; fsync/promote/release permits collection recheck. Deterministic barrier/witness mutation removes duplicate-put guard and must fail. Separate writer process crash releases lock, every readable complete recovered line remains a root, even without observed fsync; unsupported-lock refusal is typed |
| 1b / 300–390 | Shared root index/backend accounting and collector, §3 / #1712; depends on 1a | Real pump/stream and artifact roots share bytes; drop one reference, preserve the other. Unknown consumer blocks GC; injected-clock witness pins one candidate/5 ms exclusive work bound, no locked filesystem calls and a writer opportunity between claims; stalled existing writer delays GC rather than losing data. Grace clock expires only after complete scan, stale temp held by a writer survives, abandoned temp reclaims after grace, restart reconstructs all roots and intents. If enumerators exceed budget, split again before enabling collection |
| 1c / 260–380 | **Build authenticated blob HTTP route**, §6; depends on 1a/1b, before slices 2–10 | New GET/HEAD/range/conditional path uses pinned bounded reads, reverse-index read authorization and no-store. Hash-only caller, send/answer/operate-only credentials and unknown/staging/tombstoned references cannot read; legitimate retained stream reference can. Hold response during purge/GC to prove read fence; typed missing/corruption/range failures and no response path leaks credential |
| 2 / 300–390 | Durable artifact owner/publish ingress, §2 | Attested MCP/socket publish returns stable id/version/hash plus size, quota_remaining, escalated and lean hints; altered replay, forged author, bad MIME, overflow, symlink escape and pinned-handle race refuse without mutation; inject record/fsync failure |
| 3 / 280–380 | Session backend/migration, §3 | Real pin/question/lifetime transaction migrates before ack; fail copy/hash/fsync and crash replay, source stays whole, concurrent sweep skips intent |
| 4 / 300–390 | Lifecycle/viewing holds, §§1/3 | Stop purges, same-identity restart/handoff survives, open question/view/pin holds, injected loop clock checks post-hold TTL, 90 s lapse, eight-lease cap and non-operator budget |
| 5 / 280–380 | Quotas/version/config, §§1/4 | Peak live publish allocations stay within the fixed reservation for both small and 64 MiB path inputs (bounded allocation/buffer instrumentation through real ingress); hold workers at the read barrier, exhaust the global upload-memory budget across agents, and assert the next request gets artifact_storage_busy before any file-body read. Check auth/rate/quota refusals also read zero content bytes, 64 KiB-inline threshold and unchanged 1 MiB JSON cap; publish decoded 8 MiB standard / 64 MiB escalated boundaries, required/empty/281-byte reason refusals, third-per-hour later refusal, durable replay/rate recovery, 256 MiB-agent / 4 GiB-total limits and remaining-budget/hint results; version 51 held refusal, publisher-only oldest eligible eviction, atomic failed preflight, reload lowering, quota classes, 80/100% notice reset across restart |
| 6 / 260–370 | Pin/purge/redaction/missing-content, §§3/5/6 | Dispatcher scope matrix, live read held across purge, redaction after restart, old cut cannot reveal excerpts, automatic purge preserves them, shared hash remains for another live ref |
| 7 / 180–300 | MCP list/comments and metadata retention, §§2/5/6 | Real MCP mine/filter/cursor pages retain order, byte/read bounds and no rescan; unauthorized read and expired cursor refuse, unknown persisted payload preserved, catalog/annotation budget blocks growth |
| 8 / 300–390 | Durable comment custody, §7 | Actual artifact.comment at queue gate, exact-key replay after crash-before/after-enqueue, unknown-effect quiet-loop retry, permanent refusal operator-visible, changed-body request conflict never duplicates |
| 9 / 280–390 | Artifacts provider/generated wire, §§2/5/6 | Real WS atomic replay/live registration with deterministic split-lock mutation failure, sparse filtered sequences, gap snapshot and quiet reconnect, lagged close, scope denial and TS drift |
| 10 / 180–300 | TUI fallback/default deny, §§8/9 | Isolated TUI renders escaped title/version/size/open hint/comments/expired status; capability metadata bounded, no grant allocation; oversize_reason with HTML/Markdown/script and terminal controls renders as escaped plain text in the exact trusted-card cells through G7, never executable markup; bounded lean onboarding text via #2725. G7 supplies frame source/token/CSP/RPC-unsupported browser tests |

Each new acceptance behavior must be shown red once against the missing change
or a precise regression mutation, then green through the real public ingress.
Use focused nextest, retries 0, bounded condition waits and producer liveness;
no sleeps or widened timeouts. Clock-driven tests advance the existing injected
clock and real deadline. Mutation proofs must fail deterministically without
racing test threads or product-only test hooks. Slices 1c, 7 and 9 also bound
bytes read and verifies resume does not rescan. These are future implementation
tests; this PR changes accepted design documentation only.
