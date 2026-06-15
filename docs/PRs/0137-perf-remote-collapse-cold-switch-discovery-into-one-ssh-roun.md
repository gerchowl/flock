---
number: 137
title: "perf(remote): collapse cold-switch discovery into one SSH round-trip"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-14T13:24:32Z
closed: 2026-06-14T13:27:54Z
merged: 2026-06-14T13:27:54Z
base: master
head: perf/collapse-switch-probe
url: https://github.com/gerchowl/herdr/pull/137
---

# perf(remote): collapse cold-switch discovery into one SSH round-trip

Closes #133.

## What

A cold (non-warm) server switch ran remote-herdr discovery as **3–4 separate `ssh` invocations**:
1. `uname -s; uname -m`
2. `command -v herdr`
3. `test -x … && … --version && … status client --json` — once or twice (PATH binary, then default location)

Without ssh ControlMaster multiplexing that is 3–4 sequential SSH handshakes before the bridge can even start — the bulk of a cold switch’s latency.

## Change

New `probe_switch_remote_herdr`: one `/bin/sh -s` script emits tab-delimited `KEY<TAB>VALUE` lines (platform, PATH binary + its version/status, default `$HOME/.local/bin/herdr` + its version/status). The result is parsed locally and the compatible binary chosen with **no further round-trips**.

- Selection order (PATH binary, then default) and the version+protocol match gate are **identical** to the legacy chain (`remote_binary_matches`).
- The interactive `prepare_remote_herdr` install/upgrade path is **unchanged** — it still uses the per-step probes (it needs the intermediate results for install decisions).
- Robust to missing sections / empty values (tab-delimited keys, not positional lines).

## Verification

- `cargo build`, `cargo clippy --all-targets -D warnings`, `cargo fmt` all green.
- New unit tests: `parse_switch_probe_reads_tab_delimited_fields`, `switch_probe_matches_requires_version_and_protocol`.
- A cold switch to a reachable peer now issues **one** probe SSH command instead of 3–4.

Stacked on #135 (now merged). Note: CI `check` job has pre-existing flaky integration tests (`cross_area`, `cli_wrapper`) unrelated to this diff; `check-contributor` is the upstream fork gate.
