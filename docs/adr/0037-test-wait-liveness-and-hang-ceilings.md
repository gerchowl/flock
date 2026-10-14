# ADR-0037: Test wait liveness and runner hang ceilings

- Status: Accepted
- Date: 2026-10-14
- Issues: [#1317](https://github.com/gerchowl/flock/issues/1317)
- Decision owner: repository owner, architect decision on #1317 (2026-10-14)
- Scope of acceptance: condition-wait policy and measured nextest backstop

## 1. Decision

Condition waits must fail when their producer dies, emits unexpected output,
or stays silent. Use producer liveness checks, stage machines that reject
unexpected transitions, and failure-only bounded receives. A timeout must not
turn missing evidence into success. No sleeps, retries, widened windows, or
loosened thresholds to make flaky tests pass.

Every test binary gets a finite nextest termination ceiling in the default and
CI profiles. This backstop names a hung test, including a live silent producer,
and never decides whether a condition was satisfied. Every slow-timeout override
must retain termination because an override replaces the entire timeout setting.

## 2. Sizing and limits

Use per-binary, per-platform p99 test durations from roughly 20 recent dev CI
runs on macos-15 and ubuntu. Choose at least 10 times the larger p99 and never
less than 180 seconds. Give unusually slow binaries explicit measured exceptions.
Keep justified stricter existing ceilings. Record the inputs, percentile method,
partial-run coverage, and reproduction command in the measurement report.

The initial report is [nextest timings](../next/nextest-timings.md): 420 seconds
by default, 660 for mesh_delivery, and the existing 180-second readiness ceilings.
macOS live_handoff is absent from the existing CI filter, so its stricter ceiling
is supported only by Ubuntu measurements. New binaries inherit the default but
need measurements when introduced. Historical p99 cannot guarantee future load.
Nextest may allow its termination grace period after the ceiling expires.

Recollect before resizing. Investigate the slow condition or dead producer first,
then document legitimate duration changes and update the explicit exception and
measurement regression test. Retry settings are unchanged by this decision.
