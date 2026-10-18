# Supervised Codex reader fixtures (#1857)

`src/codex_reader/segment/worker/tests.rs` starts the real worker with an
Admission built from the actual local-file validator. The controller retains
Binding/Ownership; the worker owns the file/parser state only while processing
a bounded unit. It sends that owned state back rather than cloning a reducer or
letting a background thread mutate the ownership registry. Only still-current
capabilities may commit admission or deliver observations. A local PID/start
identity check runs before dispatch and again before accepting a result.

The worker has single-slot request, result and coalesced notification channels,
with only one outstanding unit. Full/disconnected channels fail without a
blocking send. `tick` is nonblocking. `wait` is optional for an off-render
controller and uses the remaining dispatch budget, not a new wait deadline.
Failed work clears cached evidence and rejects the pending claim or invalidates
the trusted source. Writer exit also releases the known ownership claim. Stale
callbacks cannot reject a replacement claim or restore retired/revoked evidence.
The public `stop` path records source loss before cancellation; controllers must
call it before dropping/moving a live reader. Drop also cancels and disconnects
work as a last cleanup measure, without accessing the separate Binding registry.

## Cadence and resource policy

Continue each pass at STEP_INTERVAL (10 ms), then pause EOF_INTERVAL (250 ms)
after successful EOF. These constants are independent of Codex lifecycle event
frequency. Runtime must schedule next_due plus result notifications, rather than
tie this to rendering or repeatedly wake a busy reader. With a 1 MiB work budget,
a 1 GiB pass needs about 1024 units (at least 10.24 seconds), plus I/O/controller
scheduling and the preceding EOF pause. Smaller work budgets increase this
latency proportionally. There is no constant-time EOF promise and no total-file
size ceiling. A continuously growing history may remain catching up.

WORK_TIMEOUT is five seconds for each dispatched unit, including notification
and controller delivery. It is not a five-second pass deadline or a lifecycle
silence lease: successful prefix chunks and empty EOF checks count as flk work
progress. Long histories can take many healthy units. The controller supplies
monotonic time; progress/EOF timestamps mark delivery, while lifecycle boundary
timestamps retain their supplied reader observation time and never renew on EOF.
A quiet start holds working even if its end was lost; a quiet completion holds
idle. Evidence age is diagnostic. Dead/disconnected readers fail immediately
when observed; an alive but silent producer fails at its outstanding-work deadline.

There are at most MAX_READERS (128) native reader threads process-wide, including
ones abandoned while blocked in kernel filesystem calls. Stop/disconnect never
joins a blocked reader. Its permit is released only by actual thread exit, so
restarts cannot allocate unlimited abandoned threads. Capacity or spawn failure
is explicit, preserving the current launch's trust history. Each active reader
retains bounded record/turn state and at most one work unit, using its supplied
Limits. No automatic retry, replacement worker or eviction occurs. Kernel I/O
is not made nonblocking by the byte budget; a stuck call holds its permit until
it returns or the process exits. Shared-pool scheduling, if desired, belongs to #1664 runtime policy, not a reason to weaken this cap or health check.

## Acceptance and regression evidence

Tests use 7/13-byte work units, private local files, a genuinely owned scripted
shell writer and supplied monotonic clocks. They cover multi-step trust admission,
EOF versus prefix progress, quiet working/idle after hours, a missing end,
source mutation, writer exit/start mismatch, dead/disconnected readers, an alive
silent producer, queue backpressure, bounded notification failure and shutdown.
A silent-producer transport fixture holds real request/result channels open on
an owned thread until explicitly released; start/stop acknowledgements have a
five-second failure ceiling. There are no sleeps, retries or product test hooks.

The capacity test reserves the remaining permits without starting 128 threads,
then proves a live blocked producer keeps its permit through stop and that new
construction fails closed until the producer actually exits. Replacement/retirement
and duplicate-live-owner tests show old callbacks cannot regain authority. Moves
retire the source, reject a transferred compaction reducer at another owner,
reject a transcript outside the destination home, and require a new locally
validated candidate plus the destination's attributable start. Compaction keeps
its existing boundary only through the owned handoff and verified admission.

Fixture helpers are shared with the segment tests; their wider visibility stays
inside the test module. No model, operator profile or live server is accessed.
This foundation does not enable lifecycle authority or consumer/screen behavior;
that integration remains #1664. #1663/#1479 remain open for the complete scope.
