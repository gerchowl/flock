# Verified Codex file/cursor fixtures (#1855)

`src/codex_reader/tests.rs` creates local files under private fixture directories,
uses the test process's own PID/start identity as the scripted launch writer,
and obtains the file through the real `codex_binding::validate` path. No model,
operator profile, live socket, sleep or retry is used. All observation times are
supplied. These tests exercise filesystem producers, not preconstructed cursors.

The real neutralized **0.160.1** prefix documented in
[the rollout provenance](../codex-rollout/README.md) is replayed in 336 incremental
record steps. Only its first metadata `id` is rebound to the fixture's `child`
thread; all subsequent bytes are unchanged. The test checks that each append is
delivered once and the existing reducer reaches idle. Synthetic fixtures cover
legacy/no-ordinal streams, declared referenced-history ordinal bases, unknown
kinds, malformed records and ordinal modes/gaps/overflow. The ordinal declaration
comes from metadata, not an arbitrary first observed number; see the
[pinned writer](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/rollout/src/ordinal.rs#L20-L28).

Continuity tests replace, truncate, edit, remove or redirect actual files after
an initial successful read. Equal-length prefix mutation must fail even when the
file identity is unchanged. A source failure is sticky: later bytes cannot
silently restore it. File/record budgets fail rather than truncate, and the
paired reducer's turn budget never evicts IDs. EOF tests advance the supplied
clock by hours without new records; working and idle remain intact. Partial EOF
retains exact suffix bytes and complete/read offsets, including an unfinished
start after idle. Snapshots preserve that distinction for later attribution.

The regression proof temporarily disables file identity/prefix verification,
new-byte cursor delivery, envelope/ordinal error enforcement, metadata matching,
resource checks and checkpoint suffix retention. All nine reader tests fail with
retries zero. The exact saved production bytes are restored and hash-checked
before the positive rerun; no fault-injection hook is part of the product.

## Boundaries

This foundation returns verified appended bytes and immutable checkpoints. It
checks an opened handle against its still-local primary path before/after each
read, rehashes the entire consumed prefix, and verifies framing/ordinals before
returning a batch. `Limits` bounds file bytes, record bytes and the paired
reducer's remembered turn IDs. Repeated prefix verification has linear work in
the consumed prefix per check; the file-byte limit bounds that work. Reads may
hold the file buffer, returned suffix and partial checkpoint concurrently.

A returned batch is not lifecycle authority. Segment attribution and admission
before `Binding::finish`, resume/copy validation, ignoring inherited partial
records, child fork delimiters, compact continuity and server metadata delivery
belong to #1856. Live writer checks, worker supervision, bounded cancellation,
EOF progress/health and stale-capability delivery belong to #1857. Call this
synchronous file code on that supervised worker; never on the render path.
A checkpoint copy alone does not prove destination-local ownership. Actual
server authority and consumer/screen integration remain #1664. #1663 and #1479
stay open until their remaining slices land.

No unvalidated history is passed to an authoritative reducer by this slice.
The caller must discard a failed batch and invalidate any paired evidence on
reader error; recovery requires independently verified binding and segment
reconstruction. Historical lifecycle interpretation belongs to the segment
adapter, so the reader shares envelope/ordinal validation with the pure reducer
without treating an inherited unmatched start as a current turn.
