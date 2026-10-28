# ADR-0045: Durable work records and server-owned supervision loops

- Status: Proposed
- Date: 2026-10-29
- Issues: [#2718](https://github.com/gerchowl/flock/issues/2718), informed by [#819](https://github.com/gerchowl/flock/issues/819), [#663](https://github.com/gerchowl/flock/issues/663), [#1044](https://github.com/gerchowl/flock/issues/1044), [#965](https://github.com/gerchowl/flock/issues/965), [#1600](https://github.com/gerchowl/flock/issues/1600) and [#1603](https://github.com/gerchowl/flock/issues/1603).
- Decision owner: repository owner; the choices below require explicit acceptance.
- Builds on: ADR-0027/0028 (delegation, observation and waits), ADR-0029 (agent-facing contracts), ADR-0030/0031 (goals and storage), ADR-0034/0035 (authority and resident owners), ADR-0036 (structured sessions), ADR-0037 (bounded tests), ADR-0038 §8 (work projection and records).
- Implemented: none of this proposal. The baseline below describes inspected code at `b2519c2c49b08498d75ac427a182ac2298ab49c2`.

## Context and placement

The owner reports a compaction with no surviving waiter, a red landing batch
unnoticed for roughly sixteen hours, and repeated manual waiter restarts
([#2718](https://github.com/gerchowl/flock/issues/2718)). This ADR does not
claim to have reproduced that incident or identified which private script died.
It specifies the missing server guarantees that would make such silence visible.

This is a separate ADR because work identity, persistence, subscription ownership
and acknowledgement span ADR-0030/0031 and ADR-0038, beyond ADR-0036's session and
transport boundary. If accepted, it extends those records in the named areas; it does not silently supersede their authority, stop ladder or wire contracts.
In particular, ADR-0038 §8 remains Proposed, and #1603's record phase is not
implicitly approved by this document. D1 and D7 below are its explicit gates.

## Inspected baseline: code, rather than roadmap

| Current behaviour | Evidence in this tree |
| --- | --- |
| One goal record is indexed by agent ID. Text, check commands, free-form status, checklist items with optional delegate owners, pause, met and latest verdict exist. It is not a general stable-ID work tree with typed PR/receipt evidence. | [`GoalRecord` and `GoalSubtask`](../../src/api/schema.rs), [`GoalIndex::apply`](../../src/app/goals.rs) |
| Startup folds goals with delegation data. Open goals are re-issued on startup and rotation; met goals stop being re-issued and can rotate out. This preserves an open record, not unlimited verdict history. | [`reissue_open_records` and `reissue_records_after_rotation`](../../src/app/delegation.rs), [`reissue_events`](../../src/app/goals.rs) |
| A durability boundary is missing for callers: `record_goal_event` applies the fold before `emit_event`; `EventHub::push` returns no result and logs an append failure while still publishing the event in memory. An append normally calls `sync_data`, but callers cannot infer success from the response. | [`record_goal_event`](../../src/app/goals.rs), [`push` and `PersistentLog::append`](../../src/api/event_hub.rs) |
| `goal.arm/status/pause/resume/stop` and `flk goal` exist. Writes attest local callers; an agent may arm its own goal but cannot pause/resume/stop it. Supervisor edges and operator authority matter. The file itself describes ancestry-based attestation as cooperative. | [`goal_writer` and handlers](../../src/app/api/goals.rs), [`run_goal_command`](../../src/cli/goal.rs) |
| Held event reads and `wait.park` report waiting state. Parks have a mandatory deadline, a 24-hour ceiling and a 16-park per-agent cap. The registry is memory-only; its tick expires waits. This is not durable recurring supervision. | [`WaitRegistry`, `handle_wait_park` and `tick_waits`](../../src/app/waits.rs) |
| Park grammar accepts delegate phases and reply correlations. PR predicates and “any delegate” predicates are explicitly unsupported; registration is not proof of a live event-to-wake implementation. | [`WaitPredicate::parse`](../../src/api/wait_predicate.rs), [`handle_wait_park`](../../src/app/waits.rs) |
| CLI events read/wait and MCP `flock_events_read` / `flock_wait_park` exist. The MCP tool registry has no general work/loop tools. | [`events.rs`](../../src/cli/events.rs), [`tools` registry and builders](../../src/mcp/tools.rs) |
| PR/CI facts already have a shared producer and restart seeding. Its CI dedupe key is repo/PR/head SHA; the first conclusion wins, including over later reruns at the same SHA. A new rule cannot assume it observes every CI attempt. | [`PrEventLedger::key`, `seed` and `derive`](../../src/pr_poll.rs) |
| A structured runtime owns process supervision and cleanup. Birth identities fence signals, sampled descendants can be retained, and ancestry lost between samples cannot be discovered. That supervisor does not constitute a work-loop registry. | [`run` and runtime worker](../../src/structured/runtime/mod.rs), [`ProcessIdentity` and `supervise`](../../src/structured/supervision/mod.rs) |
| Structured admission is journal-backed and currently accepts normal-priority, at-idle items. Wake is a queue kind. Delivery checks lifecycle, attention, idle state and outstanding permissions; stdin acceptance is not harness acknowledgement. | [`Item`, `Kind` and `State`](../../src/structured/queue/model.rs), [`Admission::enqueue`](../../src/structured/queue/admission.rs), [`Scheduler::deliver_on`](../../src/structured/queue/scheduler.rs) |
| The queue journal exposes write/commit uncertainty, and queue recovery loads on an owned I/O worker before a generation-fenced install. Neither implies that a restored agent process is automatically restarted. | [`Journal::durable`, `append` and commitment state](../../src/structured/queue/mod.rs), [`Recovered::install`](../../src/structured/workers/queue/recovery.rs) |
| Queue worker ticks publish on an initial pass, state change or admitted request. Queue permission preflight and second admission wait through book contention; operator-facing calls remain nonblocking. A future durable wake must enter that real request path, not merely set a notification. | [`Service::tick`](../../src/structured/workers/queue.rs), [`queue_waiting_on_human`, `permission_book` and `queue_submit`](../../src/structured/transport/mod.rs) |
| Permission decisions require the authenticated operator-token path and the live request ID/sequence pair. Grants belong to one runtime; resuming creates a new book. A heartbeat is not permission authority. | [`Principal`, `AgentDecide` and book contract](../../src/structured/permissions.rs) |
| Queue publication already distinguishes gap, record, synced, pending, refusal and lagged outcomes. The inspected S7 subscription dispatcher serves fleet and agent streams, not a work stream. | [`Page`](../../src/structured/workers/queue/publication.rs), [`Subscriptions::subscribe`](../../src/web/wire/subscriptions.rs) |

The accepted directions in ADR-0030 and ADR-0035 are larger than that implemented
baseline. This proposal must not report their stop ladder, automatic owner wakes
or external goal-s1 migration as already shipped.

## Owner decisions requested

Every recommendation is provisional. Approval must name D1–D7; implementing one
must not imply approval of the others.

| Decision | Options and costs |
| --- | --- |
| **D1: work identity and storage** | **(Recommended)** Stable `WorkId` records folded from the existing log, with bounded record-carrying checkpoints and an explicit compatibility link to the one-goal-per-agent API. Cost: an acknowledged writer, rotation fencing, migration and retention work before any liveness claim. Alternative: a transactional database as the authoritative store. Cost: a second storage/migration/backup contract and an amendment to ADR-0031; potentially simpler transactions at larger scale. Do not build both. |
| **D2: who can declare completion** | **(Recommended)** An assigned agent records progress and proposes completion; its supervisor/operator approves the frozen check revision and evidence. Root completion remains operator-approved. Cost: an explicit review acknowledgement, and blocked progress when no authorised reviewer is available. Alternative: agents self-close after check success. Cost: an agent can declare its own success without independent review; changes ADR-0030's done gate. |
| **D3: delivery semantics** | **(Recommended)** Durable outbox, at-least-once processing, stable wake IDs, deduped queue admission and an explicit handled acknowledgement. Cost: retained dedupe state and extra durable writes; actions outside flk still need idempotency keys or review. Alternative: mark handled when queued/started. Cost: a crash during the turn silently loses the supervisor action. “Exactly once” for arbitrary agent tool effects is not an offered guarantee. |
| **D4: heartbeat policy** | **(Recommended)** Per-loop progress deadlines, ordinary at-idle queue delivery and bounded escalation to supervisor/operator; never clear an operator composer or bypass permission. Cost: a busy, frozen or permission-blocked agent may need human intervention. Alternative: explicitly authorised destructive hard-send as proposed in #1044. Cost: draft loss, harness-specific submission verification and a separate safety design; it is not enabled by accepting the recommended policy. |
| **D5: restart and host handoff** | **(Recommended)** One active authority, fenced by an ownership epoch; restart/refold and cooperative break-before-make host transfer. Cost: an unreachable old authority blocks transfer until it can be fenced; no automatic partition failover. Alternative: replicated leases/consensus and automatic failover. Cost: a distributed authority service, partition policy and a separate design before rollout. A local file lock alone cannot fence two hosts. |
| **D6: trigger ingestion** | **(Recommended)** Typed observations from shared server producers, typed receipt submission first, and a bounded operator-configured receipt-file compatibility adapter. Cost: source-specific health/reconciliation and filesystem admission rules. Alternative: run arbitrary watch scripts per loop. Cost: recreate the original unsupervised process, credential and restart failure modes. Neither option grants instructions embedded in an event authority. |
| **D7: work projection and migration** | **(Recommended)** Deliver #1600's read-only projection first, then enable #1603's record phase explicitly; one work stream and goal-tree tile, with old goal commands as a compatibility view. Cost: a versioned projection-to-record transition and adapters for existing tooling. Alternative: new independent goal/task GUI and stores. Cost: divergent status and duplicate ownership; conflicts with ADR-0038 §8. |

Proposed initial limits, included in D1/D6 rather than hidden implementation
choices: 256 retained work nodes, 64 loop definitions and 256 unhandled wake
intents per authority; 16 KiB encoded work record, 4 KiB loop or wake record,
2 KiB wake summary and 64 linked evidence descriptors per work node. The
aggregate encoded checkpoint, including opaque future fields and dedupe state,
must fit an 8 MiB reserved budget. Refuse admission before exceeding any limit,
retain already accepted state and expose capacity exhaustion. No silent
truncation, dropping oldest pending wakes, or automatic archival.
These are proposed bounds, not measured scalability claims; slice 1 must measure
append/fsync and restore cost under them before the feature is enabled.

## Proposed record and authority model (D1, D2)

`WorkId` and `LoopId` are server-minted stable IDs; names, panes and worktree
paths are labels, never identity. A node has a goal/task kind, parent links,
acceptance revision, owning agent ID (possibly unassigned), supervisor identity,
status, evidence references, required loop IDs and an optimistic-concurrency
revision. An assignment names the verified node ID, agent ID and assignment
generation; an unverified peer name cannot stand in for any of these. Parent
links form a DAG: refuse cycles and dangling links. A completed child's state
does not automatically complete its parent. Coverage is reported separately as
unarmed, armed or degraded; activating protected work requires its required
loops to be installed.

Statuses are `planned`, `active`, `waiting`, `blocked`, `review_pending`, `done`
and `cancelled`. Pause is a separate supervision control. Every change carries
the server-attested actor and expected revision. Assigned agents can update
progress, attach evidence and enter `review_pending` within their assignment; only the authorised supervisor/operator changes acceptance, reassigns ownership,
pauses/stops a loop or approves completion. Preserve the existing goal API's
self-pause/self-stop refusal. An unresolved or peer-claimed identity gains no
local authority; do not mistake the existing cooperative ancestry check for a
security boundary. Use ADR-0036's verified principal and scope path for new
agent-facing structured commands.

Acceptance is immutable within a revision. Checks are structured argv/cwd
references under the authorised workspace, not shell strings taken from GitHub
or receipt text. Running checks requires an explicit capability, a process/birth
identity, a bounded worker and captured result/evidence. This ADR does not
silently execute legacy `GoalRecord.checks` strings. Changing acceptance
invalidates prior approval. Evidence is typed: repo/PR/head, CI run/attempt and
conclusion, receipt/task/revision/content hash, or check invocation/exit/result
hash. A link or a `DONE:` line is a claim until verified. Old-head green CI
cannot satisfy a new-head acceptance check. Approval names the exact acceptance
revision and evidence set; unknown, missing or unreadable evidence blocks it.

Proposed events retain ADR-0038's `work.created`, `work.updated`,
`work.edge_added`, `work.edge_removed` and `work.closed`, with typed
`work.loop_changed`, `work.wake_changed` and record-carrying `work.checkpoint`
payloads. Each mutation carries its actor, operation ID, revision and work ID; the occurrence variant carries both its cursor advance and wake intent.
Checkpoint copies are not new semantic changes. These are proposed schema
additions, not existing persisted kinds.

The compatibility mapping retains the existing active agent-goal binding without
inventing multiple active legacy goals. Imported checklist IDs are scoped to
their old goal; `done: true` is imported as a legacy claim, not retroactively
verified evidence. Preserve missing legacy actor information as unknown rather
than inventing an attested author. A removed agent leaves its work unassigned/blocked until an
authorised reassignment; a new agent with the same name inherits no authority.

## Durable subscription and delivery protocol (D3, D6)

A loop definition contains its work/agent IDs, predicate, source cursors,
occurrence policy (once or recurring), progress/deadline policy, escalation
recipient and ownership epoch. Held CLI/MCP reads remain connection-scoped; ending a read does not cancel a durable loop. Extend the wait service instead
of building a second independent predicate/wake engine. Existing `wait.park`
compatibility and persistence are tracked by #1240; missing predicate evaluation
must be implemented, not inferred from registration.

The durable state machine is:

```text
registered -> occurrence recorded + wake pending -> queue committed
           -> harness started -> handled acknowledgement
                   |                    |
                   +---- uncertainty / deadline -> visible escalation
```

1. Capture a source cut and its current condition when registering. Commit the
   definition, then replay from that cut and re-evaluate the condition. A source
   event between capture and registration must not fall into a gap. A condition
   already true at registration yields one occurrence, not an eternal wait.
2. Record an occurrence, its pending wake and the next source cursor in one
   durable transition. Only then advance the in-memory cursor. Notify is a
   readiness hint; replaying the durable index must reconstruct owed work even
   if every in-process notification is lost.
3. An owned I/O worker appends/syncs before publishing a committed transition or
   answering success. Introduce a fallible acknowledged write path; the current
   best-effort `EventHub::push` cannot be the commit acknowledgement. On write
   uncertainty, expose a typed unresolved outcome and reconcile by operation ID;    never answer “registered”, advance a cursor or announce handled prematurely.
   Keep filesystem work and fsync off the server deadline dispatcher.
4. Admit a `Kind::Wake` at normal priority/at idle through the existing queue
   worker. Its internal producer is authenticated server context. The wake ID
   determines the queue item ID and immutable payload hash. Repeating the same
   ID/body observes the prior result; same ID/different body refuses. Queue
   commit and loop-outbox commit are not one transaction: after a crash between
   them, look up/admit that same item ID, never mint a replacement. Preserve
   dedupe evidence until the occurrence is handled, including through queue
   compaction and runtime-generation changes. Commit uncertainty never causes
   a blind resend under a new identity.
5. Harness-started and queue-done are transport/turn facts, not proof that the
   requested supervisor action happened. `work.wake.ack` must name the wake,
   assignment and expected revision. Persist the acknowledgement before replying.
   Repeated admission requests retain the wake/item ID; only one queue item may
   be active for it. A committed/started item is not blindly written to stdin
   again: an unhandled occurrence remains in the next turn's context and
   escalates at its deadline. Queue-done does not get rewritten to queued.
   At-least-once admission is not automatic replay of an accepted turn.
   Consumer actions must query their effect or use their own idempotency keys.
6. Check each wake against the current assignment/authority before delivery.
   Reassignment, cancellation and handoff fence stale completions and late
   acknowledgements. Human attention, denied permission, retired sessions and
   unknown turn completion remain visible pending/refused states; no loop
   auto-answers a permission or treats stdin bytes as accepted work.

All open work records, definitions, source cursors, pending wakes, dedupe state,
acknowledgements and escalation deadlines participate in bounded record-carrying
checkpoints. Reserve checkpoint space before rotation and fence further rotations
until the new active file durably covers the complete state; do not depend on a
later event-loop pass getting CPU before old coverage rotates out. Restore folds
before serving work status or dispatching wakes. Readers skip and preserve unknown
kinds/fields per ADR-0028 §3a, and refuse unsafe rewrites with unknown state.
Every implementation slice adding persisted kinds tests that reader contract.

Terminal records are retained/re-issued until explicitly archived by an
authorised actor. Archival warns that older history can rotate out and offers a
bounded export; it is not an implicit response to a full store. Event history
remains retention-bounded, even when the current work record survives. Evidence
references do not promise indefinite availability of external PRs or receipts.

### Observation sources and coverage

- **PR/CI:** extend the shared poller with watched subjects independent of a
  visible UI; no per-loop `gh --watch`. Observe run/attempt identity as well as
  head, so a rerun's failure is not hidden by the existing first-conclusion
  ledger. Reconcile current state at registration/restart, including merged
  before registration and a new head with no checks. Batch-landed close is a
  typed policy observation, not any arbitrary closed PR being treated as merged.
- **Receipts:** prefer `work.evidence.attach` with validated task/work/PR/head
  fields. A file adapter is explicitly configured by the operator for a private
  root: no default directory crawl, symlink traversal or arbitrary URL fetch.
  Bound reads, validate complete records after atomic rename, and dedupe by
  receipt identity/revision/hash. Filesystem notifications are hints; startup
  and overflow reconcile the registered root. Partial/invalid receipts do not
  advance that receipt's acceptance cursor and surface a typed per-entry error
  without logging contents; they do not block other valid receipt entries.
- **Agent idle/blocked:** consume typed phase/attention and turn facts, not pane
  text or a final-line sentinel. Idle is not completion; #663's distinction
  between background work and a finished result remains necessary.
- **Disk low:** a server-owned capacity observation emits on threshold crossing
  and recovery, with hysteresis set in the policy. Samples do not continuously
  re-trigger the same condition. The monitor's own failure is a source-health
  state, not evidence that disk is healthy.

Every source has a last-success/coverage cursor, health and a bounded observation
latency. Network outages, rate limits, source panics and overflow are visible.
Reconciliation can recover current conditions, but cannot invent every transient
external state missed during downtime. Emit a coverage-gap condition requiring
review rather than claim the stream was complete. Polling belongs only in bounded
external adapters where push is unavailable; the wake scheduler itself is driven
by committed changes and deadlines, not a sleep/retry loop.

## Progress, compaction, restart and handoff (D4, D5)

An agent finishing a turn, a tool call, pane output or the supervisor's own wake
is not automatically progress. Persist meaningful progress against a work
revision, or a bounded declared long-running-job lease. The proposed default
acknowledgement/progress deadline is 15 minutes, configurable by the authorised
supervisor; a declared lease includes a finite expiry. Pauses suppress automatic delivery and escalation reminders, not pending-state
visibility. Resume re-evaluates overdue occurrences once; it does not silently
grant a new lease. Provider limits and human permission waits produce
blocked/escalation status rather than a force-send. One overdue occurrence opens
one escalation; reminders, if enabled, have an explicit capped schedule and
remain attached to that occurrence, never a hidden timed delivery retry.

Evaluate deadlines on a dispatcher reachable while any I/O/check/observation
worker is in flight. Use a supplied monotonic clock during uptime; persist wall
clock deadlines and reconstruct remaining time on restart. An already elapsed
deadline escalates once immediately. Clock rollback/skew must surface a degraded
clock state and a conservative bounded lease, not grant an indefinite wait or
mass-produce old heartbeats. Deadline delivery can overshoot with OS scheduling
or unavailable storage; report dispatcher/source health and pending age, not a
hard real-time guarantee. If the whole server/host is down, it cannot send a
heartbeat: an independently hosted monitor is needed for that failure domain.

Compaction changes no loop ownership. The next turn retrieves a bounded work and
pending-wake summary through the supported context/adapter path; it does not
start a shell waiter. After a server restart, records and pending wakes refold,
then sources reconcile and queue delivery resumes only when the assigned runtime
is live. If it never becomes live, the deadline escalates; the server must not
silently start a replacement agent.

For same-host live handoff, stop the exporting writer before import/refold and
carry the complete pending state before answering status. Cooperative cross-host
handoff additionally needs an authenticated, revisioned export and durable release
of the old authority before the target can accept a new ownership epoch. The
old authority must checkpoint its release tombstone and refuse further writes
after release; old workers cannot commit
into the new epoch. Interrupted transfer remains visibly `handoff_pending` and
resumes by transfer ID. A partition without proven fencing refuses takeover; reusing a local lock file or copying a receipt is not proof. Remote command
transport and its coordinated mesh-version rollout remain #1380's dependency; this ADR makes no mesh-v5 command or distributed-failover claim.

## Public surfaces and migration (D7)

Proposed socket verbs are `work.create/get/list/update/link/unlink`,
`work.evidence.attach`, `work.complete.propose/approve`, `work.archive`,
`work.loop.register/get/cancel`, `work.wake.ack` and `work.wake.progress`. Mutation inputs carry
an idempotency ID and expected revision, never an actor supplied by the body.
Add `flk work` and generated `flock_work_*` MCP adapters over the same service; keep `flk goal` as the compatibility view rather than maintain two task stores.
Bind grants to read, assigned-work updates or supervisor/operator actions; existing
send authority alone cannot mutate acceptance or cancel supervision. Register
any new refusals and retry classes through ADR-0029's error-code registry.

The `work` stream first projects existing goals/delegates (#1600), explicitly
showing unknown/unimplemented facts. The later record phase (#1603) uses actor-attributed events and the
cursor/gap/snapshot contract, with
an explicit schema capability and legacy-ID-to-WorkId mapping at the transition.
Phase 1 derives view IDs from existing authority/agent/delegation keys without
new records. Those keys can be reused on legacy re-arm: they are not durable
WorkIds or mutation targets. Entering phase 2 requires a complete snapshot reset
and committed migration mapping, so clients cannot confuse two goal lifetimes.
Generate Rust/TS shapes from one source. Add a goal-tree tile and sidebar tree; show assignment, status,
acceptance revision, evidence head, pending wake age, source health and overdue
acknowledgement without turning every progress sample into agent context.
Render untrusted labels safely; frame external evidence as data with unforgeable
per-item delimiters when it reaches a model. Neither the GUI nor MCP owns timers.

New verbs/streams must use the existing version/skew gates; an older server
returns a typed unsupported/version refusal, never apparent success with a loop
that was not installed. Any required local protocol bump follows the released-tag
rule. No mesh version change is part of this docs PR.

`goal-s1` integrates as a thin stop/context adapter to the server's goal/work
service, preserving ADR-0030's ladder and human-reserved decisions (#573, #965).
`watcher-s1` remains useful for independently running commands/classification,
but its owned observations enter typed wait/evidence sources; it does not become
the lifetime owner of a flk loop (#663, #857). Cut over one work assignment at a
time: verify server registration and pending delivery, disable its duplicate
script watcher, then remove that watcher. Do not run two uncoordinated injectors.
Provider-specific judges and free-form process/GPU progress remain their existing
scopes (#573, #857), not guessed from new pane scraping here.

## Pre-mortem and failure modes

Assume the owner again discovers a red batch sixteen hours late. Work backwards:

| Failure | Prevention, observable outcome and recovery |
| --- | --- |
| The architect compacted before registering a watcher, or the CLI died after registration | Registration is durable and idempotent; inspect by operation ID after an uncertain response. No acknowledged registration means no claimed protection. An armed work item without its required loop is visibly unsupervised. |
| Event arrived during subscription setup, or restart seeded it as historical | Capture/replay cut plus current-condition reconciliation. Initial terminal facts still create an occurrence; source seeding cannot suppress an armed rule. |
| Disk full, append failure, worker panic or an unacknowledged fsync | Never return committed success or advance cursor from memory alone. Retain pending/unknown state, typed storage degradation and operator-visible recovery. A memory-only notification cannot clear it. |
| Crash after queue commit but before outbox acknowledgement | Replay the same wake/item identity and compare payload, dedupe or return uncertainty; retain enough queue evidence to reconcile after compaction. |
| Agent started a turn but never acted, or acknowledged someone else's wake | Require handled acknowledgement from the assigned identity/revision; progress deadline escalates. Started/done observations do not close the occurrence. |
| PR rerun failed at the same SHA, checks never started or the shared poller stopped | Track run/attempt and source health; reconcile current PR/head/check state, expose missing checks and observation gaps. No viewer must not disable an armed source. |
| Permission pending, agent removed, provider limited, queue held or turn end unknown | Keep the pending reason and escalate; no permission bypass, auto-reassignment by name, background process kill or fabricated success. |
| Wake/receipt storm, repeated low-disk samples or malformed untrusted input | Typed keyed occurrences, transition-based conditions, bounded records/queues and admission refusal. Preserve already committed wakes and report capacity/source errors. |
| Two hosts both believe they own the loop | Epoch fence with cooperative release; no promotion without fencing. An unavailable authority is visible, not silently replaced. |
| Clock moved, server stalled in I/O, or the entire host went down | Reachable deadline dispatcher and persisted deadlines; explicit clock/dispatcher degradation. Whole-host absence needs an independent monitor, not a claim of an infallible local loop. |
| Log rotated while the loop was quiet, or a downgrade skipped new data | Record-carrying checkpoint and rotation barrier; unknown data retained and unsafe compaction refused. Old readers cannot claim to operate newer loops. |
| Observer text became instructions, or a peer forged an owner/receipt | Trusted principal at admission, typed evidence verification, safe rendering and framed model context. Refuse unverified authority; never execute commands embedded in an observation. |

“Never drop” means no silently forgotten acknowledged work: each accepted
occurrence remains pending, handled or explicitly failed/escalated. It does not
mean exactly-once external effects, unlimited retention, guaranteed agent action,
a hard scheduling deadline, or recovery from total storage loss without backups.

## Ordered implementation slices and fail-first acceptance

All estimates below count **all production Rust and TS together**, excluding
tests/docs. Each is a separate PR of at most 400 lines; estimates are planning
bounds, not permission to exceed the cap. If a slice grows, split it before
opening its implementation PR. The listed order is the dependency order; no
record-writing or autonomous wake feature is enabled before its prerequisites.

Every test starts at the real socket/CLI/MCP/wire ingress and ends at its public
status/stream or at the input observed by
[`tests/fixtures/structured-public/claude.py`](../../tests/fixtures/structured-public/claude.py).
Use only that fake model, isolated server instances, private temporary HOME/data,
short socket paths and no inherited live-server selectors or credentials. Source
feeds and clocks are controlled test adapters, never real GitHub, disks filled
on the host, judges or user panes. Fault seams must be added without changing
behaviour first if needed; show an actual failing assertion against the old
behaviour, never a compile failure for a missing API. Raw JSON calls can exercise
an as-yet unsupported verb before its implementation.

| Order / estimate | Slice and tracked scope | Fail-first public-path acceptance |
| --- | --- | --- |
| 0 / 120 | Harness/source-clock and commit-boundary fault controls (#2718) | Start the fake model through the public launch path; prove a silent/dead/unexpected producer fails within a bounded receive ceiling. This is shared test infrastructure, not a synthetic second scheduler. |
| 1 / 320 | Acknowledged append and commit-unknown outcome (#2718, ADR-0031 extension) | Inject append failure during public goal/work mutation: response must refuse/be uncertain, status must not claim a durable update, and restart must agree. Measure append and restore at proposed capacity. |
| 2 / 300 | Work identity, create/get/update and actor/revision fold (#1603, D1 gate) | Create via socket, restart, read same ID/revision; duplicate operation changes nothing, stale revision and forged actor refuse. Include unknown-kind preservation. |
| 3 / 280 | Parent links and attested assignment (#965, #2718) | Publicly link/reassign tasks; cycles refuse; a same-named replacement agent cannot edit the old assignment. |
| 4 / 320 | Evidence and completion proposal/approval (#2718) | Attach receipt/PR/check evidence, change acceptance/head, then attempt approval with stale evidence: refuse. Fake delegate self-approval cannot mark root done. |
| 5 / 320 | Capability-bound check worker (#2718) | Invoke a configured checker through the work API using a check mode of the same fake fixture: nonzero exit, silence and worker death never produce green evidence; an unauthorised argv/cwd refuses before spawn and only owned children are cleaned up. |
| 6 / 340 | Checkpoint capacity, rotation fencing and explicit archive (#2718) | Register quiet work, force rotations beyond retention, restart and read identical state; capacity refuses before loss. Archive requires the authorised actor and reports retention consequences. |
| 7 / 300 | Persisted loop definitions and park compatibility (#1240, #2718) | Register, disconnect CLI, restart/live-handoff, inspect same loop/wait ID and remaining deadline; cancellation by a different agent refuses. |
| 8 / 320 | Source cut, occurrence and cursor/outbox transition (#2718) | Gate an event between registration cut and commit, then replay it twice: one pending occurrence, including when the condition was already true. Crash before/after commit retains the correct cursor. |
| 9 / 300 | PR/CI shared-source adapter (#663) | Feed a CI failure after a successful rerun at the same head and an already merged PR at registration; both fire correctly without a viewer or per-loop poll process. A feed outage exposes coverage loss. |
| 10 / 240 | Typed receipt API (#2718) | Submit same receipt twice through API, then wrong task/head/content hash: one verified occurrence and a typed refusal for mismatches, never raw prompt execution. |
| 11 / 320 | Bounded receipt-file compatibility adapter (#2718) | Atomic rename in the allowed root fires once after restart; partial files, symlinks, oversized data and an overflow notification surface refusal/reconciliation, not a lost cursor. |
| 12 / 220 | Agent phase/attention source (#663) | Fake model enters idle with background work or blocks on permission: source reports that state without claiming task completion. Duplicate observations do not duplicate wakes. |
| 13 / 180 | Disk/source health adapter (#1044, #2718) | Inject low/recovered/low capacity readings through the source seam; public status and occurrences reflect three transitions, not sample count. A failed sampler reports unhealthy. |
| 14 / 340 | Stable wake/item admission and retained queue dedupe (#2718) | Submit the same internal wake through the public supervision service across queue compaction/recovery: one active item; altered payload refuses. Fake input is not duplicated at the queue-commit crash boundary. |
| 15 / 300 | Outbox-to-queue worker and real idle delivery (#2718) | Hold then release the permission book during delivery with no later model events: the fake receives its wake. Crash after admission before outbox update reconciles the same item ID. |
| 16 / 300 | Handled acknowledgement and progress lease (#1044) | Fake model starts/ends a turn without ack: public loop remains outstanding. Correct ack survives restart; wrong agent/revision, duplicate ack and expired progress lease have defined outcomes. |
| 17 / 320 | Reachable deadlines and escalation (#1044) | Drive supplied clock past deadline while an observation worker is blocked: one visible escalation. Pause/permission/provider-limit states do not cause destructive sends; no progress cannot extend the lease indefinitely. |
| 18 / 280 | Restart and same-host handoff reconstruction (#1240, #2718) | Isolated server handoff between occurrence, queue commit and ack preserves pending work and IDs; no auto-start of a retired agent. Deadline elapsed during downtime escalates once. |
| 19 / 340 | Cooperative cross-host authority transfer (#2718; remote transport gated on #1380) | Two isolated authorities transfer the fake assignment; old epoch operations refuse, interrupted transfer resumes by ID, and a missing release refuses takeover without claiming success. |
| 20 / 280 | CLI work/loop adapter (#2718) | Drive create/register/status/ack via `flk work`; structured results distinguish committed, pending and unknown. A newer verb against an older isolated server refuses with the existing skew contract. |
| 21 / 280 | MCP work/loop adapter (#1603) | Through fake fixture MCP calls, register and acknowledge the same work as CLI; inspect identical revision/cursor and reject agent attempts to mutate supervisor-only fields. |
| 22 / 280 | Read-only work projection (#1600; before record writers, subject to its ADR-0038/S7 gates) | Subscribe through the actual wire to existing goals/edges; public replay includes empty bootstrap and gap/synced. Projection fields do not invent evidence or completion. |
| 23 / 320 | Work-record stream transition (#1603, after D7 and record slices) | Reconnect real client at a cursor around mutation/checkpoint; no missing or duplicate folded node. Unknown additive fields and explicit version skew produce defined outcomes. |
| 24 / 340 | Goal-tree tile and sidebar (#1600) | Browser over isolated fake-model server: create/link/change a task through API, see the tree and pending/source/ack badges change; a gap replaces obsolete nodes and malicious labels render safely. |
| 25 / 300 | Goal compatibility and stop/context migration (#573, #965) | Register work for the fake model; drive the public SessionStart hook with compact/resume source and retrieve goal/checks/status/delegates plus pending wakes in bounded context; no shell waiter is needed or silently started twice. |

Slice 22's placement is a scheduling convenience, not a dependency on phase 2:
#1600 can proceed as a read-only projection independently of the gated writers.
All other “after” constraints stand. For each implementation PR record the red
command/assertion and exit code, restore the fix, and run focused nextest/client
checks. Fake-clock advancement drives conditions; no test sleeps, timed retries
or wider ceilings may manufacture ordering (ADR-0037).

## Boundaries and tracked follow-up

- General process/GPU progress sources and external watcher-s1 command semantics
  remain [#857](https://github.com/gerchowl/flock/issues/857); loop registration
  must refuse an unsupported predicate rather than claim it is watched.
- The legacy goal judge/stop ladder and external goal-s1 shim migration remain
  [#573](https://github.com/gerchowl/flock/issues/573) and
  [#965](https://github.com/gerchowl/flock/issues/965). This ADR does not claim
  that all adapters or judges already exist in flk.
- Remote command transport and mesh rollout remain
  [#1380](https://github.com/gerchowl/flock/issues/1380). Cross-host transfer is
  gated on its authority/transport dependencies, not encoded as v5 mail text.
- Automatic partition failover, independently hosted host-down monitoring and
  permanent audit archival are unresolved D1/D5 operational extensions tracked
  by [#2718](https://github.com/gerchowl/flock/issues/2718); accepting the
  recommended bounded single-authority design does not implement or promise them.
- Work projections/GUI and durable work writers stay tracked separately by
  [#1600](https://github.com/gerchowl/flock/issues/1600) and
  [#1603](https://github.com/gerchowl/flock/issues/1603). Neither feature is
  advertised as shipped by this Proposed ADR.
