# ADR-0045: Durable work records and server-owned supervision loops

- Status: Accepted
- Accepted: 2026-10-29; owner decision: “accept all rec with star”.
- Date: 2026-10-29
- Issues: [#2718](https://github.com/gerchowl/flock/issues/2718), informed by
  [#819](https://github.com/gerchowl/flock/issues/819),
  [#663](https://github.com/gerchowl/flock/issues/663),
  [#1044](https://github.com/gerchowl/flock/issues/1044),
  [#965](https://github.com/gerchowl/flock/issues/965),
  [#1600](https://github.com/gerchowl/flock/issues/1600) and
  [#1603](https://github.com/gerchowl/flock/issues/1603).
- Decision owner: repository owner; D1–D11 decided on 2026-10-29.
- Builds on: ADR-0027/0028 (delegation, observation and waits), ADR-0029
  (agent-facing contracts), ADR-0030/0031 (goals and storage), ADR-0034/0035
  (authority and resident owners), ADR-0036 (structured sessions), ADR-0037
  (bounded tests), ADR-0038 §8 (work projection and records).
- Coordination: [ADR-0042](https://github.com/gerchowl/flock/pull/2724)
  (`0f234e23`), [ADR-0044](https://github.com/gerchowl/flock/pull/2728)
  (`1ba1c7c9`) and the [S6e answer-wake
  amendment](https://github.com/gerchowl/flock/pull/2731) (`0d301649`),
  inspected source revisions. The owner also accepted their recommended choices
  on 2026-10-29; acceptance does not claim implementation.
- Implemented: none of this proposal. The baseline below describes inspected
  code at `b2519c2c49b08498d75ac427a182ac2298ab49c2`.

## Context and placement

The owner reports a compaction with no surviving waiter, a red landing batch
unnoticed for roughly sixteen hours, and repeated manual waiter restarts
([#2718](https://github.com/gerchowl/flock/issues/2718)). This ADR does not
claim to have reproduced that incident or identified which private script died.
It specifies the missing server guarantees that would make such silence visible.

This is a separate ADR because work identity, persistence, subscription
ownership and acknowledgement span ADR-0030/0031 and ADR-0038, beyond ADR-0036's
session and transport boundary. It extends those records in the named areas; it
does not silently supersede their authority, stop ladder or wire contracts. In
particular, ADR-0038 §8 remains Proposed, and #1603's record phase is not
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

The accepted directions in ADR-0030 and ADR-0035 are larger than that
implemented baseline. This proposal must not report their stop ladder, automatic
owner wakes or external goal-s1 migration as already shipped.

## Owner decisions (2026-10-29)

The owner accepted D1–D11: “accept all rec with star”. Each chosen option below
records that decision, including the D4 limits and revised D8 endpoint.
Alternatives are retained as rejected options with their costs. Acceptance
approves the design; it does not declare the implementation slices shipped.

| Decision | Options and costs |
| --- | --- |
| **D1: work identity and storage** | **Decided 2026-10-29: chosen recommendation.** Stable `WorkId` records folded from the existing log, with bounded record-carrying checkpoints and an explicit compatibility link to the one-goal-per-agent API. Cost: an acknowledged writer, rotation fencing, migration and retention work before any liveness claim. Rejected alternative: a transactional database as the authoritative store. Cost: a second storage/migration/backup contract and an amendment to ADR-0031; potentially simpler transactions at larger scale. Do not build both. |
| **D2: who can declare completion** | **Decided 2026-10-29: chosen recommendation.** An assigned agent records progress and proposes completion; its supervisor/operator approves the frozen check revision and evidence. Root completion remains operator-approved. Cost: an explicit review acknowledgement, and blocked progress when no authorised reviewer is available. Rejected alternative: agents self-close after check success. Cost: an agent can declare its own success without independent review; changes ADR-0030's done gate. |
| **D3: delivery semantics** | **Decided 2026-10-29: chosen recommendation.** Durable outbox, at-least-once processing, stable wake IDs, deduped queue admission and an explicit handled acknowledgement. Cost: retained dedupe state and extra durable writes; actions outside flk still need idempotency keys or review. Rejected alternative: mark handled when queued/started. Cost: a crash during the turn silently loses the supervisor action. “Exactly once” for arbitrary agent tool effects is not an offered guarantee. |
| **D4: heartbeat policy** | **Decided 2026-10-29: chosen recommendation.** Per-loop progress and handling deadlines, healthy-source waits capped at four hours by default (at most 24 hours), a two-hour no-subject-change limit, ordinary at-idle delivery, the D8 escalation chain with reminders ON by default, and finite pauses (default 30 minutes). Never clear an operator composer or bypass permission. Cost: notification traffic and resumed-agent compute; busy, frozen or permission-blocked agents may need human intervention. Rejected alternative: explicitly authorised destructive hard-send as proposed in #1044. Cost: draft loss, harness-specific submission verification and a separate safety design; it is not enabled by accepting the recommended policy. |
| **D5: restart and host handoff** | **Decided 2026-10-29: chosen recommendation.** One active authority, fenced by an ownership epoch; restart/refold and cooperative break-before-make host transfer. Cost: an unreachable old authority blocks transfer until it can be fenced; no automatic partition failover. Rejected alternative: replicated leases/consensus and automatic failover. Cost: a distributed authority service, partition policy and a separate design before rollout. A local file lock alone cannot fence two hosts. |
| **D6: trigger ingestion** | **Decided 2026-10-29: chosen recommendation.** Typed observations from shared server producers, typed receipt submission first, and a bounded operator-configured receipt-file compatibility adapter. Cost: source-specific health/reconciliation and filesystem admission rules. Rejected alternative: run arbitrary watch scripts per loop. Cost: recreate the original unsupervised process, credential and restart failure modes. Neither option grants instructions embedded in an event authority. |
| **D7: work projection and migration** | **Decided 2026-10-29: chosen recommendation.** Deliver #1600's read-only projection first, then enable #1603's record phase explicitly; one work stream and goal-tree tile, with old goal commands as a compatibility view. Cost: a versioned projection-to-record transition and adapters for existing tooling. Rejected alternative: new independent goal/task GUI and stores. Cost: divergent status and duplicate ownership; conflicts with ADR-0038 §8. |
| **D8: an escalation endpoint people can see** | **Decided 2026-10-29: chosen GUI endpoint with optional interim channel.** Supervisor → its supervisor → operator, with durable attention, native desktop notifications in the flk Tauri GUI and remote push to its iOS/Android app. #2733 owns mobile delivery/enrolment design and depends on ADR-0043 remote access. Protected loops require an enrolled, healthy GUI endpoint or, until mobile push ships, an optional #1005 tg-mcp/Telegram binding. Telegram is then removed under #2734. Reminders remain ON every 15 minutes until meaningful progress or a bounded acknowledgement lease. Cost: native notification permissions, device enrolment, authenticated remote access, push infrastructure and delivery-health checks; transport delivery does not prove the human read it. Rejected alternatives: GUI inbox alone can be silent while closed; Telegram as a permanent mandatory endpoint adds an unwanted external dependency; OS notifications without a healthy enrolled endpoint cannot claim remote reachability. |
| **D9: receipt-bound lifecycle wake** | **Decided 2026-10-29: chosen recommendation.** A committed LoopWakeReceipt may resume its exact confirmed Hibernated recipient for that recipient's original ItemId, extending ADR-0044 Q5 in the same narrow way as #2731's AnswerReceipt. Cost: a new constrained internal capability, resume compute and joint lifecycle/queue fencing tests. Rejected alternative: leave the item queued for a separate authorised send/focus; cost: every unattended hibernated supervisor can remain asleep forever. Neither option treats read, subscription or an uncommitted event as start permission. |
| **D10: GitHub identity and transport** | **Decided 2026-10-29: chosen recommendation.** One shared, bounded polling owner per configured credential reference, using an operator-enrolled read-only GitHub App installation with explicit repositories. No agent credentials or implicit ambient `gh` login. Cost: installation/credential provisioning, quota accounting and up to the configured polling latency (default two minutes while healthy). Rejected alternative: verified GitHub webhooks, costing a reachable endpoint, signature-secret references, replay protection and reconciliation; or an explicitly enrolled read-only user token, costing broader user identity/rate-limit coupling. Never silently substitute the operator's general token. |
| **D11: check execution boundary** | **Decided 2026-10-29: chosen recommendation.** An operator-provisioned sandbox runner under a dedicated unprivileged OS account, with explicit input snapshots, scrubbed environment and no network/credentials. Refuse execution when unavailable. Cost: platform-specific isolation/provisioning and preloaded dependency caches; some builds cannot run offline. Rejected alternative: run as the server user with env scrubbing only. Cost: that UID can still read operator files/credentials; this is not a sandbox and is not enabled by accepting the recommendation. |

Accepted initial limits, included in D1/D6 rather than hidden implementation
choices: 256 retained work nodes, 64 loop definitions and 256 unhandled wake
intents per authority; 16 KiB encoded work record, 4 KiB loop or wake record, 2
KiB wake summary and 64 linked evidence descriptors per work node. The aggregate
encoded checkpoint, including opaque future fields and dedupe state, must fit an
8 MiB reserved budget. Refuse admission before exceeding any limit, retain
already accepted state and expose capacity exhaustion. No silent truncation,
dropping oldest pending wakes, or automatic archival. These are accepted design
bounds, not measured scalability claims; slice 1 must measure append/fsync and
restore cost under them before the feature is enabled.

## Record and authority model (D1, D2)

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
progress, attach evidence and enter `review_pending` within their assignment;
only the authorised supervisor/operator changes acceptance, reassigns ownership,
pauses/stops a loop or approves completion. Preserve the existing goal API's
self-pause/self-stop refusal. An unresolved or peer-claimed identity gains no
local authority; do not mistake the existing cooperative ancestry check for a
security boundary. Use ADR-0036's verified principal and scope path for new
agent-facing structured commands.

Acceptance is immutable within a revision. Checks are structured argv/cwd
references under the authorised workspace, not shell strings taken from GitHub
or receipt text. Running checks requires an explicit capability, a process/birth
identity, a bounded worker and captured result/evidence. Under D11 the runner is
provisioned by the operator, never selected from repository text, and runs as a
dedicated unprivileged account rather than the server/operator UID. Materialise
only declared inputs into a read-only snapshot, with a private writable scratch
area and preloaded read-only dependency cache. Deny network access and host
home, SSH agent sockets, credential stores, undeclared mounts and inherited file
descriptors. Clear the environment; admit only a configured PATH/locale and
private HOME/XDG/temp paths, with no inherited FLOCK_*, GH_*, cloud or model
credentials. The server gains no new root/sudo capability. A missing isolation
backend, missing offline dependency or timeout refuses the check; never fall
back to running under the operator identity or claim a green result. This ADR
does not silently execute legacy `GoalRecord.checks` strings. Changing
acceptance invalidates prior approval. Evidence is typed: repo/PR/head, CI
run/attempt and conclusion, receipt/task/revision/content hash, or check
invocation/exit/result hash. A link or a `DONE:` line is a claim until verified.
Old-head green CI cannot satisfy a new-head acceptance check. Approval names the
exact acceptance revision and evidence set; unknown, missing or unreadable
evidence blocks it.

The accepted event design retains ADR-0038's `work.created`, `work.updated`,
`work.edge_added`, `work.edge_removed` and `work.closed`, with typed
`work.loop_changed`, `work.wake_changed` and record-carrying `work.checkpoint`
payloads. Each mutation carries its actor, operation ID, revision and work ID;
the occurrence variant carries both its cursor advance and wake intent.
Checkpoint copies are not new semantic changes. These are unimplemented schema
additions, not existing persisted kinds.

### Subscription authority and information flow

Registration requires the operator, the attested supervisor of the work, or an
assigned agent with an explicit `loop.manage` grant bound to that work and its
own wake target. An assigned agent cannot subscribe on behalf of a sibling or
choose an arbitrary recipient. Ancestor routing comes only from attested work
and delegation edges. Cancelling or weakening protection remains supervisor/
operator-only. A management grant is not a source-read grant. `loop.manage`
names an accepted assignment capability, not an implicit extension of any
existing token scope.

At registration, predicate expansion, reconciliation, each firing, delivery and
read of loop status, intersect the principal's `read` scope with per-work,
per-repository and per-source subject grants. Every predicate field, event kind,
repo, branch and evidence reference must be readable. Validate every escalation
recipient and operator-channel binding for the same data. Refuse an unreadable
predicate before installing it or adding its repo to the shared watch set. Use a
uniform authorisation refusal, without revealing whether a private repo, agent
or event exists. Revocation suspends observation/delivery and strips old
payloads from unauthorised public views; pending custody remains protected for
an authorised operator to reconcile. Do not leak event existence through cursor
movement, counts, diagnostic text, notification bodies or predicate outcomes.

This follows the binding [#2585 scope
ruling](https://github.com/gerchowl/flock/issues/2585#issuecomment-6471599727):
scopes grant exactly what they name. `send` does not read events or register
loops; `read` does not manage loops or wake processes; `operate` implies neither
read nor send. The accepted receipt capability in D9 is narrower than generic
lifecycle authority and requires separate owner acceptance. A powerful polling
credential never expands a subscriber's grants.

The compatibility mapping retains the existing active agent-goal binding without
inventing multiple active legacy goals. Imported checklist IDs are scoped to
their old goal; `done: true` is imported as a legacy claim, not retroactively
verified evidence. Preserve missing legacy actor information as unknown rather
than inventing an attested author. A removed agent leaves its work
unassigned/blocked until an authorised reassignment; a new agent with the same
name inherits no authority.

## Durable subscription and delivery protocol (D3, D6)

A loop definition contains its work/agent IDs, predicate, source cursors,
occurrence policy (once or recurring), progress/deadline policy, escalation
recipient and ownership epoch. Held CLI/MCP reads remain connection-scoped;
ending a read does not cancel a durable loop. Extend the wait service instead of
building a second independent predicate/wake engine. Existing `wait.park`
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
   if every in-process notification is lost. If outbox capacity refuses the
   transition, leave the source cursor at the unconsumed event. Capacity release
   wakes the dispatcher to re-evaluate that same event with the same occurrence
   key, not a timer/spin retry. Reserve bounded control-state space for the
   loop's capacity/coverage error and escalation even when the data outbox is
   full. A source that expires that history produces a visible coverage gap,
   never a silently advanced cursor.
3. An owned I/O worker appends/syncs before publishing a committed transition or
   answering success. Introduce a fallible acknowledged write path; the current
   best-effort `EventHub::push` cannot be the commit acknowledgement. On write
   uncertainty, expose a typed unresolved outcome and reconcile by operation ID;
   never answer “registered”, advance a cursor or announce handled prematurely.
   Keep filesystem work and fsync off the server deadline dispatcher.
4. Admit a `Kind::Wake` at normal priority/at idle through the existing queue
   worker. Its internal producer is authenticated server context. The wake ID
   determines the queue item ID and immutable payload hash. Repeating the same
   ID/body observes the prior result; same ID/different body refuses. Queue
   commit and loop-outbox commit are not one transaction: after a crash between
   them, look up/admit that same item ID, never mint a replacement. Preserve
   dedupe evidence until the occurrence is handled, including through queue
   compaction and runtime-generation changes. Commit uncertainty never causes a
   blind resend under a new identity.
5. Harness-started and queue-done are transport/turn facts, not proof that the
   requested supervisor action happened. `work.wake.ack` must name the wake,
   assignment and expected revision. Persist the acknowledgement before
   replying. Repeated admission requests retain the wake/item ID; only one queue
   item may be active for it. A committed/started item is not blindly written to
   stdin again: an unhandled occurrence remains in the next turn's context and
   escalates at its deadline. Queue-done does not get rewritten to queued.
   At-least-once admission is not automatic replay of an accepted turn. Consumer
   actions must query their effect or use their own idempotency keys.
6. Check each wake against the current assignment or authorised escalation role
   and authority before delivery. Reassignment, cancellation and handoff fence
   stale completions and late acknowledgements. Human attention, denied
   permission, retired sessions and unknown turn completion remain visible
   pending/refused states; no loop auto-answers a permission or treats stdin
   bytes as accepted work.

All open work records, definitions, source cursors, pending wakes, dedupe state,
acknowledgements and escalation deadlines participate in bounded record-carrying
checkpoints. Reserve checkpoint space before rotation and fence further
rotations until the new active file durably covers the complete state; do not
depend on a later event-loop pass getting CPU before old coverage rotates out.
Restore folds before serving work status or dispatching wakes. Readers skip and
preserve unknown kinds/fields per ADR-0028 §3a, and refuse unsafe rewrites with
unknown state. Every implementation slice adding persisted kinds tests that
reader contract.

The 8 MiB checkpoint budget is aggregate, not one log line or wire frame. Encode
pages of at most 64 KiB plus a final manifest naming their epoch, hashes and
coverage cursor. Only a complete synced manifest replaces prior coverage; a
crash midway retains the previous checkpoint and its subsequent events. Internal
checkpoint pages are not work deltas. Public snapshots are paged under the
existing frame limit, so checkpointing cannot create an undeliverable event.

Terminal records are retained/re-issued until explicitly archived by an
authorised actor. Archival warns that older history can rotate out and offers a
bounded export; it is not an implicit response to a full store. Event history
remains retention-bounded, even when the current work record survives. Evidence
references do not promise indefinite availability of external PRs or receipts.

### Observation sources and coverage

- **PR/CI:** extend the shared poller with watched subjects independent of a
  visible UI; no per-loop `gh --watch`. D10's polling owner resolves its
  enrolled App-installation credential reference outside the model/check
  environment, restricted to selected repos with metadata, pull-request, check
  and Actions reads only; no write/admin scopes or agent/ambient user token
  fallback. A request's subscriber must independently have the repository read
  grant both before watch-set admission and before receiving any observation.
  Installation access alone is not subscriber access. Use one token/budget owner
  per credential reference; carry its spending window through restart/handoff,
  and do not fork a poller per agent or loop. Observe run/attempt identity as
  well as head, so a rerun's failure is not hidden by the existing
  first-conclusion ledger. Reconcile current state at registration/restart,
  including merged before registration and a new head with no checks.
  Batch-landed close is a typed policy observation, not any arbitrary closed PR
  being treated as merged.
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

The recommended GitHub adapter polls every 120000 ms, has a 10000 ms request
ceiling, and spends at most 1000 GraphQL points in its configured 3600000 ms
budget window, retaining a 500-point provider reserve. Account actual reported
cost; reserve the query's bounded worst-case cost before issuing it and retain
the reservation when timeout leaves cost unknown; cap concurrency at two. These
values are accepted registry defaults below, not claims about available GitHub
quota. Honour provider remaining/reset and Retry-After even when they are
stricter; track concurrent outside spending as lost budget certainty. Exhaustion
defers observation to the provider's reset or local budget boundary and exposes
rate-limited/source-stale status, rather than spawn extra requests or borrow
another identity. No inbound endpoint is opened under polling. If the owner
chooses webhooks, signature validation and durable source-ID dedupe precede
admission; webhooks cannot bypass per-repo read checks, and periodic
reconciliation remains necessary. Polling/webhook delivery never executes issue
or PR text as commands.

Every source has a last-success/coverage cursor, health and a bounded
observation latency. Network outages, rate limits, source panics and overflow
are visible. Reconciliation can recover current conditions, but cannot invent
every transient external state missed during downtime. Emit a coverage-gap
condition requiring review rather than claim the stream was complete. Polling
belongs only in bounded external adapters where push is unavailable; the wake
scheduler itself is driven by committed changes and deadlines, not a sleep/retry
loop.

## Progress, compaction, restart and handoff (D4, D5)

An agent finishing a turn, a tool call, pane output or the supervisor's own wake
is not automatically progress. Persist meaningful progress against a work
revision or a finite declared long-running-job lease. The accepted default
progress deadline is 15 minutes and the maximum declared lease is one hour. All
policy timings resolve through the ADR-0042 entries below, not local timers with
hidden defaults. Core progress needs an attested action receipt (for example a
bound delegation or queue effect) or explicit operator progress confirmation. A
wake acknowledgement, status-text edit or empty revision bump alone is not
meaningful progress; rich PR/check verification is added in its later slices.

Registration arms the progress deadline even when no external event is pending,
unless the loop enters the verified healthy-source wait below. If it expires
with no existing wake, commit one timer occurrence keyed by loop ID and
progress-boundary revision through the same occurrence/outbox path. Silence
cannot turn the timer off or require a shell waiter to re-arm it.

### Bounded healthy-source waiting

Persist a typed `waiting_healthy_source` state, bound to the armed LoopId,
source kind, exact subject ID/revision, assignment and predicate revision. Entry
requires a completed authorised poll covering that subject within
`work.loops.source_stale_after_ms`, with the awaited object explicitly still
open/pending. A scheduled request, another repository's successful poll, an
unchanged cursor or absence of a receipt is not health evidence. Store the
observation and last successful coverage time, original wait start, resolved
policy revision and hard expiry in the checkpoint.

While these conditions hold and neither the hard nor no-change expiry has
arrived, suppress the ordinary inactivity occurrence and its
escalation/reminders for this wait. Waiting calmly on a healthy long batch must
not page the operator every fifteen minutes or consume the one-hour action
lease. This is an observable typed state, not an assertion that polling or a
status edit is meaningful work progress. It cannot mask an outstanding
actionable wake, unrelated escalation or overdue handling obligation. Entering
it does not clear an existing incident.

The first source error, staleness boundary, terminal observation, no-change
expiry or hard expiry ends the state immediately, without waiting for another
agent turn. Persist a typed exit reason (`source_error`, `source_stale`,
`subject_terminal`, `wait_limit` or `subject_unchanged`) with the observation.
Terminal observations use the ordinary occurrence/cursor/outbox transition;
stale/error/limit/no-change exits create one keyed attention occurrence and
expose degraded coverage. Retain wake custody through capacity/commit
uncertainty. Never restart an already overdue inactivity timer as if the agent
had just progressed, or page for that obsolete boundary while the resulting wake
has a fresh handling window.

Each supported source kind has a finite hard maximum in the ADR-0042 registry:
GitHub PR/CI/landing-batch subjects default to four hours, bounded at most 24
hours; receipt subjects and agent-phase subjects retain their one-hour defaults
and 24-hour ceilings. In addition, all supported kinds have a two-hour
no-subject-change limit, owned by `work.loops.wait.no_subject_change_ms`.
Persist the last verified subject-state change and its deadline. Only a new
check, job or status for that awaited object resets this boundary; successful
polls, heartbeat timestamps, status-text edits by the waiter and repeated
observations do not. Initial entry starts it when no earlier change is known;
re-entry, restart and handoff preserve it. A real subject change resets only
this boundary, never the hard maximum. The earlier boundary wins. Receipt
waiting requires a typed pending producer object and fresh producer coverage,
not merely a successful directory scan; agent waiting requires a freshly
observed pending assigned work object, not an idle process alone. Disk
thresholds have no open/pending object and do not support this state.
Unsupported kinds refuse it. Repeated polls, re-entry for the same awaited
object, acknowledgements, restart and handoff never renew the original maximum;
poll revisions and CI reruns are not new wait generations. A new wait after
expiry requires verified progress or an explicit authorised operator decision
recorded as a new wait generation. Policy shrinkage may expire it immediately;
reload cannot extend its persisted hard boundary. Revoked read authority ends
observation under the existing suspension/scrubbing rules, never preserving a
claim of healthy coverage.

### Fresh handling boundary after delivery

`work.loops.handling_timeout_ms` is separate from the inactivity/progress key.
The first committed queue `DELIVERED` fact for each wake ItemId (the harness
reported it started, not merely admission or a stdin write) starts a fresh
handling deadline, default fifteen minutes. It does not inherit an overdue
progress deadline. Duplicate delivery observations and replay reconstruct that
same boundary; they cannot renew it. Prior to delivery, retain the owed wake and
its visible queue/lifecycle/delivery escalation policy. Delivery cannot rewind
an escalation already opened for an independently failed obligation. An
unanswered delivered wake cannot enter healthy waiting to escape its handling
deadline. A verified handling acknowledgement may establish a new healthy wait
only with the exact source/subject evidence above; a bare acknowledgement
cannot.

### Escalation must leave the sleeping supervisor's context (D8)

At an unsuppressed progress deadline or an expired handling deadline, open one
durable escalation keyed by the wake and work revision. Its chain starts at the
assigned agent's attested supervisor, then that supervisor's supervisor, and
ends at the operator. Allow five minutes per agent hop; at most eight agent
hops, with a visited-ID set to reject cycles. A missing, retired, Stopped,
Crashed or unauthorised recipient is skipped immediately, recording a typed
recipient-unavailable reason, never waiting on a name that may now identify
someone else. A confirmed Hibernated recipient can wake only under D9. A
permission-blocked or unresponsive recipient still owes progress; expiry
advances the chain without answering its permission prompt. A root supervisor
with no parent advances directly to the operator. If no safe ancestor path
exists, go directly to the validated operator binding.

With healthy sources and delivery, the accepted defaults reach the operator
within the applicable 15-minute progress or fresh handling interval plus at most
eight five-minute hops (55 minutes), plus observation latency (normally up to
two minutes for polling) and scheduling/transport overhead. A verified healthy
wait delays this budget until its exit condition; the finite per-kind maximum
and no-subject-change limit still apply. This is a policy budget, not a hard
real-time or human-response guarantee; vanished ancestors shorten the route
immediately.

At operator stage, file durable attention and notify an enrolled, healthy GUI
endpoint: the Tauri shell's native desktop notifications, plus the Tauri
iOS/Android app with remote push. #2733 owns the mobile reachability, push relay
and device-enrolment design; it depends on ADR-0043 authenticated remote access.
Do not invent that transport here or claim a foreground-only inbox provides
mobile background delivery. Until mobile push ships, the operator may choose an
optional #1005 tg-mcp/Telegram binding as an interim stand-in. Remove that
adapter when mobile push ships; #2734 tracks removal and migration of unresolved
escalation custody. Telegram is not the permanent required path.

Both endpoint types must satisfy the same enrolment, source-read and health
contract before protected loops arm. A desktop endpoint alone proves only its
available desktop delivery, not away-from-desk mobile reachability; expose that
coverage honestly. Use scoped credential references and authorised destinations;
no token enters model context, receipt text or a check runner. An endpoint's
acknowledge action invokes only `work.escalation.ack` for that escalation; it
grants no permission-answer, start, send or general operator capability. The GUI
inbox projects the same attention record. The existing [notification
log](../../src/app/notifications.rs) can evict unread entries at its cap and
lose older history at rotation, so authoritative unresolved escalation stays in
the work checkpoint and is re-projected by ID. A toast, socket write, OS
delivery or push receipt is not a human acknowledgement.

Reminders are **ON by default every 15 minutes**, at most one per escalation per
interval, continuing while unresolved. Coalesce them onto the same attention
record rather than accumulating an unbounded queue of reminders. A human or
supervisor acknowledgement buys a finite 30-minute responsibility lease; it does
not silently resolve the work. Repeating the same acknowledgement cannot renew
that lease or rewind a hop; another lease requires verified progress. Meaningful
verified progress/handling resolves that escalation and establishes the next
progress boundary. A lease with no progress expires back into the chain/reminder
policy. Merely reading a GUI card or acknowledging transport delivery does not
suppress reminders.

Protected-loop arming refuses when the operator binding is absent, lacks the
required source-read grant or cannot pass its delivery-health check. If it goes
away later, retain the escalation and a typed delivery-degraded state, file the
local attention/OS fallback and continue bounded health checks and reminders;
never mark the incident delivered/handled or present the fallback as equally
observable. External channel delivery needs a stable escalation/delivery ID and
an uncertain outcome on a lost response, not a newly minted duplicate action.
Once a recipient has legitimately received data, revocation cannot retract that
copy; it prevents subsequent disclosures. The whole-host-down limitation still
requires an independently hosted monitor.

### Finite pauses and lifecycle wake (D4, D9)

A new protected-loop pause always has a finite expiry: 30 minutes by default,
with a 24-hour maximum. It suppresses automatic delivery and reminders only
until that persisted deadline; retained pending state and the remaining pause
are visible throughout. At expiry, re-evaluate overdue work once and resume the
chain. Resume never grants a fresh progress lease just because it was called.
Permanent cessation requires the existing authorised, audited cancel/stop path.
Legacy unprotected `goal.pause` remains as implemented; if a compatibility goal
owns protected loops, its pause maps to this finite policy and reports the
effective expiry. Replaying a pause operation cannot extend its expiry; a new
authorised pause is a new explicit, audited action. This explicitly narrows
ADR-0030's indefinite pause for protected loops, rather than letting an old API
disable them forever.

D9 coordinates with [ADR-0044
Q2/Q5](https://github.com/gerchowl/flock/pull/2728) and [#2731's AnswerReceipt
wake](https://github.com/gerchowl/flock/pull/2731), at the inspected heads
above. A committed LoopWakeReceipt binds the accepted loop, occurrence,
authorised recipient node/agent/generation, original ItemId and the owner-issued
protection grant. It may resume only that confirmed Hibernated agent for that
item. For an ordinary wake this is the assignee. An escalation hop can mint a
separate receipt only for the attested ancestor already allowed by the
protection grant and source-read checks, with a separate hop-bound ItemId. It
cannot retarget or reuse the child's receipt to start a different agent.
Validate custody, current assignment/role, ownership recovery, absence/cleanup
proof and unchanged session at wake admission; do not trust a wake flag in user
JSON. It authorises no general start/focus, no new conversation and no
permission grant. Always-grant lifetime follows ADR-0044 Q4's accepted decision;
the receipt itself restores no permission authority. Read/list/subscribe, an
uncommitted event and an unregistered predicate cannot mint it.
Stopped/Crashed/replaced/unknown targets retain explicit-start/recovery
requirements and trigger the escalation chain. On restart, a restored Hibernated
record alone never spawns. Replaying an unhandled, committed receipt after
ownership recovery permits only that narrow exception to ADR-0044 Q6's
cold-start rule, alongside #2731's owed-answer wake. The joint Q6 decision is
accepted; completed receipts and checkpoints are not new wake triggers and
cannot resume every saved session.

A committed pending wake or unresolved admission blocks ADR-0044 Q2's idle
quiesce. An idle loop with no occurrence does not. Before the generation-scoped
freeze commits, an admitted wake cancels hibernation; after freeze it records
wake owed, waits for confirmed cleanup, then coalesces with send/answer wakes
under ADR-0044's one-per-agent and node limits. Preserve the original ItemId,
rotate generated MCP credentials and use ADR-0044 Q9's original-input bootstrap.
A possible stdin write remains held/uncertain, not automatically replayed.
Lifecycle wake target/readiness/grace policy remains owned by ADR-0044 and its
pending registry entries below. D9 and the accepted companion Q2/Q5/Q6 and S6e
choices must be implemented before advertising automatic hibernated-loop wake;
no lifecycle authority is inferred from #2585's send/answer scopes.

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
is live. D9 may revive a confirmed Hibernated assignee; it cannot start a
replacement. If recovery/wake refuses or never becomes live, the deadline
escalates.

For same-host live handoff, stop the exporting writer before import/refold and
carry the complete pending state before answering status. Cooperative cross-host
handoff additionally needs an authenticated, revisioned export and durable
release of the old authority before the target can accept a new ownership epoch.
The old authority must checkpoint its release tombstone and refuse further
writes after release; old workers cannot commit into the new epoch. Interrupted
transfer remains visibly `handoff_pending` and resumes by transfer ID. A
partition without proven fencing refuses takeover; reusing a local lock file or
copying a receipt is not proof. Remote command transport and its coordinated
mesh-version rollout remain #1380's dependency; this ADR makes no mesh-v5
command or distributed-failover claim.

## Timing registry and apply contract (ADR-0042, #2724)

These are accepted registry descriptors, not implemented settings. All durations
are integer milliseconds with explicit finite bounds. `G/N/A` below admits
global, node and operator-owned agent contexts; `G/N` admits global/node only.
Project files cannot change supervision timing, notification destinations,
credential bindings or check sandbox policy. Only an authorised supervisor or
operator may set a protected agent's policy, under config-specific grants.

Use ADR-0042's single typed registry and destination resolver, with explicit
node/project/agent IDs: **defaults < file < env < explicit owning-process flags
< runtime**; within an admitted source, global < node < agent. In particular,
node env beats agent file. Runtime resets reveal the lower layer, and restart
clears runtime overrides rather than treating them as durable policy. Resolve
current policy before rearming on restore; persisted deadlines keep the earlier
finite boundary if losing an override would lengthen protection. No alias/cwd
scope inference, agent-side env parser or per-loop precedence scheme is added.
Canonical env names are generated under `FLOCK_CFG__` with `__` separators, e.g.
`FLOCK_CFG__WORK__LOOPS__PROGRESS_TIMEOUT_MS`; secret bindings are references
only. Source admission/security floors apply before precedence. Each loop stores
resolved policy revision/provenance; typed runtime `config.set` is the override
path, not an arbitrary duration smuggled in a subscription payload.

| Accepted registry key | Default / allowed range | Scope / consumer and reload |
| --- | --- | --- |
| `work.loops.progress_timeout_ms` | 900000; 60000–86400000 | G/N/A; progress dispatcher, recompute from the last verified progress boundary |
| `work.loops.handling_timeout_ms` | 900000; 60000–86400000 | G/N/A; handling dispatcher from first committed DELIVERED per wake ItemId; replay never renews it |
| `work.loops.wait.github_max_ms` | 14400000; 60000–86400000 | G/N/A; PR/CI/landing-batch healthy-wait maximum from original entry; reload may shorten, never extend that wait |
| `work.loops.wait.no_subject_change_ms` | 7200000; 60000–7200000 | G/N/A; all supported healthy waits, from last verified check/job/status change; polls and replay never renew it |
| `work.loops.wait.receipt_max_ms` | 3600000; 60000–86400000 | G/N/A; typed pending receipt subject with fresh producer coverage; original-entry maximum |
| `work.loops.wait.agent_phase_max_ms` | 3600000; 60000–86400000 | G/N/A; pending assigned work with fresh phase coverage; original-entry maximum |
| `work.loops.max_progress_lease_ms` | 3600000; 60000–86400000 | G/N/A; reject/shorten newly requested leases beyond the bound; shrinking cannot erase an already raised escalation |
| `work.loops.pause_timeout_ms` | 1800000; 60000–86400000; no null/0/infinite | G/N/A; pause dispatcher, expiry from original pause boundary; an authorised request may shorten it |
| `work.loops.escalation.hop_timeout_ms` | 300000; 60000–900000 | G/N/A; agent-hop deadline from hop entry, never waiting on a missing recipient |
| `work.loops.escalation.max_hops` | 8; 1–32 | G/N/A; attested route budget; loops/cycles refuse rather than await this timer |
| `work.loops.escalation.reminder_interval_ms` | 900000; 60000–3600000; positive and ON by default | G/N/A; next reminder from previous attempt, coalesced by escalation; reload cannot discard an overdue reminder |
| `work.loops.escalation.ack_lease_ms` | 1800000; 60000–3600000 | G/N/A; finite acknowledgement lease, from the acknowledgement boundary |
| `work.loops.source_stale_after_ms` | 300000; 60000–3600000, at least twice the source poll interval | G/N; health dispatcher from last successful coverage observation |
| `work.loops.github.poll_interval_ms` | 120000; 30000–3600000 | G/N; shared poll owner, watched subjects survive absence of viewers |
| `work.loops.github.request_timeout_ms` | 10000; 1000–30000 | G/N; future requests only; timeout reports uncertainty/unavailable, never a fabricated conclusion |
| `work.loops.github.budget_window_ms` | 3600000; 60000–86400000 | G/N; quota owner, preserve accounted spending across reload rather than granting a fresh budget |
| `work.loops.github.budget_points` | 1000; 1–5000, additionally bounded by provider quota | G/N; shared credential budget; no guarantee of unused quota |
| `work.loops.github.reserve_points` | 500; 1–5000 | G/N; provider remaining-point floor before a query; provider reset/Retry-After are external facts, not configurable retry timers |
| `work.loops.github.concurrency` | 2; 1–4 | G/N; admission for future requests, no per-loop workers |
| `work.loops.receipts.reconcile_interval_ms` | 60000; 1000–3600000 | G/N; registered-root reconciliation; startup/overflow also reconcile immediately |
| `work.loops.receipts.read_timeout_ms` | 5000; 1000–30000 | G/N; owned reads; a timed-out filesystem operation remains fenced and consumes its worker slot until it actually ends |
| `work.loops.disk.sample_interval_ms` | 60000; 1000–300000 | G/N; capacity sampler; threshold transitions and sampler health remain separate |
| `work.loops.notification.request_timeout_ms` | 10000; 1000–30000 | G/N; endpoint delivery requests, unknown delivery on timeout does not acknowledge the escalation |
| `work.loops.notification.health_interval_ms` | 60000; 1000–300000 | G/N; enrolled endpoint delivery-health checks; failed checks keep degraded state |
| `work.checks.timeout_ms` | 60000; 1000–900000 | G/N/A; isolated check worker; bounded cancellation/reap failure is an error, never pass |
| `sessions.structured_hibernate_idle_ms` | ADR-0044 Q2/Q10: default 0, proposed enabled value 300000 | G/N; lifecycle owner; an active loop alone is not an idle blocker, outstanding wake custody is |
| `sessions.structured_wake_target_ms` | ADR-0044 Q7 proposed 5000; 1000–30000 | G/N; target telemetry, not proof of delivery or a spawn-retry deadline |
| `sessions.structured_wake_ready_timeout_ms` | ADR-0044 Q7 proposed 30000; 1000–120000 | G/N; lifecycle readiness owner; coordinate acceptance/range with #2728, do not widen existing stop timers |
| `sessions.structured_hibernate_grace_ms` | Finite default requires H3 measurement and owner approval on #2717 | G/N; lifecycle cleanup owner; no invented grace value here, hibernation integration stays gated until its descriptor is complete |

The lifecycle-only rows are coordinated proposals to ADR-0044's registry work,
not changes to that PR or assertions that the keys exist. Its wake-concurrency
key and permission/cleanup guards still apply. Policy changes prepare consumers
before publication and report per-key outcomes under ADR-0042; no successful
reload may leave an old detached reminder timer. Shorter limits can make an
existing boundary due immediately; lengthening does not clear an incident
already raised. Pauses never acquire an indefinite lifetime on restart. Missing
node identity, unsupported config capabilities or refused source/scope admission
blocks arming with a typed diagnostic. No default-only hidden timers remain.

D8/D10/D11 also require node-scoped operator-owned descriptors for
`work.loops.escalation.operator_binding`, `work.loops.github.credential`,
`work.loops.github.repositories` and `work.checks.executor`. Bindings have no
ambient credential fallback; missing mandatory bindings refuse the affected
protected registration/check. A repo file cannot redirect notifications or
select a credential/executor. Registry declarations, canonical env/reference
codecs, provenance, ranges, source admission and apply tests must land before
activation; this is an explicit #2724/#2719 dependency, not bespoke
configuration in a worker.

## Public surfaces and migration (D7)

The accepted socket verbs are `work.create/get/list/update/link/unlink`,
`work.evidence.attach`, `work.complete.propose/approve`, `work.archive`,
`work.loop.register/get/wait/pause/resume/cancel`, `work.wake.ack`,
`work.wake.progress` and the recipient-bound `work.escalation.ack`. Mutation
inputs carry an idempotency ID and expected revision, never an actor supplied by
the body. Add `flk work` and generated `flock_work_*` MCP adapters over the same
service; keep `flk goal` as the compatibility view rather than maintain two task
stores. Bind grants to read, assigned-work updates or supervisor/operator
actions; existing send authority alone cannot mutate acceptance or cancel
supervision. Register any new refusals and retry classes through ADR-0029's
error-code registry.

The `work` stream first projects existing goals/delegates (#1600), explicitly
showing unknown/unimplemented facts. The later record phase (#1603) uses
actor-attributed events and the cursor/gap/snapshot contract, with an explicit
schema capability and legacy-ID-to-WorkId mapping at the transition. Phase 1
derives view IDs from existing authority/agent/delegation keys without new
records. Those keys can be reused on legacy re-arm: they are not durable WorkIds
or mutation targets. Entering phase 2 requires a complete snapshot reset and
committed migration mapping, so clients cannot confuse two goal lifetimes.
Generate Rust/TS shapes from one source. Add a goal-tree tile and sidebar tree;
show assignment, status, acceptance revision, evidence head, pending wake age,
source health, healthy-wait subject/expiry and overdue acknowledgement without
turning every progress sample into agent context. Render untrusted labels
safely; frame external evidence as data with unforgeable per-item delimiters
when it reaches a model. Neither the GUI nor MCP owns timers.

New verbs/streams must use the existing version/skew gates; an older server
returns a typed unsupported/version refusal, never apparent success with a loop
that was not installed. Any required local protocol bump follows the
released-tag rule. No mesh version change is part of this docs PR.

`goal-s1` integrates as a thin stop/context adapter to the server's goal/work
service, preserving ADR-0030's ladder and human-reserved decisions (#573, #965).
`watcher-s1` remains useful for independently running commands/classification,
but its owned observations enter typed wait/evidence sources; it does not become
the lifetime owner of a flk loop (#663, #857). Cut over one work assignment at a
time: verify server registration and pending delivery, disable its duplicate
script watcher, then remove that watcher. Do not run two uncoordinated
injectors. Provider-specific judges and free-form process/GPU progress remain
their existing scopes (#573, #857), not guessed from new pane scraping here.

## Pre-mortem and failure modes

Assume the owner again discovers a red batch sixteen hours late. Work backwards:

| Failure | Prevention, observable outcome and recovery |
| --- | --- |
| The architect compacted before registering a watcher, or the CLI died after registration | Registration is durable and idempotent; inspect by operation ID after an uncertain response. No acknowledged registration means no claimed protection. An armed work item without its required loop is visibly unsupervised. |
| Event arrived during subscription setup, or restart seeded it as historical | Capture/replay cut plus current-condition reconciliation. Initial terminal facts still create an occurrence; source seeding cannot suppress an armed rule. |
| Disk full, append failure, worker panic or an unacknowledged fsync | Never return committed success or advance cursor from memory alone. Retain pending/unknown state, typed storage degradation and operator-visible recovery. A memory-only notification cannot clear it. |
| Crash after queue commit but before outbox acknowledgement | Replay the same wake/item identity and compare payload, dedupe or return uncertainty; retain enough queue evidence to reconcile after compaction. |
| Healthy long batch caused repeated false alarms | Subject-specific successful coverage and open/pending evidence suppress inactivity escalation only until the per-kind hard maximum. Stale/error/terminal/limit/no-change exits end the wait and retain one wake; unrelated obligations remain protected. |
| Agent started a turn but never acted, or acknowledged someone else's wake | Require handled acknowledgement from the assigned identity/revision; progress deadline escalates. Started/done observations do not close the occurrence. |
| PR rerun failed at the same SHA, checks never started or the shared poller stopped | Track run/attempt and source health; reconcile current PR/head/check state, expose missing checks and observation gaps. No viewer must not disable an armed source. |
| Permission pending, agent removed, provider limited, queue held or turn end unknown | Keep the pending reason and escalate; no permission bypass, auto-reassignment by name, background process kill or fabricated success. |
| Wake/receipt storm, repeated low-disk samples or malformed untrusted input | Typed keyed occurrences, transition-based conditions, bounded records/queues and admission refusal. Preserve already committed wakes and report capacity/source errors. |
| Supervisor/parent disappeared, or every agent in the chain stayed idle | Resolve attested parents with a hop deadline and visited set; missing/stopped/unauthorised recipients advance immediately to the validated operator binding. Default endpoint reminders persist without an open inbox. |
| Operator binding vanished or a notification was “seen” but nobody acted | Keep authoritative escalation/control state, fallback attention and endpoint-health error. Transport/read receipts do not resolve it; responsibility acknowledgements have a finite lease. |
| Agent hibernated while wake custody was pending | Joint ADR-0044 Q2/Q5 and #2731 rules gate quiesce, preserve wake owed across freeze/cleanup and permit only receipt-bound resume of the exact confirmed target. |
| A subscriber borrowed a poller's private-repo access | Per-work/source/repo read checks at registration, ingestion and every sink; revocation scrubs public payloads. Credential installation rights do not authorise the subscriber. |
| Two hosts both believe they own the loop | Epoch fence with cooperative release; no promotion without fencing. An unavailable authority is visible, not silently replaced. |
| Clock moved, server stalled in I/O, or the entire host went down | Reachable deadline dispatcher and persisted deadlines; explicit clock/dispatcher degradation. Whole-host absence needs an independent monitor, not a claim of an infallible local loop. |
| Log rotated while the loop was quiet, or a downgrade skipped new data | Record-carrying checkpoint and rotation barrier; unknown data retained and unsafe compaction refused. Old readers cannot claim to operate newer loops. |
| Observer text became instructions, or a peer forged an owner/receipt | Trusted principal at admission, typed evidence verification, safe rendering and framed model context. Refuse unverified authority; never execute commands embedded in an observation. |

“Never drop” means no silently forgotten acknowledged work: each accepted
occurrence remains pending, handled or explicitly failed/escalated. It does not
mean exactly-once external effects, unlimited retention, guaranteed agent
action, a hard scheduling deadline, or recovery from total storage loss without
backups.

## Ordered implementation slices and fail-first acceptance

All estimates below count **all production Rust and TS together**, excluding
tests/docs. Each is a separate PR of at most 400 lines; estimates are planning
bounds, not permission to exceed the cap. If a slice grows, split it before
opening its implementation PR. The listed order is the dependency order; no
record-writing or autonomous wake feature is enabled before its prerequisites.

Every test starts at the real socket/CLI/MCP/wire ingress and ends at its public
status/stream or at the input observed by [the public fake
model](../../tests/fixtures/structured-public/claude.py). Use only that fake
model, isolated server instances, private temporary HOME/data, short socket
paths and no inherited live-server selectors or credentials. Source feeds and
clocks are controlled test adapters, never real GitHub, disks filled on the
host, judges or user panes. Fault seams must be added without changing behaviour
first if needed; show an actual failing assertion against the old behaviour,
never a compile failure for a missing API. Raw JSON calls can exercise an as-yet
unsupported verb before its implementation.

| Order / estimate | Slice and tracked scope | Fail-first public-path acceptance |
| --- | --- | --- |
| 0 / 120 | Shared supplied-clock, source and commit fault seams (#2718) | Start only the fake model through public ingress; inject a silent/dead/unexpected producer and show a bounded assertion failure, not a hang or missing-seam compile failure. |
| 1 / 320 | Acknowledged append / commit-unknown (#2718) | Inject append failure through public mutation: no durable success or cursor advance; restart agrees. Measure accepted-capacity write/restore cost. |
| 2 / 280 | Minimal stable work/wake IDs and revision fold (#2718, D1 gate) | Create the protected assignment through socket, duplicate the operation, restart and observe one stable identity; forged actor/stale revision refuse. No evidence/check/DAG subsystem yet. |
| 3 / 340 | Bounded checkpoints, rotation fence and control-state reserve (#2718) | Interrupt pages before manifest and rotate beyond retention; restore the same work and pending/control state. Unknown kinds survive; no incomplete checkpoint replaces prior coverage. |
| 4 / 320 | Durable loop register/read/cancel with source-read grants (#1240, #2585, #2718) | Register for the fake's readable phase, disconnect/restart and inspect the same loop. A loop targeting an unreadable repo/agent refuses without leaking event existence, count, cursor or notification; revoke access after registration and prove public status/delivery is scrubbed. |
| 5 / 320 | Occurrence/cursor/outbox atomic transition (#2718) | Fire between registration cut and commit, replay twice and get one pending occurrence. Fill the outbox, refuse the next event without cursor movement, release capacity and observe the same event admitted once without a timer retry. |
| 6 / 340 | Stable queue admission and retained wake dedupe (#2718) | Submit the same wake twice across compaction/recovery: one active ItemId and one fake input. Different body/same ID refuses; queue-commit crash reconciles by that ID. |
| 7 / 300 | Outbox worker and actual idle delivery (#2718) | Gate the permission book and release with no later model event; fake receives the original wake. No started/handled claim precedes the appropriate durable/harness fact. |
| 8 / 320 | Receipt-bound Hibernated wake (#2728 Q2/Q5/Q6, #2731, D9 gate) | Let the fake enter confirmed Hibernated; one committed LoopWakeReceipt resumes once for the original ItemId with no new start/send/operate right. Pre-/post-freeze arrivals cancel hibernation or retain wake owed until cleanup. Reads, uncommitted events, Stopped/Crashed/replaced/uncertain identities never spawn; pending custody blocks quiesce. Restart can run only the owed committed receipt, not all saved sessions. |
| 9 / 380 | Ack/progress/handling deadlines, healthy waits and finite pause (#1044) | Start/end a fake turn without progress and emit no external events; advance supplied clock while I/O is held and observe one timer occurrence and overdue state. A late DELIVERED wake gets the full fresh handling interval, unchanged by replay. Register a healthy pending subject, advance beyond the ordinary progress/lease intervals with successful polls and assert no escalation or reminder intents; separately inject stale/error/terminal/maximum exits and assert immediate wait exit, one occurrence and the fresh handling boundary after delivery. Keep a changing pending subject healthy until four hours, then assert wait_limit at the default hard maximum; separately keep polls healthy but the object unchanged and assert subject_unchanged at two hours. A real new check/job/status resets only the no-change boundary. Repeat both across restart without renewing their boundaries; overrides beyond 24 hours refuse. Default pause expires after its persisted finite deadline, including restart; resume/ack without progress cannot disable protection indefinitely. |
| 10 / 320 | Attested supervisor chain and durable attention (#1044, D8) | Fake supervisor does not act: advance each hop and reach operator. Retire a parent, create a cycle or use an unreadable recipient: skip/refuse safely and keep escalation owed; source data never reaches an unauthorised notice. |
| 11 / 340 | Operator notification outbox / enrolled desktop endpoint (D8, #2733) | Controlled Tauri native-notification endpoint receives escalation and reminders with one escalation ID while the inbox is closed. Missing/unhealthy endpoint refuses arming; later endpoint loss retains degraded/unresolved custody. Acknowledge grants only the finite lease; no progress reactivates reminders. Test the optional interim #1005 binding against the same contract; its removal/migration is #2734. Fake inputs remain unduplicated. |
| 12 / 320 | CI/landing-batch RED adapter, GitHub credential/read check/quota producer (#663, D10) | Fake source reports same-SHA rerun failure, already-merged PR and rate exhaustion: public loop fires/degrades correctly without viewer/per-loop poller. App access with no subscriber repo-read grant refuses and emits no private event. Provider reset/request timeout do not become success. |
| 13 / 240 | Typed receipt ingress (#2718) | Same receipt twice yields one verified event; wrong task/head/hash refuses and cannot inject model instructions. |
| 14 / 320 | Receipt-file compatibility (#2718) | Atomic rename/restart fires once; partial/symlink/oversized records and overflow trigger bounded errors/reconciliation without silently skipping a source cursor. |
| 15 / 220 | Full phase/attention and disk health adapters (#663, #1044) | Drive fake phase/permission/background signals plus injected disk low/recovered/low; public transitions differ from sample count. Failed sampler is unhealthy, never healthy-by-absence. |
| 16 / 280 | Work DAG and assignment changes (#965, #2718) | Public link/reassign refuses cycles; same-named replacement cannot mutate the prior assignment or receive its wake. |
| 17 / 280 | Evidence and acceptance revision (#2718) | Attach typed receipts/PR/check evidence, change head/revision and observe stale evidence refusing approval. |
| 18 / 320 | Isolated check runner (#2718, D11) | Configured fake-fixture check under declared UID/sandbox cannot inherit a sentinel credential, host home or network; unavailable sandbox/unauthorised argv, nonzero exit, silence and worker death never yield green evidence. |
| 19 / 220 | Completion proposal/review/archive (#2718) | Fake delegate cannot self-approve root; authorised review names exact evidence. Archive requires authority and warns of retention consequences. |
| 20 / 280 | Same-host handoff reconstruction (#1240, #2718) | Handoff between occurrence/admission/ack preserves IDs, pause/escalation/reminder deadlines and pending custody, with no implicit replacement start. |
| 21 / 340 | Cooperative cross-host transfer (#2718, gated #1380) | Two isolated authorities with the fake transfer one assignment; old epoch refuses, interrupted transfer resumes by ID, missing release refuses takeover. |
| 22 / 280 | CLI work/loop surface (#2718) | Drive create/register/status/ack through CLI and inspect committed/pending/unknown outcomes; older isolated server refuses new verbs truthfully. |
| 23 / 280 | MCP work/loop surface (#1603) | Real fixture MCP calls observe the same revision/cursor; read/send-only or unrelated-agent calls cannot manage subscriptions, cancel protection or broaden event access. |
| 24 / 280 | Read-only work projection (#1600, its ADR-0038/S7 gates) | Actual wire subscription projects existing goals/edges with empty bootstrap and gap/synced; no invented evidence or durable record IDs. |
| 25 / 320 | Record-phase stream transition (#1603, D7) | Reconnect real client around checkpoint/migration: explicit reset/mapping, no missing/duplicated folded node or unreadable field. |
| 26 / 340 | Goal tree and GUI inbox projection (#1600) | Browser against isolated fake server shows task/evidence, pending/source/ack and persistent operator escalation badges; safe rendering and gap replacement hold. Closing the inbox does not stop enrolled native/push endpoint reminders. Mobile push integration follows #2733 and ADR-0043; no foreground-only delivery is claimed. |
| 27 / 300 | Goal/adapter migration (#573, #965) | Public SessionStart compact/resume boundary for the fake retrieves bounded goal/check/status/delegates and pending wakes. Legacy protected pause is finite; no shell waiter or duplicate injector is started. |

Slices 1–11 form the **minimal durable wake and visible escalation** chain,
using a built-in authorised phase source; record writes cannot be enabled before
checkpoint/authority prerequisites. Evidence, checks, source-specific ingestion,
CLI/MCP adapters and GUI come after it. PR/CI protection is advertised only once
slice 12 also passes (including both healthy-wait and exit paths against its
real adapter), and hibernated protection only after the joint lifecycle slice 8
dependencies are accepted/implemented. The enrolled operator endpoint/outbox in
slice 11 is core delivery infrastructure, not the later goal-tree GUI. Native
desktop notifications come first; #2733 mobile remote push follows its ADR-0043
dependencies, and must use the same durable attention/acknowledgement contract.
Interim Telegram support is optional, subject to the same healthy-binding gate,
and removed under #2734 when mobile push ships. Until an enrolled GUI or interim
endpoint is healthy, protected-loop arming refuses instead of silently falling
back to an inbox. Slice 12 follows the minimal chain immediately because
CI/landing-batch RED is the exact incident source; it takes priority over
receipt adapters, evidence, checks and user interfaces. Slice 9 proves the wait
fold with a controlled source; slice 12 proves the same transitions through the
CI adapter. Projection slice 24 may ship independently under #1600's own gates,
but must not delay or claim to complete the minimal wake chain. For each
implementation PR record the red command/assertion and exit code, restore the
fix, and run focused nextest/client checks. Fake-clock advancement drives
conditions; no test sleeps, timed retries or wider ceilings may manufacture
ordering (ADR-0037).

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
  recommended bounded single-authority design does not implement or promise
  them.
- Work projections/GUI and durable work writers stay tracked separately by
  [#1600](https://github.com/gerchowl/flock/issues/1600) and
  [#1603](https://github.com/gerchowl/flock/issues/1603). Neither feature is
  advertised as shipped by this accepted design.
