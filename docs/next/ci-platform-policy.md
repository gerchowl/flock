# CI platform policy

Owner-approved policy (2026-10-16, #1613): ordinary pull requests run full Linux
build, lint and test checks, plus macOS build and lint (`macos-15 / build` and
`macos-15 / lint`). They schedule no macOS test shards. The build still compiles
all test binaries and verifies the complete shard partition.

Landing batch PRs whose head branch starts with `land/`, and pushes to `dev` or
`main`, run the full macOS test suite as well. Batch detection uses
`github.head_ref`: on PR events `github.ref` instead names the PR merge ref.
The existing merge-group trigger also retains full tests. Other reusable-leg
callers default to full tests unless they explicitly disable them.

The reusable leg's boolean `run-tests` input controls the test job before matrix
expansion, so ordinary macOS PRs create no shard runners. See
[GitHub job conditions](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idif).
Build and lint always remain enabled. The required `check (macos-15)` gate runs
with `always()` and accepts only a successful reusable leg: on ordinary PRs this
requires build and lint, and on batches/pushes it also requires every test shard.
The result-gate step itself uses `always()`. Failed, cancelled, skipped or missing
legs fail explicitly; disabling test shards by policy does not skip the required
gate. The Linux gate retains the same failure rules and full test coverage.

A macOS-only test failure may first surface in a landing batch, rather than its
individual PR. The batch must pass its full macOS checks before landing. A green
ordinary PR's macOS aggregate is evidence of build/lint success, not test success.
The policy reduces PR runner demand; it does not reduce batch coverage or change
the existing action pins, permissions, test filters or shard assignments.
