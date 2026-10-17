# Structured queue admission and batch formation

ADR-0036 §3/§4's worker-owned admission core is the first scheduling slice
(#1878, part of #1802 and #1428). Server producer routing/acknowledgements
remain #1803; runtime reservation and lifecycle is #1879. This core never
writes to a harness. Disk operations belong on the dedicated blocking worker.

`Admission::enqueue` takes a trusted dispatcher `Principal` and one gate
outcome. The gate is persisted once as send/hold/stash/drop before acceptance;
a repeated item ID is refused. Operator authority supplies human attribution.
Agent/channel/system/fork attribution comes from the dispatcher, never from
text or an item's claimed producer. IDs must use the UUID wire shape. The MVP
refuses unsupported delivery/priority modes and unknown scheduling types with
the registered `queue_mode_unsupported` code. Invalid UUIDs, forged gate/batch
metadata, missing reply/continuation anchors and invalid commands are refused
before any append. A failed journal operation returns its source error and an
uncertain-write code; callers must reopen after uncertain I/O, not blindly retry.

Only an authenticated operator can create a command item. Its body must start
with `/` and have no attachments. Formation sends it verbatim as the only text
block of an isolated turn. Producer attribution remains on the queue item and
its raw audit record. Slash-prefixed text from every other producer is ordinary
labelled input, so the resulting block starts with `From`, not `/`. This is the
architect-approved exception recorded in ADR-0036 §3 and §3a.

Formation is pure: `batch(now_ms)` returns an owned candidate and does not spend
a cursor, reserve items or mark them delivered. The runtime owner must enforce
S4/S6 and reserve before writing. Eligible items are queued, normal, at_idle,
gated send, without a live edit lease. Malformed leases are ineligible. A
continuation requires its anchor to be done with no outstanding result flag.
Consecutive eligible human inputs batch in FIFO order, stopping at any other
producer or command; non-human inputs and commands go alone. Active human
editors are skipped. Default max_batch is 8; zero is invalid. A single-item
candidate uses its item UUID; a multi-item candidate has a fresh version-4
batch UUID and one producer-labelled block per member. Replies name their
question; attachments remain literal blob references.

Both the journal and stdin framing impose a 2 MiB ceiling. Admission checks
the expanded text envelope and reserves room for later lifecycle/refusal
metadata and the largest sequence/state encodings. An item whose later update
cannot fit is refused before acceptance. Formation takes a consecutive prefix
that fits both ceilings, preserving FIFO and the count bound. No payload is
truncated. Queue storage counts the final newline in its bound, so formation
uses that same conservative limit for stdin too.

Atomic `batch` records contain full replacements of existing items and retain
their positions. Validation folds on a private copy; a missing member or I/O
failure cannot publish a partial batch. Reopen applies the whole record. An
older reader skips the unfamiliar kind, retains its raw cursor and refuses
checkpointing; because membership/order did not change, later upsert/remove
positions remain valid. Unknown variants/fields still follow the storage
core's preservation and rewrite-refusal rules.
