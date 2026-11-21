# Capacity publication correctness and timing

The wire correctness test
`near_global_capacity_publication_writes_one_record_per_claim_and_finish`
compares an empty journal with one holding 184,319 claims. Seeding completes
before the command is sent and has no local deadline. A test-only stream counter
measures actual bytes written, including overwrites, at each pre-sync barrier.
Each claim and finish must write exactly its single serialized record. After
normalizing the decimal widths of the timestamp and payload digest, write sizes
must agree across both journal sizes and remain below 2 KiB for this fixture.
The real authenticated wire path must acknowledge success before publishing the
caused permission-close event.

Frame waits service the owner and check worker liveness. Unexpected ACKs,
publication frames, EOF and worker exit fail the test. The nextest default
420-second termination ceiling bounds a silent live producer; it is a hang
backstop, not a latency target. Frame deadlines use the injected `TestClock`:
loaded machines cannot consume their five virtual seconds. A separate silent
producer test advances that clock and verifies the wait panics at the deadline.
No application clock is advanced in the correctness test and no runtime timeout
behavior is under test. Other wire fixtures retain their existing
communication deadlines; the capacity seed has no other callers.

Run the separate, ignored reporting probe with:

```sh
just capacity-publication-probe
```

It reports command-to-ACK and ACK-to-publication elapsed times without asserting
latency. Default nextest and CI runs exclude it. The correctness test does not
print timing results.
