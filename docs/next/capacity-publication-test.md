# Capacity publication correctness and timing

The wire correctness test
`near_global_capacity_publication_writes_one_record_per_claim_and_finish`
compares an empty journal with one holding 184,319 claims. Seeding completes
outside deadlines. The entire fixture runs with a task-scoped `TestClock` for
harness waits, including construction, worker readiness, prompt delivery,
WebSocket upgrade and sends, frame reads, and worker teardown. The same scoped
deadline helper serves the wire and structured-worker test harnesses. Other
tests retain their existing default harness deadlines.

A test-only filesystem boundary replaces the journal module's standard and
platform file interfaces. Its raw files remain private. Every write through
`File`, cloned files, `OpenOptions`, `std::fs::write`, and `std::fs::copy` reaches
the same directory observer. Rename updates existing handles' destinations;
replacing a `Stream` cannot reset the observer. A separate boundary test checks
reopened handles, copying and writes after replacement.

Each foreground claim and finish must write exactly its single serialized
record, below 2 KiB for this fixture. After normalizing the decimal widths of the
timestamp and payload digest, write sizes must agree across journal sizes.
Background compaction has a separate budget: it may copy the retained log once,
including catch-up, and may publish one rename. Writes to other files fail the
assertion. The test awaits publication before teardown, so background copying
does not compete with the production journal-drain caller budget.

The compaction-race test holds claim sync with an owned channel, releases it,
and waits for the snapshot rename before allowing the owner to consume the
claim. Finish must then use the replacement file and retain the same observer.
This forces the race without sleeps. Both tests exercise the authenticated wire
path and require the successful ACK before its caused permission-close event.

The nextest default 420-second termination ceiling remains a hang backstop.
Silent-producer tests advance the injected clock to verify failure of wire and
worker harness waits. Production deadlines remain unchanged: journal acquisition
has a one-second budget on an empty journal before seeding; app revocation and
transport shutdown each have five seconds; journal drain has a one-second caller
budget. Near-capacity startup replay is tracked separately in #3207. The real structured
worker also retains its two-second version probe before seeding; five-second I/O
completion plus two-second cancellation wait during shutdown; and process cleanup
graces of 150 ms for EOF, 250 ms for TERM, and two seconds for KILL confirmation.
The fixture configures the existing worker stall threshold at 60 seconds.

Run the ignored, non-gating timing probe with:

```sh
just capacity-publication-probe
```

It reports command-to-ACK and ACK-to-publication elapsed times without asserting
latency. Default nextest and CI runs exclude it. Correctness tests neither
measure nor print elapsed times.
