# CI shard result and assignment rules

The required platform check succeeds only when its reusable leg succeeds.
JUnit summaries are advisory in the shard and aggregate jobs: a malformed report
can produce a diagnostic failure without changing the test/lint result. Aggregate
jobs use `!cancelled()` so superseded workflow runs can cancel. In a workflow that
remains active, failed, skipped or cancelled dependencies still fail the result
gate. See [GitHub status checks](https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#status-check-functions).

Nix push checks include the reusable CI leg and `.config/**`, covering shard
placement and nextest groups/ceilings. Pull-request checks retain their unfiltered
trigger. Cache restoration and retention policy are unchanged.

`scripts/ci_shards.py verify` validates serial-pty assignments in the default and
CI profiles before publishing a shard matrix. The assignment reader accepts only
unions (`|`) of literal `binary(name)` or `binary(=name)` selectors, with whitespace.
Names contain ASCII letters/digits, underscores, dots or hyphens. For literal
names, nextest's default glob matcher has exact-name semantics.
See [nextest matchers](https://nexte.st/docs/filtersets/reference/#default-matchers).
Other predicates, operators, pattern/escaped matchers, partial expressions and
platform-conditioned overrides fail with the profile, override index and filter.
They require explicit assignment support before CI can publish a matrix.

Nextest remains authoritative for the actual selected tests. Verification checks
for duplicates, omissions and strays, and labels name only binaries containing
selected tests on that leg. A platform-excluded binary therefore disappears from
the label as well as the selected set.
