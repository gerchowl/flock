# ADR-0046: Do not adopt SlateDB now

- Status: Accepted
- Date: 2026-11-02
- Issues: [#2845](https://github.com/gerchowl/flock/issues/2845)
- Decision owner: repository owner; decided on 2026-11-02 (close the spike as
  not-now and record revisit triggers).
- Builds on: ADR-0009 (no second trust domain, no interval polling), ADR-0026
  (never memory-only mail), ADR-0031 (storage), ADR-0039 (vigil telemetry),
  ADR-0043 (fail-closed node status), ADR-0045 (durable work records).

## Context

Spike #2845 asked whether SlateDB, an embedded LSM key-value store that keeps
its data in object storage, should back some or all of flock's persistent
state. Three independent reviewers (storage engine, distributed systems and
fencing, maintainer and operations) read the proposal against SlateDB 0.17.0
(2026-09-29) and flock `origin/dev`. All three said not to adopt it now.

Facts the decision rests on, with their source:

- **Latency.** SlateDB durable writes are bounded below by the 100 ms
  `flush_interval` plus an object PUT. Flock's current local fsync and SQLite
  paths measure p50 about 4 ms, p99 under 12 ms (ADR-0031), and 83 us p50 for
  the mesh store.
- **Query shape.** The mesh mail store depends on SQL indexes and
  `BEGIN IMMEDIATE` state machines.
- **Pre-1.0 churn.** SlateDB promises on-disk format compatibility only between
  adjacent releases, and releases have come out monthly since about April
  (SlateDB release history). The API changed too: `await_durable` was removed
  in 0.16.0. A user who skips a few flk releases is exposed.
- **Footprint.** 71 to 116 new crates against today's 344, even with a
  local-filesystem-only build; the default build adds reqwest, rustls,
  aws-lc-rs and zstd, with C/cmake build inputs.
- **Offline versus shared.** The local-filesystem backend works offline but no
  other node can read it. A remote store can be shared, but a laptop cannot
  write to it while offline. Bucket keys would be a new secret class, the
  "second trust domain" ADR-0009 rejected.
- **Handoff fencing.** Live handoff today transfers single-writer ownership of
  `mesh-mail.sqlite` cooperatively (`runtime_store::suspend`,
  then reopen after `wait_committed` in the handoff import path). SlateDB fences on open regardless of whether the
  old writer handed over. If a handoff crashes after the importer opened the
  database, the two processes can keep fencing each other.
- **Silent mail loss.** If someone sets `await_durable=false` to fit the
  Stop-hook budget, mail custody the old server acked from memory is lost when
  the importer opens the database, with no error. That breaks ADR-0026's
  "never memory-only mail" rule.
- **Readers.** Managed-checkpoint `DbReader`s write checkpoints into the
  writer's manifest, so a laptop that sleeps while holding one blocks GC.
  `FollowLatest` mode avoids that, but reads can fail after GC. Readers poll
  the manifest every 10 s by default, the interval polling ADR-0009 rejected,
  and a durable but stale node row contradicts ADR-0043's fail-closed
  `online:false`.
- **Local backend durability.** `object_store`'s `LocalFileSystem` has
  `fsync: false` by default. Related open bugs: object_store#888
  (directory-entry durability) and #767 (deletes not fsynced).

## Decision

Flock does not adopt SlateDB now. Verdict per store:

| Store | Verdict | Reason |
| --- | --- | --- |
| Event log | Keep as is | Local fsync at p50 about 4 ms, p99 under 12 ms (ADR-0031) beats the 100 ms flush interval plus PUT; SlateDB is clearly worse. |
| Mesh mail store | Keep SQLite | Needs SQL indexes and `BEGIN IMMEDIATE` state machines; 83 us p50 today; cooperative handoff works. |
| Inbox / Stop-hook path | Keep as is | The hook budget cannot absorb a durable-write floor of 100 ms plus PUT, and the workaround (`await_durable=false`) violates ADR-0026. |
| Atomic-write stores | Keep; consolidate later | SlateDB is clearly worse. A cheaper path is folding them into the existing rusqlite database (see Follow-ups). |
| ADR-0045 work records and receipts | Keep local; revisit survivability separately | The only candidate use, for records that should outlive a dead host. Periodic SQLite `VACUUM INTO` snapshots to a peer or fleet storage cover it without a new dependency tree. |
| Cross-node registry | Keep the live, fail-closed model | A shared durable store brings stale rows, 10 s manifest polling and bucket credentials, against ADR-0009 and ADR-0043. |

Log and telemetry export is a separate concern owned by ADR-0039 (vigil).
Vigil is not a state store, and this ADR does not change that.

## Rejected alternatives

1. **SlateDB wholesale.** Worse on latency, query shape, footprint and
   upgrade safety for every store above (see Context).
2. **SlateDB for the cross-node registry now.** Gives sharing, but needs bucket
   keys (a second trust domain, ADR-0009), makes offline writes impossible on a
   remote backend, polls on an interval, and can serve a durable but stale row
   where ADR-0043 requires fail-closed `online:false`.
3. **Postgres with sqlx for flock's in-process state.** A server per host, or
   one central database, is a single point of failure and a second trust
   domain, which ADR-0009 rejected. sqlx compile-time checks need a live
   database or a committed offline cache. SQLite already provides SQL,
   transactions and WAL in-process. A fleet-level Postgres service outside
   flock (for example the usage ledger, #588) is a different question and out
   of scope for this ADR.

## Consequences

- No new dependency tree, no new on-disk format, no new secret class.
- Dead-host survivability of work records stays unsolved until the follow-up
  spike; no store lets one node read another's state while it is down.
- The single-writer cooperative handoff of `mesh-mail.sqlite` is unchanged.

## Revisit triggers

Reopen the question if any of these happens:

- SlateDB RFC 0030 (pluggable or local WAL) ships.
- object_store#888 is fixed.
- Flock gains a concrete requirement to read a node's state while that node is
  down.
- A measured local-backend durable-write p99 shows SlateDB within the budget of
  the store in question.

## Follow-ups

- A separate spike (to be filed) to fold the atomic-write stores into the
  existing rusqlite database ("one engine, one set of crash rules") and to
  evaluate periodic SQLite snapshots (`VACUUM INTO`) of ADR-0045 work records
  to a peer or fleet storage.
