# Codex rollout reducer fixtures (#1661)

The reducer tests replay the existing composed Codex **0.160.1** records in
[`../codex/finished.jsonl`](../codex/finished.jsonl) and
[`../codex/in-progress.jsonl`](../codex/in-progress.jsonl). They split the
finished file at every byte boundary, including inside JSON strings and before
newlines. No Codex process or model runs.

[`src/codex_rollout/tests.rs`](../../../src/codex_rollout/tests.rs) also
constructs scripted JSONL records for the ADR-0028 §1a reducer table: both
start/end aliases, aborts, malformed complete records, partial final records,
unknown kinds, ignored output/error/compaction/child events, duplicate and late
boundaries, conflicting IDs, terminal failures, legacy and paginated ordinals,
source invalidation and verified-resynchronization inputs. Supplied monotonic
clocks advance by hours without sleeps. Matching boundaries alone advance the
boundary cursor and timestamp.

Shape references are pinned upstream sources:
[names and aliases](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/protocol/src/protocol.rs#L1406-L1418),
[start/complete payloads](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/protocol/src/protocol.rs#L2152-L2198),
[abort payload](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/protocol/src/protocol.rs#L4256-L4277),
and [ordinal state](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/rollout/src/ordinal.rs).
Paginated tests explicitly supply the verified segment's expected first ordinal;
legacy tests omit ordinals. No thread-ID arithmetic or age lease is inferred.

The caller supplies record-byte and remembered-turn-ID budgets. Capacity
exhaustion yields sticky unknown; IDs are never evicted to make room. Reader
budget selection and verified file/prefix/cursor reconstruction belong to #1663.
Fresh reducer construction after an invalidation models verified reconstruction;
it is not evidence of an implemented reader, binding, or authority path
(#1662–#1664). Empty history cannot establish lifecycle idle.
