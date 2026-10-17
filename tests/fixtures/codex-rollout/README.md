# Codex rollout reducer fixtures (#1661)

The #613 fixtures are **synthetic**, composed to represent Codex **0.160.1** records in
[`../codex/finished.jsonl`](../codex/finished.jsonl) and
[`../codex/in-progress.jsonl`](../codex/in-progress.jsonl). They split the
finished file at every byte boundary, including inside JSON strings and before
newlines, asserting both partial-prefix and completed state. No Codex process or
model runs. These files are not evidence of a recorded writer shape.

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

## Real rollout provenance (round 2)

[`real-0.160.1.jsonl`](real-0.160.1.jsonl) is derived from an existing local
Codex **0.160.1** session, created on 2026-10-14. It is the complete contiguous
prefix through the first matching `task_complete`: **336 records**, ordinals
**0–335**, with no omitted or reordered records. The completion includes an
error object and a null last-agent message; it still commits the matching end.
No new session or model call was made to obtain it.

Derivation is reproducible with
[`scripts/derive_codex_rollout_fixture.py`](../../../scripts/derive_codex_rollout_fixture.py):

```sh
python3 scripts/derive_codex_rollout_fixture.py "$CODEX_HOME/sessions/<existing-rollout>.jsonl" /tmp/neutral-rollout.jsonl
```

The script checks the first metadata version, retains every record until the
first matching completion, and preserves dictionary keys and their order,
arrays, container/value types, numeric values, booleans, nulls, ordinals, record
and nested `type` discriminants, roles, `cli_version` and reviewed public enum
values such as `history_mode: paginated`, source markers and status. Reviewed discriminants
are allowlisted; an unknown value fails derivation for explicit review.
All other string content is replaced with deterministic neutral strings. This
covers session/turn/item IDs (equality retained), messages, tool arguments and
output, encrypted content, instructions, user/account identifiers, paths, cwd,
repository details and freeform model/configuration content. Timestamp
strings become a fixed neutral timestamp. No original session text, hostname,
user path, repository name or credential is retained. Numeric timing/counter
fields remain unchanged. Inspect the output before using a different source.

The real prefix includes `session_meta`, `turn_context`, `world_state`,
`token_usage_record`, `event_msg` and `response_item`. Every payload is an object;
its complete replay validates the reducer's current object-envelope rule and
paginated ordinal handling against this real writer. Legacy/no-ordinal and
malformed-envelope cases remain explicitly **scripted** coverage, not a claim
that this source recorded those forms.

The real-fixture test exercises every byte split, including partial-prefix
state. It replays each complete prefix once, clones that exact pure-state
checkpoint for each split of the next record, and compares the **entire**
post-record reducer with the unsplit result. Equality includes ordinal, cursor,
retired IDs, failure and partial bytes, so continuing with the remaining suffix
has the same final idle state for every split. Test-only cloning avoids
quadratically re-parsing the whole file and does not add a runtime input path.

The existing compatibility-pin maintenance test checks this real fixture's
metadata version against the declared writer version as well as the synthetic
metadata and scripted `--version` output.
