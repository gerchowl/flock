# Structured queue admission and batch formation

ADR-0036 §3/§4's worker-owned admission core is the first scheduling slice
(#1878, part of #1802 and #1428). Server producer routing/acknowledgements
remain #1803; runtime reservation and lifecycle is #1879. This core never
writes to a harness. Disk operations belong on the dedicated blocking worker.

`Admission::enqueue` takes a trusted dispatcher `Principal` and one gate
outcome. The gate is persisted once as send/hold/stash/drop before acceptance;
an identical retry under the same ID returns success without another append,
even after lifecycle changes or reopen. A persisted SHA-256 of the normalized
admission request (including trusted origin and gate) distinguishes a changed
payload, origin or gate, refused as `queue_item_conflict`. Lifecycle updates
retain that hash; legacy items without it cannot prove an identical retry and
return the conflict code. Operator authority supplies human attribution.
Agent/channel/system/fork attribution comes from the dispatcher, never from
text or an item's claimed producer. IDs must use the UUID wire shape. The MVP
refuses unsupported delivery/priority modes and unknown scheduling types with
the registered `queue_mode_unsupported` code. Invalid UUIDs, forged gate/batch
metadata, missing reply/continuation anchors and invalid commands are refused
before any append. Reply question IDs must be canonical lowercase hyphenated
UUIDs, both at admission and on replacement. Attachment strings must be
`blob:<sha256>` with exactly 64 lowercase hex digits; arbitrary text, paths,
URLs and noncanonical hashes are refused. Deterministic validation errors are `invalid_request`, entropy failures
are `internal_error`, and serialization errors are `serialization_error`.
Only actual append I/O failures or an already poisoned journal report
`input_write_unknown`; callers must reopen after uncertain I/O, not blindly
retry. Validation failure does not poison the journal or spend a cursor.

Only an authenticated operator can create a command item. Its body must start
with `/` and have no attachments. Formation sends it verbatim as the only text
block of an isolated turn. Producer attribution remains on the queue item and
its raw audit record. Slash-prefixed text from every other producer is ordinary
labelled input, so the resulting block starts with `From`, not `/`. This is the
architect-approved exception recorded in ADR-0036 §3 and §3a.

For valid candidates, `batch(now_ms)` returns an owned candidate without
spending a cursor, reserving items or marking them delivered. An unbounded
head instead receives a durable `failed` state with `queue_item_too_large`;
a head that fails rendering receives its render refusal (for example
`invalid_request` for a malformed reply anchor or blob ref). Formation then
continues with the next eligible item. A valid prefix is returned before a
later malformed member becomes the head and is failed, so valid items are
never discarded because a later member cannot render. A compact `fail`
record retains the original content, even when a full replacement cannot fit.
The failure is published only after sync and survives reopen. The runtime owner must enforce
S4/S6 and reserve before writing. Eligible items are queued, normal, at_idle,
gated send, without a live edit lease. Only human producers may have edit
leases; admission and replacement refuse them for other origins. Malformed
human leases are ineligible. A
continuation requires its anchor to be done with no outstanding result flag.
Consecutive eligible human inputs batch in FIFO order, stopping at any other
producer or command; non-human inputs and commands go alone. Active human
editors are skipped. Default max_batch is 8; zero is invalid. A single-item
candidate uses its item UUID; a multi-item candidate has a fresh version-4
batch UUID and one producer-labelled block per member. Each non-command
header names its producer and item UUID, reply question UUID when applicable,
and JSON-encoded attachment refs. Header JSON also escapes U+2028, U+2029
and U+0085 so they cannot form a labelled header line. The body is enclosed in nonce delimiters
per ADR-0036 §3a. Each rendering uses 128 random bits in lowercase hex,
regenerated if present anywhere in the body; fake labels and delimiter-like
text cannot escape that block. This framing preserves attribution, rather than
claiming that a model cannot follow instructions it reads in an untrusted body.

Both the journal and stdin framing impose a 2 MiB ceiling. Admission checks
the expanded text envelope and reserves room for later lifecycle/refusal
metadata and the largest sequence/state encodings. An item whose later update
cannot fit is refused before acceptance. Formation takes a consecutive prefix
that fits both ceilings, preserving FIFO and the count bound. No payload is
truncated. Queue storage counts the final newline in its bound, so formation
uses that same conservative limit for stdin too.

`replace()` preserves producer, kind and original admission hash. It checks
the replacement's validation, single-item stdin/lifecycle budgets and combined
journal record size before writing. Atomic `batch` records contain full
replacements of existing items and retain their positions. Validation folds on a private copy; a missing member or I/O
failure cannot publish a partial batch. Reopen applies the whole record. An
older reader skips the unfamiliar kind, retains its raw cursor and refuses
checkpointing; because membership/order did not change, later upsert/remove
positions remain valid. Unknown variants/fields still follow the storage
core's preservation and rewrite-refusal rules.

Older readers skip an unfamiliar `fail` record just as they skip `batch`,
preserve its cursor and block rewrites. It changes no positions, so later
upserts/removes remain valid. A maximal legacy item's failure metadata can
make a later checkpoint exceed the line ceiling; that checkpoint fails
without replacing the journal. Normal admission reserves this space; retained-growth follow-up is #1820.
