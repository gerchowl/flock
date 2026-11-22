# In-place build backend measurements (#3081)

This is the first measurement step selected in the [owner decision](https://github.com/gerchowl/flock/issues/3081#issuecomment-6874876882).
The [consolidated measurement plan](https://github.com/gerchowl/flock/issues/3081#issuecomment-6784277482)
and ADR-0052 supply the design context. This report supplies evidence for a future
run-broker ADR; it does not implement or decide that ADR.

On this Mac, same-path seed reuse re-archived in 1.17–1.23 seconds, while
relocating the seed took 112.43–133.63 seconds. Two steady edited builds
finished in 91.77–93.62 seconds per pair, versus 87.48–88.19 seconds for one.
The retained compiler cache did not give Rust hits across changed target paths.

## Scope and reproduction

Measured on 2026-11-22. Rust 1.95.0, Cargo 1.95.0, nextest 0.9.146,
sccache 0.18.0, and the Nix CLI supplied by Lix 2.95.2.

Measured source: `fad0aafc91de25a790db46b4b5b857cde000baf5`, including #3216's
debug settings and #3224's merged GUI integration binary. The machine has 14
logical cores, 64 GiB RAM, macOS 26.6.2, and APFS storage. No cluster or VM access
was used. All builds use two Cargo jobs and disable compiler incremental state.

The operator supplies the private scratch parent and admission-file location as
`SCRATCH` and `TASK_SLOT`. The exact invocation is:

```sh
TASK_TARGET="$SCRATCH/m3081"
nix develop --command python3 scripts/measure_build_backend.py \
  --target "$TASK_TARGET" --slot "$TASK_SLOT"
# After the main run exits successfully:
nix develop --command python3 scripts/measure_build_backend.py \
  --target "$TASK_TARGET" --slot "$TASK_SLOT" --append-controls
```

The main run requires an unused target prefix; append modes reuse the owned experiment. Repeating the experiment requires another
exclusively owned prefix and an appropriate admission file. The script checks
the destination volume for at least 15 GiB free before every copy and checks
admission before every build. It waits at most 300 minutes for the admission
file and a one-minute load average below 10. Each command has a 30-minute fail
ceiling. Four simultaneous cached `nix develop` entries are monitored and stopped
if load reaches 10. No Flock command is invoked, and inherited `FLOCK_*` values
are removed from measured workloads and cache processes.

The harness writes `results.json`, individual command logs, source snapshots,
compressed archives, and Cargo HTML timings under `$TASK_TARGET-results` and
the measured target directories. Raw logs and timings contain local paths and
must remain private. The accompanying JSON contains metrics and relative
scratch names suitable for publication.

Every archive command is run inside the devShell as:

```sh
export CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 NEXTEST_TEST_THREADS=2
export CARGO_NET_OFFLINE=true
export CARGO_TARGET_DIR="$RUN_TARGET"
export FLOCK_GUI_DIST="$RUN_SOURCE/tests/fixtures/gui-dist"
/usr/bin/time -l cargo nextest archive --locked --features web,ts \
  --timings --archive-file "$RESULTS/$SCENARIO.tar.zst"
```

`cargo fetch --locked` runs once before timing. A private sccache daemon, cache,
socket and persistent temporary directory belong to the experiment. The current
harness sets `TMPDIR=$TASK_TARGET-sccache/tmp` for every subprocess it launches,
including auxiliary commands; the cache wrapper selects that same directory.
The shared daemon and other agents' targets are untouched. A `finally` stops
the private cache and verifies that its endpoint no longer accepts connections.
SIGINT and SIGTERM cancel/reap tracked live subprocess groups and take the same
cleanup path. Signals during cache startup are deferred until its startup
client returns; cleanup then runs. Startup and shutdown clients each have a
30-second ceiling. An already-active endpoint is rejected without stopping it.
The normal-run and signal/exception probes exercise a real dedicated daemon.

The 46 retained Cargo samples are historical first-round data: their workload
TMPDIR was inherited, the cache wrapper used its owned persistent temporary
directory, and cache shutdown was performed separately after measurement.
They have not been relabelled as measurements of the corrected lifecycle.
The new Nix samples use the corrected lifecycle and TMPDIR; their
`lifecycle_run` fields link to the JSON audit, including controller hashes,
`stop_exit=0` and `endpoint_gone=true`. Cache-stat deltas are measured
with `sccache --show-stats --stats-format json` around each serial command or
concurrent pair. `SCCACHE_IGNORE_SERVER_IO_ERROR=1` permits uncached compilation
if the cache service fails. Archive creation builds test executables; it does
not execute tests, so the nextest thread setting is recorded for the future
lease contract rather than exercised here. The checked-in GUI fixture keeps
frontend building outside the timed operation.

The initial cold run means an empty Cargo target and empty private sccache. Registry downloads,
the Nix store, OS page cache and existing Zig cache are retained. Seed copies
use `/bin/cp -cRp` (APFS clone, preserve metadata), after the cold archive. Same-path
runs move the prior target aside and restore the seed at the original absolute
target path with the source path unchanged. Before each restore, the touched
module's original timestamp is restored so earlier scenarios cannot invalidate
the seed. Different-path runs relocate both
source and target. Fresh checkouts are independent `git archive HEAD`
extractions with a seed target, without Git metadata. They are source-tree
materializations rather than network clones. This deliberately includes new
source timestamps. The append command measures two further fresh source
extractions with empty targets and the existing private sccache, to distinguish
compiler-cache hits from Cargo's seed reuse. Touch changes `src/main.rs`'s mtime. Each edit cycle appends
a different harmless comment to that module and builds an archive; two series
of ten cycles are measured without restoring the target in between.

The one/two-build comparison uses independent writable source and target
copies and different source comments before each run, retaining the Cargo job
cap of two *per build*. The shared private sccache remains in use. Cargo lock
messages are counted from complete command logs. Initial `serial-*` and
`concurrent-*` runs include first use of relocated copies. The append command
therefore repeats the one/two comparison twice as `steady-*`, after both copies
have been built at their final paths. Those steady runs supply the concurrency
comparison; the original runs remain in the raw table. The absence of lock
messages does not prove that no lock was acquired or briefly waited on.
Lock-message kinds and counts are recorded, not individual wait durations;
command wall times include any waits. Offline mode does not eliminate the
observed package-cache lock messages.

For the full-debug comparison the archive command adds:

```sh
export CARGO_PROFILE_DEV_DEBUG=true CARGO_PROFILE_TEST_DEBUG=true
/usr/bin/time -l cargo nextest archive --locked --features web,ts \
  --timings --archive-file "$RESULTS/$SCENARIO.tar.zst" \
  --config 'profile.dev.package."*".debug=true'
```

This restores Cargo's full dev/test debug information for both the package and
dependencies. Current repository settings are package line tables and no
dependency debug information. Each comparison has its own target. The full-debug
cold run starts with an empty target but retains the private sccache from the
current-profile runs; profile-specific Rust cache keys differ, while C/C++ work
can still hit. The counters disclose that reuse. These are separate cold-target
observations rather than a randomized, identically cold-cache profile trial.

Nix measurements run `/usr/bin/time -l nix --option eval-cache false develop
--command true` once, then `/usr/bin/time -l nix develop --command true` twice
sequentially and in two groups of four concurrent entries. Fresh evaluation is
only cold-ish: the store and fetch caches are retained, and the outer devShell
already exists. This does not measure downloading or building a new toolchain.
The original 28.99-second fresh-evaluation sample triggered automatic GC and a
GC-lock wait; it is quarantined and excluded from aggregates. All eleven original
Nix entries remain in the raw data but are superseded by eleven replacement
samples using the corrected lifecycle and owned TMPDIR. One of the 22 total Nix
entries has GC signals; none of the 11 replacements does. The fresh-evaluation
aggregate now uses the 2.5946-second replacement. No aggregate cell remains
contaminated or unavailable in this dataset.

Daemon auto-GC is an uncontrolled host setting. The harness does not override
`min-free` or `max-free`. It detects the captured auto-GC and GC-lock messages
for every measured command, including Cargo. A signal in any concurrent member
quarantines the entire batch. Rows and groups record `gc_contaminated` and
`aggregate_eligible`; excluded observations remain visible. Nix batches retry
up to three total attempts, with unique log names; only a signal-free successful
attempt enters aggregates. Exhaustion records an unavailable cell rather than
using contaminated timing. No retries were needed in this replacement run;
regression tests exercise both retry success and exhaustion. A GC-affected
archive stops the experiment and requires restarting from the original source,
target and cache state, rather than quietly retrying against warmed artifacts.

The daemon GC lock is not readable by the invoking account. The detector
therefore cannot prove the absence of unrelated daemon GC that emits no signal
in the captured output. Here, “clean” means no detected GC signal, not a
host-wide GC exclusion lease. Bootstrap `nix develop` can also trigger GC
outside the timed commands. Concurrent warm entries can emit ignored
SQLite-busy warnings despite successful exits; these remain diagnostic counts,
not GC signals.

The exact fix-round rerun command, after the original experiment, was:

```sh
nix develop --command python3 scripts/measure_build_backend.py \
  --target "$TASK_TARGET" --slot "$TASK_SLOT" --rerun-nix --gc-attempts 3
```

The controller hashes in `lifecycle_runs` identify the tested scripts; the Nix
rerun started from worktree head `17bc1f9b456129f6b9193ceafdf677ec10206c68`
with these controller edits. The Nix configuration and lockfile were unchanged.

## Pending backends and excluded scenarios

| Scenario | Status and reason |
| --- | --- |
| Four concurrent archive builds | Skipped: this host permits at most two concurrent builds for this task. |
| k3s cold/warm builds, test shards, first rustc | Pending: cluster access is excluded from this round. |
| tart cold/warm builds, first rustc | Pending: VM access is excluded from this round. |
| Eight concurrent Nix entries | Replaced by four under the task's host rules. |
| Tests from the archive | Not executed: this step measures archive production. |
| Truly cold Nix store/toolchain | Skipped: no cache purge or new toolchain build is authorized. |
| Cross-host or filesystem comparisons | Pending: this round measures only this Mac and APFS. |

## Resource-accounting limits

`time -l` maximum RSS and peak memory footprint describe the timed command's
resource accounting; they are not a simultaneous sum of the whole process tree.
The harness also samples the sum of RSS for the owned process group each second.
Zero in this sampled column means no one-second sample was collected, not zero
memory use. Sampling can miss short peaks and count shared pages more than once. Concurrent
cache work in the private daemon is outside those process groups. These values
guide initial lease estimates, not a proven memory limit. External machine
activity and warm OS caches remain sources of timing variation.

Target storage is `du -sk` immediately after each archive; compressed archive
storage is file length. APFS clones can share physical extents, so adding those
target sizes overstates exclusive physical usage. Exclusive extent usage and
system memory pressure are not measured. The disk admission guard uses actual
free space on the destination volume rather than a sum of `du` values.


## Results

Raw counters, tool versions and per-command arguments are in [the result JSON](run-broker-measurements.json). Wall times include command launch and observation overhead. All listed commands exited 0. Aggregates use only rows marked `aggregate_eligible`; raw tables retain excluded samples.

| Scenario | n | Wall samples (s) | Median (s) | Min–max (s) |
| --- | ---: | --- | ---: | --- |
| Cold, current settings | 1 | 140.77 | 140.77 | 140.77–140.77 |
| Seed at same path | 2 | 1.23, 1.17 | 1.20 | 1.17–1.23 |
| Seed at different path | 2 | 112.43, 133.63 | 123.03 | 112.43–133.63 |
| One-module touch | 2 | 80.04, 90.68 | 85.36 | 80.04–90.68 |
| Fresh source checkout | 2 | 106.32, 137.20 | 121.76 | 106.32–137.20 |
| Fresh checkout, empty target and warm sccache | 2 | 157.74, 180.46 | 169.10 | 157.74–180.46 |
| Tenth edit rebuild | 2 | 86.31, 88.14 | 87.22 | 86.31–88.14 |
| Single edited build | 2 | 88.19, 87.48 | 87.83 | 87.48–88.19 |
| Two edited builds, per build | 4 | 93.46, 93.59, 91.31, 91.75 | 92.61 | 91.31–93.59 |
| Full-debug cold | 1 | 179.85 | 179.85 | 179.85–179.85 |
| Full-debug touch | 2 | 101.64, 101.92 | 101.78 | 101.64–101.92 |
| Nix fresh evaluation | 1 | 2.5946 | 2.5946 | 2.5946–2.5946 |
| Nix warm sequential | 2 | 2.5890, 2.5869 | 2.5880 | 2.5869–2.5890 |
| Nix four concurrent, per entry | 8 | 3.7775, 3.7658, 3.7836, 3.8020, 4.0713, 4.2546, 4.0627, 4.1190 | 3.9324 | 3.7658–4.2546 |

Cold measurements have one sample each. The concurrent samples within a pair share machine conditions and are not independent repetitions; the pair itself is repeated twice.

### Per-command resource samples

RSS and footprint columns are bytes. Target storage is KiB; archive length is bytes. A dash means the metric does not apply. Process-group RSS is sampled at one-second intervals.

| Run | Wall s | Maximum RSS B | Peak footprint B | Sampled group peak RSS B | Target KiB | Archive B | Cargo lock messages | GC | Used |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- |
| cold | 140.77 | 9329737728 | 201557632 | 14772240384 | 2259804 | 179373747 | 0 | no | yes |
| same-1 | 1.23 | 204079104 | 192448064 | 202244096 | 2227400 | 179373747 | 0 | no | yes |
| different-1 | 112.43 | 9346449408 | 212141760 | 10077224960 | 2327968 | 179395308 | 0 | no | yes |
| touch-1 | 80.04 | 9508896768 | 205817472 | 10244833280 | 2260580 | 179373794 | 0 | no | yes |
| fresh-1 | 106.32 | 9432317952 | 206063360 | 9867280384 | 2327828 | 179367058 | 0 | no | yes |
| same-2 | 1.17 | 204488704 | 192841280 | 201998336 | 2227400 | 179373747 | 0 | no | yes |
| different-2 | 133.63 | 9373384704 | 200476288 | 9509634048 | 2327976 | 179395364 | 0 | no | yes |
| touch-2 | 90.68 | 9525493760 | 211437248 | 9690038272 | 2260580 | 179373574 | 0 | no | yes |
| fresh-2 | 137.20 | 9442951168 | 204048064 | 10162503680 | 2327828 | 179366967 | 0 | no | yes |
| edit-1-01 | 88.03 | 9503227904 | 209766080 | 10148954112 | 2260772 | 179373195 | 0 | no | yes |
| edit-1-02 | 87.57 | 9454125056 | 205506176 | 9564815360 | 2260964 | 179373198 | 0 | no | yes |
| edit-1-03 | 81.40 | 9416736768 | 209782464 | 10278207488 | 2261156 | 179372887 | 0 | no | yes |
| edit-1-04 | 79.56 | 9399812096 | 205981376 | 10205396992 | 2261348 | 179372900 | 0 | no | yes |
| edit-1-05 | 87.19 | 9419898880 | 203933312 | 9645342720 | 2261540 | 179373206 | 0 | no | yes |
| edit-1-06 | 87.55 | 9408348160 | 206292608 | 9669378048 | 2261732 | 179373166 | 0 | no | yes |
| edit-1-07 | 96.42 | 9363079168 | 213255872 | 10211803136 | 2261924 | 179373085 | 0 | no | yes |
| edit-1-08 | 101.01 | 9324658688 | 213436096 | 9522642944 | 2262116 | 179372966 | 0 | no | yes |
| edit-1-09 | 88.23 | 9391112192 | 214058688 | 10143645696 | 2262308 | 179372989 | 0 | no | yes |
| edit-1-10 | 86.31 | 9431875584 | 205948672 | 9875111936 | 2262500 | 179372894 | 0 | no | yes |
| edit-2-01 | 93.23 | 9363783680 | 213419712 | 9656336384 | 2262692 | 179373143 | 0 | no | yes |
| edit-2-02 | 89.45 | 9462153216 | 212469440 | 9650454528 | 2262884 | 179373030 | 0 | no | yes |
| edit-2-03 | 87.02 | 9528033280 | 213288704 | 9998991360 | 2263076 | 179373183 | 0 | no | yes |
| edit-2-04 | 82.79 | 9431384064 | 213354240 | 9577103360 | 2263268 | 179373193 | 0 | no | yes |
| edit-2-05 | 78.33 | 9393487872 | 211142336 | 9724903424 | 2263460 | 179372983 | 0 | no | yes |
| edit-2-06 | 88.10 | 9477046272 | 213239552 | 10530390016 | 2263652 | 179373216 | 0 | no | yes |
| edit-2-07 | 86.02 | 9446817792 | 211535552 | 10113024000 | 2263844 | 179372933 | 0 | no | yes |
| edit-2-08 | 87.30 | 9451569152 | 212584128 | 9691054080 | 2264036 | 179372818 | 0 | no | yes |
| edit-2-09 | 87.94 | 9262104576 | 211109568 | 9748774912 | 2264228 | 179372880 | 0 | no | yes |
| edit-2-10 | 88.14 | 9419948032 | 213305024 | 9526788096 | 2264420 | 179373204 | 0 | no | yes |
| serial-1 | 124.83 | 9493577728 | 213632768 | 9629351936 | 2332120 | 179391177 | 0 | no | yes |
| concurrent-1-0 | 94.96 | 9442312192 | 200902272 | 9602039808 | 2332300 | 179391317 | 4 | no | yes |
| concurrent-1-1 | 121.50 | 9320005632 | 207341312 | 12437422080 | 2332120 | 179390735 | 5 | no | yes |
| serial-2 | 88.99 | 9340518400 | 210060992 | 10393583616 | 2332492 | 179391222 | 0 | no | yes |
| concurrent-2-0 | 91.54 | 9452322816 | 204965504 | 9863856128 | 2332684 | 179391252 | 4 | no | yes |
| concurrent-2-1 | 92.16 | 9343336448 | 206538432 | 10194567168 | 2332300 | 179390856 | 5 | no | yes |
| full-debug-cold | 179.85 | 10586685440 | 207423104 | 10727194624 | 3391332 | 200856567 | 0 | no | yes |
| full-debug-touch-1 | 101.64 | 10364321792 | 208979712 | 10616324096 | 3391744 | 200856414 | 0 | no | yes |
| full-debug-touch-2 | 101.92 | 10519232512 | 208422592 | 10649387008 | 3391936 | 200856627 | 0 | no | yes |
| nix-fresh-eval-1 | 28.99 | 1572864 | 1098496 | 488964096 | — | — | 0 | yes | no |
| nix-warm-1 | 2.25 | 1572864 | 1098496 | 497303552 | — | — | 0 | no | no |
| nix-warm-2 | 1.95 | 1572864 | 1098496 | 375439360 | — | — | 0 | no | no |
| nix-four-a-1 | 2.87 | 1572864 | 1098496 | 446119936 | — | — | 0 | no | no |
| nix-four-a-2 | 2.82 | 1572864 | 1098496 | 448364544 | — | — | 0 | no | no |
| nix-four-a-3 | 2.57 | 1572864 | 1098496 | 480755712 | — | — | 0 | no | no |
| nix-four-a-4 | 2.87 | 1572864 | 1098496 | 445759488 | — | — | 0 | no | no |
| nix-four-b-1 | 2.62 | 1572864 | 1098496 | 460996608 | — | — | 0 | no | no |
| nix-four-b-2 | 2.64 | 1572864 | 1114944 | 461029376 | — | — | 0 | no | no |
| nix-four-b-3 | 2.62 | 1572864 | 1098496 | 460816384 | — | — | 0 | no | no |
| nix-four-b-4 | 2.62 | 1572864 | 1098496 | 460931072 | — | — | 0 | no | no |
| empty-checkout-1 | 157.74 | 9459220480 | 210994880 | 9706930176 | 2259008 | 179404152 | 0 | no | yes |
| empty-checkout-2 | 180.46 | 9387245568 | 211371712 | 9512927232 | 2257988 | 179404129 | 0 | no | yes |
| steady-serial-1 | 88.19 | 9348513792 | 210273984 | 9519513600 | 2332876 | 179391143 | 0 | no | yes |
| steady-concurrent-1-0 | 93.46 | 9439379456 | 211175104 | 10104307712 | 2333068 | 179391314 | 4 | no | yes |
| steady-concurrent-1-1 | 93.59 | 9407905792 | 201557632 | 9603661824 | 2332492 | 179390942 | 5 | no | yes |
| steady-serial-2 | 87.48 | 9501999104 | 214222528 | 9623535616 | 2333260 | 179391318 | 0 | no | yes |
| steady-concurrent-2-0 | 91.31 | 9402826752 | 208209600 | 9609445376 | 2333452 | 179391283 | 4 | no | yes |
| steady-concurrent-2-1 | 91.75 | 9363013632 | 209618624 | 10253172736 | 2332684 | 179390795 | 5 | no | yes |
| nix-fresh-eval-fix1-r1-a1-1 | 2.59 | 1572864 | 1098496 | 447234048 | — | — | 0 | no | yes |
| nix-warm-fix1-r1-a1-1 | 2.59 | 1572864 | 1098496 | 448020480 | — | — | 0 | no | yes |
| nix-warm-fix1-r1-a1-2 | 2.59 | 1572864 | 1098496 | 449380352 | — | — | 0 | no | yes |
| nix-four-a-fix1-r1-a1-1 | 3.78 | 1572864 | 1098496 | 450920448 | — | — | 0 | no | yes |
| nix-four-a-fix1-r1-a1-2 | 3.77 | 1572864 | 1098496 | 451395584 | — | — | 0 | no | yes |
| nix-four-a-fix1-r1-a1-3 | 3.78 | 1572864 | 1098496 | 451543040 | — | — | 0 | no | yes |
| nix-four-a-fix1-r1-a1-4 | 3.80 | 1572864 | 1098496 | 452542464 | — | — | 0 | no | yes |
| nix-four-b-fix1-r1-a1-1 | 4.07 | 1572864 | 1098496 | 411942912 | — | — | 0 | no | yes |
| nix-four-b-fix1-r1-a1-2 | 4.25 | 1572864 | 1098496 | 498122752 | — | — | 0 | no | yes |
| nix-four-b-fix1-r1-a1-3 | 4.06 | 1572864 | 1098496 | 412155904 | — | — | 0 | no | yes |
| nix-four-b-fix1-r1-a1-4 | 4.12 | 1572864 | 1098496 | 408092672 | — | — | 0 | no | yes |

### Sccache deltas

Hit rate is hits / (hits + misses). “n/a” means no cacheable requests. Reasons below are the exact reason keys emitted by this sccache version, restricted to nonzero deltas. Concurrent-pair deltas belong to the entire pair.

| Run or pair | Compile requests | Hits | Misses | Hit rate | Non-cacheable reason counts |
| --- | ---: | ---: | ---: | --- | --- |
| cold | 371 | 0 | 227 | 0.00% | `-`: 3, `-o`: 3, `crate-type`: 125, `missing input`: 5 |
| same-1 | 1 | 0 | 0 | n/a | `-`: 1 |
| different-1 | 78 | 1 | 0 | 100.00% | `-`: 1, `crate-type`: 76 |
| touch-1 | 3 | 0 | 0 | n/a | `crate-type`: 3 |
| fresh-1 | 78 | 1 | 0 | 100.00% | `-`: 1, `crate-type`: 76 |
| same-2 | 1 | 0 | 0 | n/a | `-`: 1 |
| different-2 | 78 | 1 | 0 | 100.00% | `-`: 1, `crate-type`: 76 |
| touch-2 | 3 | 0 | 0 | n/a | `crate-type`: 3 |
| fresh-2 | 78 | 1 | 0 | 100.00% | `-`: 1, `crate-type`: 76 |
| edit-1-01 | 3 | 0 | 0 | n/a | `crate-type`: 3 |
| edit-1-02 | 3 | 0 | 0 | n/a | `crate-type`: 3 |
| edit-1-03 | 3 | 0 | 0 | n/a | `crate-type`: 3 |
| edit-1-04 | 3 | 0 | 0 | n/a | `crate-type`: 3 |
| edit-1-05 | 3 | 0 | 0 | n/a | `crate-type`: 3 |
| edit-1-06 | 3 | 0 | 0 | n/a | `crate-type`: 3 |
| edit-1-07 | 3 | 0 | 0 | n/a | `crate-type`: 3 |
| edit-1-08 | 3 | 0 | 0 | n/a | `crate-type`: 3 |
| edit-1-09 | 3 | 0 | 0 | n/a | `crate-type`: 3 |
| edit-1-10 | 3 | 0 | 0 | n/a | `crate-type`: 3 |
| edit-2-01 | 3 | 0 | 0 | n/a | `crate-type`: 3 |
| edit-2-02 | 3 | 0 | 0 | n/a | `crate-type`: 3 |
| edit-2-03 | 3 | 0 | 0 | n/a | `crate-type`: 3 |
| edit-2-04 | 3 | 0 | 0 | n/a | `crate-type`: 3 |
| edit-2-05 | 3 | 0 | 0 | n/a | `crate-type`: 3 |
| edit-2-06 | 3 | 0 | 0 | n/a | `crate-type`: 3 |
| edit-2-07 | 3 | 0 | 0 | n/a | `crate-type`: 3 |
| edit-2-08 | 3 | 0 | 0 | n/a | `crate-type`: 3 |
| edit-2-09 | 3 | 0 | 0 | n/a | `crate-type`: 3 |
| edit-2-10 | 3 | 0 | 0 | n/a | `crate-type`: 3 |
| serial-1 | 77 | 1 | 0 | 100.00% | `crate-type`: 76 |
| concurrent-1 | 80 | 1 | 0 | 100.00% | `crate-type`: 79 |
| serial-2 | 3 | 0 | 0 | n/a | `crate-type`: 3 |
| concurrent-2 | 6 | 0 | 0 | n/a | `crate-type`: 6 |
| full-debug-cold | 371 | 1 | 226 | 0.44% | `-`: 3, `-o`: 3, `crate-type`: 125, `missing input`: 5 |
| full-debug-touch-1 | 4 | 0 | 0 | n/a | `-`: 1, `crate-type`: 3 |
| full-debug-touch-2 | 3 | 0 | 0 | n/a | `crate-type`: 3 |
| empty-checkout-1 | 371 | 2 | 225 | 0.88% | `-`: 3, `-o`: 3, `crate-type`: 125, `missing input`: 5 |
| empty-checkout-2 | 371 | 2 | 225 | 0.88% | `-`: 3, `-o`: 3, `crate-type`: 125, `missing input`: 5 |
| steady-serial-1 | 3 | 0 | 0 | n/a | `crate-type`: 3 |
| steady-concurrent-1 | 6 | 0 | 0 | n/a | `crate-type`: 6 |
| steady-serial-2 | 3 | 0 | 0 | n/a | `crate-type`: 3 |
| steady-concurrent-2 | 6 | 0 | 0 | n/a | `crate-type`: 6 |

### Concurrent groups

The group wall interval excludes the following disk-size scan. The sum of per-build sampled peaks bounds the combined RSS observed at those sample times. Unobserved peaks and daemon work remain outside this accounting.

| Group | Wall s | GC | Used |
| --- | ---: | --- | --- |
| serial-1 | 124.85 | no | yes |
| concurrent-1 | 121.52 | no | yes |
| serial-2 | 89.01 | no | yes |
| concurrent-2 | 92.18 | no | yes |
| nix-fresh-eval | 29.00 | yes | no |
| nix-warm | 4.23 | no | no |
| nix-four-a | 2.90 | no | no |
| nix-four-b | 2.66 | no | no |
| steady-serial-1 | 88.20 | no | yes |
| steady-concurrent-1 | 93.62 | no | yes |
| steady-serial-2 | 87.50 | no | yes |
| steady-concurrent-2 | 91.77 | no | yes |
| nix-fresh-eval | 2.61 | no | yes |
| nix-warm | 5.21 | no | yes |
| nix-four-a | 3.83 | no | yes |
| nix-four-b | 4.27 | no | yes |

### APFS clone-copy samples

| Destination relative to scratch parent | Wall s |
| --- | ---: |
| `m3081-seed` | 0.392 |
| `m3081` | 0.398 |
| `m3081-relocated-source-1` | 0.382 |
| `m3081-relocated-1` | 0.393 |
| `m3081-fresh-1` | 0.405 |
| `m3081` | 0.387 |
| `m3081-relocated-source-2` | 0.400 |
| `m3081-relocated-2` | 0.406 |
| `m3081-fresh-2` | 0.455 |
| `m3081-parallel-source-0` | 0.520 |
| `m3081-parallel-0` | 0.469 |
| `m3081-parallel-source-1` | 0.506 |
| `m3081-parallel-1` | 0.483 |

### Nix diagnostics

| Run | SQLite-busy messages | Auto-GC messages | GC-lock messages | GC | Used |
| --- | ---: | ---: | ---: | --- | --- |
| nix-fresh-eval-1 | 0 | 1 | 1 | yes | no |
| nix-warm-1 | 0 | 0 | 0 | no | no |
| nix-warm-2 | 0 | 0 | 0 | no | no |
| nix-four-a-1 | 2 | 0 | 0 | no | no |
| nix-four-a-2 | 2 | 0 | 0 | no | no |
| nix-four-a-3 | 0 | 0 | 0 | no | no |
| nix-four-a-4 | 2 | 0 | 0 | no | no |
| nix-four-b-1 | 1 | 0 | 0 | no | no |
| nix-four-b-2 | 2 | 0 | 0 | no | no |
| nix-four-b-3 | 1 | 0 | 0 | no | no |
| nix-four-b-4 | 2 | 0 | 0 | no | no |
| nix-fresh-eval-fix1-r1-a1-1 | 0 | 0 | 0 | no | yes |
| nix-warm-fix1-r1-a1-1 | 0 | 0 | 0 | no | yes |
| nix-warm-fix1-r1-a1-2 | 0 | 0 | 0 | no | yes |
| nix-four-a-fix1-r1-a1-1 | 2 | 0 | 0 | no | yes |
| nix-four-a-fix1-r1-a1-2 | 2 | 0 | 0 | no | yes |
| nix-four-a-fix1-r1-a1-3 | 1 | 0 | 0 | no | yes |
| nix-four-a-fix1-r1-a1-4 | 1 | 0 | 0 | no | yes |
| nix-four-b-fix1-r1-a1-1 | 1 | 0 | 0 | no | yes |
| nix-four-b-fix1-r1-a1-2 | 1 | 0 | 0 | no | yes |
| nix-four-b-fix1-r1-a1-3 | 2 | 0 | 0 | no | yes |
| nix-four-b-fix1-r1-a1-4 | 2 | 0 | 0 | no | yes |

## Implications for the run-broker ADR

- **Retain worktree targets and stable paths.** APFS clone copies took
  0.382–0.520 seconds, but only the same-path seed avoided rebuilding the
  application. Its archive runs took 1.17 and 1.23 seconds, versus 112.43 and
  133.63 seconds after source/target relocation. A seed is not portable merely
  because it is cheap to copy. Preserve source paths, target paths, timestamps,
  toolchain, lockfile, features and debug settings when evaluating trusted CI
  seeds. These local experimental seeds are not trusted cache publications.
- **Inject inner parallelism and admit by memory.** Every build here used two
  Cargo jobs with incremental compilation disabled. The initial cold run reached
  13.76 GiB sampled process-group RSS; the daemon was outside that accounting.
  An admission estimate must cover at least that observed demand plus unmeasured
  peaks and shared-service memory. These samples do not establish a safe hard
  limit or a tail percentile. Nextest worker injection remains necessary for
  actual test runs, whose memory and runtime are not measured by archiving.
- **Two warm builds can overlap usefully with explicit limits.** Steady single
  group walls were 88.203 and 87.497 seconds; paired walls were 93.616 and 91.768
  seconds. That is 1.88× and 1.91× the corresponding single-run throughput, with
  four Cargo jobs admitted in total. The sums of separately sampled per-build
  RSS peaks were 18.35 and 18.50 GiB; these are not measured simultaneous peaks.
  Each pair emitted nine package-cache lock messages even with offline Cargo.
  Separate targets avoid sharing their build outputs, but do not remove shared
  package-cache contention. No conclusion about four builds or two cold builds
  follows from this warm comparison.
- **Keep the current debug settings in storage estimates.** The current cold
  target was 2,259,804 KiB, versus 3,391,332 KiB with full debug information:
  33.37% less target storage. Archives were 179,373,747 versus 200,856,567 bytes,
  a 10.70% reduction. Cold-target walls were 140.77 versus 179.85 seconds, and
  touch runs were 80.04/90.68 versus 101.64/101.92 seconds. Timing samples are
  neither randomized nor identically cold-cache trials, so the wall differences
  are observations rather than a measured causal speedup.
- **A warm sccache directory is not a replacement for a target.** Both
  empty-target fresh checkouts reported two C/C++ hits, 225 Rust misses and no
  Rust hits: a 0.88% aggregate cacheable-request hit rate. Their walls were
  157.74 and 180.46 seconds. The pinned Rust hasher includes the compiler working
  directory and the non-excluded `CARGO_*` environment, including
  `CARGO_TARGET_DIR`. Changing that target variable is sufficient to change its
  key. This source-based explanation is consistent with the misses; the two
  changing paths were not individually ablated. See the
  [sccache 0.18.0 Rust key implementation](https://github.com/mozilla/sccache/blob/v0.18.0/src/compiler/rust.rs#L1486-L1525).
  The seeded edit loop instead reused dependency artifacts through Cargo and
  usually offered sccache no cacheable work. Binary and procedural-macro crates
  that invoke the system linker are outside this version's Rust cache support;
  see its [Rust limitations](https://github.com/mozilla/sccache/blob/v0.18.0/docs/Rust.md).
  The observed reason codes, including `crate-type`, `-o`, `-` and
  `missing input`, are preserved above. No path-normalization change was made.
- **Separate environment preparation from each admitted command.** After GC
  quarantine, fresh evaluation took 2.5946 seconds; warm entries took
  2.5869–2.5890 seconds; four-way entries took 3.7658–4.2546 seconds each.
  All eight replacement concurrent entries emitted twelve ignored SQLite-busy
  warnings in total, and all exited 0. The excluded original 28.99-second
  sample included automatic GC and a GC-lock wait, so it cannot establish an
  evaluation/cache speedup. These signal-free samples do not establish a large
  sequential warm-start advantage either. Shared-cache contention still motivates
  measuring a cached `print-dev-env` per lockfile next. Sourcing that output was
  not timed here, so no numerical speedup for that proposed design is claimed.
- **Quota retained targets and expire outputs separately.** Twenty comment-edit
  rebuilds had a median of 87.56 seconds and a 78.33–101.01-second range. Target
  allocation grew by 3,648 KiB from the first to the twentieth edit sample.
  That is a narrow dirty-rebuild workload, not evidence that arbitrary edits,
  changed dependencies or changed profiles cannot grow a target. Final target
  size and archive length also exclude peak temporary linker storage. Lease
  sizing and output retention must account for both.

## Validation and retained scratch

The main command and the append-controls command both completed end to end with
exit 0: 46 archive commands and 11 original Nix entries. The fix-round Nix rerun
also exited 0 and added 11 signal-free replacements. The raw resource table has
68 rows; aggregates use 57 eligible rows (46 historical Cargo and 11 new Nix).
The 42 sccache and 13 copy observations retain their original numbers. Both commands write their own
result files. Static Python compilation and the existing-prefix rejection
check passed; that rejection deliberately returns 1 and preserves the results.
The initial GNU-copy failure and the append-directory failure both returned 1
before being corrected. A second exploratory run was interrupted after finding
that a prior touch would invalidate the next same-path seed sample; the final
harness restores the original timestamp. None of those exploratory timings is
included in the result tables. Raw exploratory outputs remain private.

The regression command is:

```sh
mkdir -p "$SCRATCH/m3081-fix1-tests"
BUILD_BACKEND_TEST_ROOT="$SCRATCH/m3081-fix1-tests" \
  nix develop --command python3 -m unittest scripts.test_build_backend_control -v
```

All eight tests passed with exit 0: three GC-policy regressions and five real
private-daemon cases (normal exit, exception, SIGINT, SIGTERM, and refusal to
adopt an occupied endpoint). The exception and signal probes intentionally exit
1, 130 and 143; the test driver verifies those exits, child termination, owned
TMPDIR, successful cache stop and a non-listening endpoint. Their fixtures are
retained under the listed test root. No user-default cache service is addressed.

The final validation command is `nix develop --command just lint`, with two Cargo
jobs, incremental compilation disabled, the task-owned target, the GUI fixture,
and the compiler wrapper disabled. The first-round run passed with exit 0, including TypeScript,
client lint, formatting and all three Clippy passes. The fix-round run inside
`nix develop` also passed with exit 0 after the controller changes. The public-host gate and
whitespace checks also passed. This uses the narrower validation explicitly
requested for a docs/script spike; no Rust production code changes. Lint uses
the original task target after measurement, so its retained contents include
additional checking artifacts beyond the timed size snapshots.


The review fix increases runtime controller code from 317 to 503 nonblank,
noncomment lines, excluding docstrings and tests (361 raw lines before; 572 raw
runtime lines across the harness and lifetime helper afterward). Rust production
code changes remain zero. This is the review-fix round; the increase is reported
for the architect's split decision rather than bypassing the experiment fixes.

All scratch roots remain under the operator-supplied `$SCRATCH`. No scratch
root was deleted; the admission owner handles reclamation. The JSON `scratch`
array is the exhaustive relative-name inventory, including excluded attempts:

```text
m3081
m3081-attempt1
m3081-attempt1-results
m3081-attempt1-sccache
m3081-attempt1-source
m3081-attempt2
m3081-attempt2-fresh-1
m3081-attempt2-fresh-source-1
m3081-attempt2-relocated-1
m3081-attempt2-relocated-source-1
m3081-attempt2-results
m3081-attempt2-retired-1
m3081-attempt2-sccache
m3081-attempt2-seed
m3081-attempt2-source
m3081-empty-checkout-1
m3081-empty-checkout-2
m3081-empty-source-1
m3081-empty-source-2
m3081-fix1-tests
m3081-fresh-1
m3081-fresh-2
m3081-fresh-source-1
m3081-fresh-source-2
m3081-full-debug
m3081-parallel-0
m3081-parallel-1
m3081-parallel-source-0
m3081-parallel-source-1
m3081-relocated-1
m3081-relocated-2
m3081-relocated-source-1
m3081-relocated-source-2
m3081-results
m3081-retired-1
m3081-retired-2
m3081-sccache
m3081-seed
m3081-source
```
