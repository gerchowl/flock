# ADR-0044: Durable structured lifecycle and idle hibernation

- Status: Accepted (owner decision 2026-10-29)
- Date: 2026-10-29
- Issues: [#2717][source-1]
- Decision owner: repository owner; all recommended options accepted on
  2026-10-29: [owner decision on #2728][owner-decision].
- Scope of acceptance: Q1–Q10 and the H1–H10 implementation plan below,
  including the narrow receipt-bound wake coordination with ADR-0045 D9.
  Acceptance records design, not shipped functionality. Q2's upper bound,
  Q8's maximum and H3's finite grace still require owner approval on #2717;
  acceptance supplied no unmeasured numbers for them.
- Implemented: none by this ADR. This PR changes documentation only.
- Builds on: ADR-0028 (observations), ADR-0036 (structured runtime and custody),
  ADR-0038 (GUI), ADR-0041 (configuration); coordinates with accepted directions
  in ADR-0042 ([#2724][source-2]) and ADR-0043 ([#2727][source-3]).
- Amends: ADR-0036 §1/§1a/§3/§4 and its cold-restore rule; ADR-0028's
  observation axes remain separate.

## Why a separate record

[#2717][source-1] requests cheap idle `claude -p` agents with durable lifecycle
states and send/focus wakeups. It references [#1430][source-4],
[#1307][source-5], [#2293][source-6] and [#2367][source-7]; their bodies and
comments inform this decision. The GUI remains a client of the owning server,
not a second runtime manager. The configuration decision is ADR-0041, including
redaction for every caller and env-over-file precedence for admitted env keys,
not a new GUI-only setting. This does not unblock `structured.*`: Q10 below
explicitly decides placement/source admission, coordinated with ADR-0042.

This separate record preserves the inspected implementation baseline while
amending [ADR-0036 §1/§1a][source-8], §3 and §4 for the lifecycle/queue
ownership defined below. Its explicit-resume and metadata-only cold-restore
direction is extended only for confirmed Hibernated records with an authorised
new send/focus or an owed committed receipt under Q5/Q6. Saved identity or a
read alone still cannot start a process. Native view integration belongs to
#1430, and home-server routing/TLS to #2293. Neither is claimed implemented
here.

## Code baseline and evidence

Inspected dev `b2519c2c49b08498d75ac427a182ac2298ab49c2`. Paths below describe
that tree, not the independently queued S10 branches. Function names identify
stable inspection points; later slices must recheck them against their base.

- Structured identity persists runtime, agent/name, harness, session id, cwd and
  requested model. Restore is Offline with unknown observations; generation is
  an in-memory fence, not durable process authority. Evidence: [`Record`,
  `Entry::cold`, `Registry::begin_launch`, `Serialize`][source-9]

- Session snapshots include the registry. Snapshot replacement syncs the
  temporary file and renames it; directory-sync failure is logged but not
  returned as a failed save. This is insufficient as a new safety-critical
  lifecycle commit acknowledgement. Evidence: [`capture`][source-10],
  [`save_json_with_directory_sync`][source-11]

- Process outcomes distinguish Exited, Stopped, Crashed, Refused and Failed;
  accepted input without a result is not a successful exit. Workers currently
  publish Offline after confirmed cleanup, Unknown when authority remains
  uncertain. Evidence: [`Outcome`, `Exit`, `collect`][source-12],
  [`Workers::poll`][source-13]

- Launch builds stream-json argv with `--session-id`; retaining a profile does
  not implement the requested `--resume` wake path. Generated settings/MCP files
  have a separate lifetime from that persistent profile. Evidence:
  [`LaunchProfile::prepare`, `ProfileConfigDir`, `SessionConfigDir`][source-14]

- Preparation is not init readiness: registry refusal precedes first stdin, init
  refusal follows it. A pre-input init handshake is not provided by this
  runtime. Evidence: [`run_controlled`, `collect`][source-12]

- Supervision owns process identities, closes stdin, escalates EOF → TERM →
  KILL, reaps descendants and removes transient files; it does not infer a
  hibernation intent from exit status. Evidence: [`supervise`,
  `cleanup_transient`][source-15]

- A sole queue writer holds an OS lease, syncs changes and publishes committed
  cuts. Write uncertainty poisons its handle. The scheduler holds unresolved
  previous submissions rather than replaying them on restart. Evidence:
  [`Journal`][source-16], [`Scheduler::new`, `deliver`, `exited`][source-17]

- Send tickets acknowledge committed admission, not execution. Their keys
  include generation; lost completions can report input_write_unknown. Cold
  queue recovery installs a read source, not a running writable scheduler.
  Evidence: [`Ticket`, `structured_enqueue`, `Sends::poll`][source-18],
  [`Recovery`, `Recovered::install`][source-19]

- Open permission handles and exact-input Always grants live in Book. `end`
  clears grants; orphan recovery writes closes, not resurrected prompts or
  approvals. Request sequence is fenced against the durable stream cursor.
  Evidence: [`Book::blocked`, `end`, `resume_after`, `Orphaned`][source-20],
  [`Recorder::recover`, `finish`][source-21], [`SessionGrant`][source-22]

- Durable model/permission stream records publish after sync; History supports
  replay/live cuts and gaps. EventHub is a separate, rotating audit log; its
  push logs append failure without returning it to the caller. Neither a
  frontend cursor nor an EventHub push is a lifecycle transaction
  acknowledgement. Evidence: [`Transport`, `Recorder`][source-23],
  [`History::subscribe`, `Cut::page`][source-24], [`EventHub::push`,
  `PersistentLog::append`][source-25]

- Existing public state separates turn, background, attention, waiting and
  lifecycle. Lifecycle includes Live/Hibernated/Offline/Gone/Unknown, not the
  nine states requested here. Evidence: [`AgentState`, `Lifecycle`,
  `Turn`][source-26], [`SessionState`][source-27]

- Structured get/list project registry facts; start can reuse a registered
  identity with operator-only resume authority, and stop requests cleanup.
  Existing agent.hibernate/resume handlers address terminal panes. Evidence:
  [`structured_info`, `handle_structured_stop`][source-28],
  [`start_structured_request`][source-29], [`handle_agent_hibernate`,
  `handle_agent_resume`][source-30], [`agent_hibernate`,
  `agent_resume`][source-31]

- MCP exposes narrowed spawn, get/list, stop, messages and events.read, not raw
  agent.start argv. S7 commands route agent.send through shared admission; fleet
  rows carry AgentState. events.read offers bounded waiting. Evidence:
  [`tools`][source-32], [`dispatch_verb`][source-33],
  [`FleetAgentRow`][source-34], [`read_events`][source-35]

## Owner decisions: decided 2026-10-29

The owner typed “accept all rec with star” on 2026-10-29. The linked decision
accepts all recommendations; Q1–Q10 below are decided, not open questions. Each
chosen option and its cost remain explicit. Alternatives are retained as
rejected options, not as implementation choices.

### Q1: state vocabulary — decided 2026-10-29

Chosen (Recommended; accepted 2026-10-29): Add structured-only
`session_lifecycle` with the nine states below; keep ADR-0028 axes and project
legacy lifecycle conservatively. Costs an additive wire/schema/client migration
and two explicitly related vocabularies.

Not selected (2026-10-29): Replace the shared Lifecycle enum. Costs a
cross-harness migration and risks confusing turn observations with process
ownership.

### Q2: automatic policy — decided 2026-10-29

Chosen (Recommended; accepted 2026-10-29): Opt-in idle timeout (Q10 path
`sessions.structured_hibernate_idle_ms`), disabled at 0; use 300000 ms as the
suggested enabled value. Eligibility requires confirmed idle, no background
work, no prompts, no unresolved batch or admission, and no eligible queued item.
Costs less savings for unknown/blocked agents and one resident server timer. The
Q2 upper bound remains pending owner approval on #2717 before coding; the
accepted default 0 and suggested enabled value 300000 ms are unchanged.

Not selected (2026-10-29): Enable fleet-wide by default immediately. Saves
sooner but surprises users with cold starts before cost/latency measurements.

### Q3: exactly once — decided 2026-10-29

Chosen (Recommended; accepted 2026-10-29): Guarantee one durable queue identity
and no automatic replay of a possibly submitted batch. Wake-only races deliver
the waking item once; ambiguous writes become held with a typed reason. Costs
operator recovery in ambiguous crash windows.

Not selected (2026-10-29): Promise exactly-once model effects across arbitrary
crashes. Cannot be established from stdin writes and result events alone;
requires a harness deduplication/transaction contract, so do not advertise it.

### Q4: permission book survival — decided 2026-10-29

Chosen (Recommended; accepted 2026-10-29): Preserve audit history, revocation
history and sequence high-water; retire live-runtime Always grants at
hibernation and show that fact. No open prompt may hibernate. Costs renewed
consent after wake; explicitly narrows “permission book survives” to durable
history, not continuing approval authority.

Not selected (2026-10-29): Preserve Always approvals across processes. Requires
durable exact-input grants, current issuer/revocation/expiry checks and a
separately approved authority model before enabling hibernation; not merely
replaying an old Allow record.

### Q5: wake authority — decided 2026-10-29

Chosen (Recommended; accepted 2026-10-29): An already authorised send wakes only
its target after admission; explicit operator focus may wake with operate
authority. An owed committed AnswerReceipt or LoopWakeReceipt has only the
exact-recipient, original-ItemId wake authority specified below and in ADR-0045
D9. Read/list/subscription, unknown principals and mesh relays without existing
send authority never acquire start rights. Costs an explicit wake capability in
the GUI path and a typed refusal for read-only focus.

Not selected (2026-10-29): Any focus/subscription wakes. Simpler UI, but read
access becomes process creation and a resource-exhaustion capability.

### Q6: restart and failure — decided 2026-10-29

Chosen (Recommended; accepted 2026-10-29): Persist hibernated intent; a restored
record alone never spawns. After ownership recovery, an authorised new
send/focus may wake its confirmed Hibernated target. Replay of an unhandled
committed AnswerReceipt/LoopWakeReceipt may wake only its confirmed Hibernated
recipient for the bound original item. Stop/crash states require explicit
operator start. Resume failure never starts a fresh conversation silently. Costs
visible manual repair for lost sessions or incompatible binaries.

Not selected (2026-10-29): Automatically resume every saved live/intermediate
state. Shorter recovery, but can resurrect stopped work or duplicate uncertain
input.

### Q7: wake latency — decided 2026-10-29

Chosen (Recommended; accepted 2026-10-29): A 5 s target from committed intent to
prepared runtime handle, with a 30 s readiness fail-ceiling that requests
bounded supervisor cancellation. Init progress is measured separately after
first input (Q9). Neither timer proves non-delivery or retries a spawn. Costs
visible slow wakes, possible cancellation of a slow launch, and measurements
before a stronger SLO.

Not selected (2026-10-29): Keep readiness unbounded. Avoids cancelling slow
launches but can occupy a wake slot indefinitely.

### Q8: wake concurrency — decided 2026-10-29

Chosen (Recommended; accepted 2026-10-29): At most one wake per agent and a
configurable per-node ceiling (default 2), still subject to existing spawn
admission. Excess wakes retain their queue item and a typed capacity reason.
Costs queueing delay and new scheduler bookkeeping. The Q8 maximum remains
pending owner approval on #2717 before coding; the accepted default 2 is
unchanged.

Not selected (2026-10-29): Unbounded simultaneous wakes. Lower burst latency but
an idle fleet can exhaust memory/process limits together.

### Q9: resume bootstrap — decided 2026-10-29

Chosen (Recommended; accepted 2026-10-29): Keep the existing pre-input registry
guard; allow only the original first queued item to bootstrap init, then
validate the reported session before further inputs. A focus-only wake is
process-ready with unknown turn until input arrives, or takes the typed
focus_wake_unused exit below when its enabled idle timeout expires with a
pristine-wake proof. Costs additional admission/freeze fencing, possible
first-input uncertainty if init refuses, and resident memory when the policy is
disabled or work prevents the exit.

Not selected (2026-10-29): Require validated init before any user input. Needs a
proven content-free harness handshake; cannot assume it exists or send an
invented prompt.

### Q10: configuration placement and sources — decided 2026-10-29

Chosen (Recommended; accepted 2026-10-29): Move the non-secret policy leaves
outside `structured` to `sessions.structured_hibernate_idle_ms` and
`sessions.structured_wake_concurrency`. Register admitted env mappings under
ADR-0042, retaining all credential/UID keys in the file-only blocklist. Cost:
two new registry/default/validation/reload entries and a dependency on the
config slices; these are accepted schema directions, not available keys today.

Not selected (2026-10-29): Keep `structured.hibernate_idle_ms` and
`structured.wake_concurrency` file-only. No env override is promised, including
after the env-over-file flip. Cost: deployment tooling must write files and
cannot configure this policy through 12-factor env inputs.

Q4's audit/history-only survival policy is chosen: live Always grants end at
hibernation. Q3's stronger model-effect guarantee is rejected. Neither may be
substituted during implementation without a new owner decision.

## Ownership and durable state

Use one lifecycle owner at the agent's owning server. AppState keeps pure data;
workers own timers, disk, leases and children. A view remains attached when the
process hibernates, showing saved transcript plus a sleeping status; there is no
fake PTY and rendering never causes a launch. Explicit focus emits an authorised
wake intent through the same service as send. A browser reconnect is only a
read.

The nine values describe the structured session manager, not unobserved model
activity. Working/Idle edges require existing S4 evidence; attention/background
remain separate and may make the idle process ineligible for hibernation.
Unknown evidence never manufactures Idle. Preserve the existing Unknown
projection for recovery when none of the nine states can be established.

- **Newly registered identity (no previous lifecycle record) → Starting**. First
  durable launch intent and exclusive ownership; no fabricated Stopped edge

- **Crashed/Stopped → Starting**. Explicit operator start, confirmed cleanup and
  exclusive ownership

- **Starting/Waking → Working or Idle when evidenced; otherwise retain
  transitional state with unknown turn**. Validated init and observed turn; init
  alone does not establish Idle

- **Working → Idle**. Observed top-level result and idle reducer boundary

- **Idle → Working; execution remains unconfirmed until harness evidence**.
  Durably reserved queue batch / accepted input

- **Idle → Hibernating**. Timer/manual request passes all eligibility and freeze
  checks

- **Waking (focus-only, process-ready) → Hibernating, reason focus_wake_unused;
  never a fabricated Idle**. Enabled idle timeout expires, or explicit
  authorised hibernate; pristine-wake proof, queue/book freeze and state recheck
  below

- **Hibernating → Hibernated**. EOF exit, I/O drain, durable boundaries and
  complete descendant cleanup proven

- **Hibernated → Waking**. Authorised admitted send/focus, or validated owed
  committed receipt for its original item; one new launch epoch

- **Starting/Working/Idle/Hibernating/Hibernated/Waking/Crashed → Stopping**.
  Authorised explicit stop, persisted first. Crashed with confirmed cleanup is
  included even though its process has ended; uncertain cleanup must still be
  resolved

- **Stopping → Stopped**. Complete process/I/O cleanup or already proven no
  owned process

- **Stopped → Stopped, idempotent acknowledgement without a new transition**.
  Repeated authorised stop

- **Starting/Working/Idle/Hibernating/Waking → Crashed, with typed cause and
  cleanup certainty**. Unexpected exit, launch refusal or failed cleanup

- **Crashed with uncertain cleanup → Remains Crashed, now explicitly startable;
  never automatically Waking**. Operator recovery proves old process absent

A focus-only wake does not become observed Idle by silence. For Q9's special
exit, the owner must prove this launch was focus-only, reached process
readiness, and accepted **zero** user envelopes since spawn. This is an
in-memory proof for the current launch only. There must be no unresolved
admission, queued eligible input, permission/control operation, reported tool or
background work, or unclassified output that could conceal such work. This is a
pristine-input proof for closing stdin, not proof of a model turn result. It
must be obtained under the same generation-scoped transport/book freeze as
ordinary hibernation and rechecked by the queue owner before committing the
exit.

The timeout starts at process readiness and uses Q2's enabled idle duration.
Repeated focus does not extend it. A send, input reservation, prompt or observed
work invalidates the proof; then only the normal evidenced-idle path may
hibernate. Clean EOF plus drain/reap is still required before Hibernated. Do not
recover this in-memory proof after restart: interrupted Waking remains recovery
required. At policy value 0 this automatic exit is intentionally disabled; an
explicit authorised hibernate can use the same proof. Neither path invents an
opening turn or waits forever for a result of input that was never submitted.

Persist `(agent_id, logical_session_id, lifecycle_seq, launch_epoch, state,
intent, reason, stream_through, queue_through)` without credentials, raw tool
input, PIDs as authority, or serialized monotonic clock instants. Placement is
separate real view membership, not ownership. Queue/stream cursors keep their
namespaces; never substitute one cursor for the other.

Storage: lifecycle records in the existing agent queue journal under its sole
writer/lease, with a format-version migration and checkpoint fold. This
serializes lifecycle intent with queue admission; it does not make the harness
stream and queue atomic. Observed stream facts are referenced only after their
fsync; replay deduplicates by source sequence. Persist each lifecycle edge
before publishing it, and checkpoint the last edge so rotation cannot erase it.
Unknown/newer kinds remain preserved raw and inactive per ADR-0028 §3a. Older
writers must refuse mutation of the new format, rather than overwrite it.

Before a new identity launches, its registry metadata must have a durable
acknowledgement, including directory sync; the current log-only sync failure is
not sufficient. A lifecycle journal never resurrects an absent/unknown registry
identity: orphan journals remain inactive for recovery.

The journal owner must be available without a model process: retain a
lightweight writable queue/lifecycle worker for a hibernated agent, lazily
recovered on first mutation after restart. This replaces no active writer with a
second one. Cold read-only history recovery stays read-only. A failed sync is
not an acknowledged transition; expose storage uncertainty, refuse launch, and
reopen/reconcile under the lease. EventHub is a derived notification, never the
commit authority.

Persisted epochs fence publications but do not prove process death. On restart,
reconcile the supervisor's identity evidence and session lease before admitting
any wake. An interrupted Starting/Waking/Hibernating/Stopping intent becomes a
recovery-required Crashed record with last intent retained, not a guessed clean
Hibernate/Stop. A durably completed Hibernated record remains Hibernated only
after absence/lease reconciliation. Save placement through its existing owning
schema once #1430 is integrated; no new layout in the lifecycle journal.

## Quiesce, wake and delivery protocol

1. Arm a supplied-monotonic-clock deadline after evidenced idle, or the separate
   focus-only process-ready deadline with the pristine-wake proof above. Any
   accepted input, prompt, background work, ownership change or stop invalidates
   it. Reload recalculates from the existing idle boundary or focus
   process-readiness boundary, respectively; disabling cancels the automatic
   deadline. Restart earns a new full interval; wall-clock jumps never create
   eligibility.

2. At expiry the queue owner checks there is no reserved/uncertain batch,
   unresolved admission, eligible item, read-only journal, or cleanup
   uncertainty. Ask the runtime for a generation-scoped permission/input freeze
   acknowledgement. Freeze atomically checks the book and transport gate before
   prohibiting new stdin/control work. A reserved decision or newly arrived
   prompt refuses it. Recheck queue/state after the acknowledgement, then commit
   Hibernating.

3. Close stdin through supervision; drain and reap every owned child, including
   MCP descendants. Do not call explicit Stop and relabel Offline after the
   fact. Only a confirmed clean EOF path qualifies. Escalation or unconfirmed
   cleanup records Crashed, retaining recovery evidence. The current
   supervisor's EOF grace is 150 ms, then TERM 250 ms and KILL 2 s ([constants
   and `supervise`][source-15]); these existing stop timings are not a measured
   graceful-hibernation budget. H3 must measure model/MCP EOF-to-exit under
   controlled gates, propose a finite hibernation grace backed by those
   measurements and obtain owner approval on #2717 before coding the hibernation
   implementation. Documenting a number is not approval. Test a slow but
   cooperative MCP child exiting within the approved grace without recording
   Crashed. It must also test a child that never exits and requires escalation.
   This is a lifecycle-specific design/measurement gate, not permission to widen
   existing stop deadlines or classify a forced exit as clean hibernation. The
   profile and journals remain; transient settings/MCP files and their agent
   credentials do not survive.

4. A send arriving before freeze commits cancels the idle attempt. A send after
   freeze commits durably enters the same queue and records wake owed; finish
   cleanup before spawning. Send, focus and receipt triggers coalesce to one
   wake. Stop wins: it cancels pending wake intent, leaves unsubmitted items
   held, and requires an explicit new operator start. Late init or timer
   callbacks cannot undo Stop.

5. A wake reserves current spawn capacity and exclusive session ownership,
   commits Waking with a fresh epoch, regenerates private settings/MCP
   registration and launches `--resume <session>` instead of `--session-id`.
   Keep stable agent id, session/profile, queue item IDs and placement; rotate
   the **agent's** generated settings/MCP credentials, not an attached TUI
   client's capability. Validate the supported binary contract before first
   input. A prepared handle does not prove init readiness: Q9 permits the
   original first item through the existing guarded bootstrap, then checks init
   session identity before any later item. Init refusal after that write is
   uncertain, never “nothing submitted”. A focus-only wake awaits input or its
   typed focus_wake_unused exit without fabricating input or claiming observed
   Idle. Never inject a fresh opening prompt on a resumed session.

6. The existing queue scheduler offers the waking item after process readiness,
   using its original item/batch identity. A result settles it normally. Crash
   before submission permits the still-queued item to run after an explicit
   repair; crash after a possible write holds it, never automatically re-admits
   it. Lost send acknowledgements are resolved by same-key inspection, not new
   IDs.

“Exactly once” here covers an uncontested hibernate → wake → send sequence and
idempotent admission across duplicate wake triggers. It does not assert that a
model/tool side effect happened once after an ambiguous crash. Existing custody
TTL, expiry correction and reply retention remain ADR-0036 §3's rules, with no
new Store::Outcome or extended TTL. Permission history survives; the selected Q4
retires live grants at hibernation. A new process never inherits an open
control-response handle or an MCP process from its predecessor.

### Narrow receipt-bound wake (Q2/Q5/Q6; ADR-0045 D9)

The owner also accepted [ADR-0045 D9][work-adr] and the coordinated [S6e
AnswerReceipt rule][answer-adr] on 2026-10-29. A committed, unhandled
`LoopWakeReceipt` binds the accepted loop, occurrence, authorised recipient
node/agent/generation, original `ItemId` and owner-issued protection grant. An
`AnswerReceipt` likewise binds its committed answer to its exact confirmed
Hibernated asker and original reply item. Neither is a general start, send,
focus, operate or permission-deciding capability.

At admission revalidate custody, current assignment/role and source-read grants,
unchanged session, ownership recovery and confirmed absence/cleanup. Resume only
that confirmed Hibernated recipient for its bound original item. An escalation
hop needs its own receipt and hop-specific ItemId for the attested ancestor
already authorised by the protection grant; it cannot retarget the child's
receipt. Stopped, Crashed, replaced and Unknown recipients keep their explicit
start/recovery requirements and use the owning answer/loop escalation path. User
JSON, read/list/subscribe, an uncommitted event, unregistered predicate or
completed receipt/checkpoint cannot mint a wake.

On restart only an owed, unhandled committed receipt may replay this constrained
wake after ownership recovery. Restoring a Hibernated row never wakes all saved
sessions. Reuse the original ItemId, validate the current binding and coalesce
with other admitted wakes under the same limits. A possible prior stdin write
remains held/uncertain; the receipt does not authorise replaying it.

Outstanding receipt custody, including pending admission and queue capacity
refusal, blocks idle quiesce; an armed loop with no occurrence does not. Before
freeze commits, an admitted answer/loop wake cancels hibernation; after freeze
it records wake owed, waits for confirmed cleanup, then joins the one-per-agent
wake. Rotate only generated agent MCP credentials, use Q9's bootstrap, and
restore no Always grant. Automatic receipt wake is advertised only after the
joint lifecycle, S6e and ADR-0045 receipt/queue slices are implemented and their
public-path tests pass. Scope names alone supply none of this authority.

S10e's attached-TUI token is bound to the **TUI client process/connection**, not
the hibernating agent's process. Agent hibernation alone neither revokes nor
re-mints that token; its normal process-liveness, expiry, revocation, detach and
server-ownership rules still apply. Permission decisions still require a current
request handle from the new agent epoch. See S10e [#2711 at
e8eded86][source-36], specifically [`ClientSession::decide`,
`attach_native_client`, `detach_native_client`][source-37], and the app-session
[process/expiry checks][source-38].

## Surfaces, configuration, costs and limits

Expose one typed session-lifecycle projection and reason through agent.get/list,
CLI JSON, fleet stream rows and GUI/TUI status; new CLI/MCP lifecycle operations
must delegate to the same owner, not infer state from rendered text. Keep the
legacy lifecycle mapping: Starting/Working/Idle map Live only with live
ownership; Waking before ownership and recovery uncertainty map Unknown;
Hibernating stays Live until confirmed exit; Hibernated maps Hibernated;
Stopped/Crashed with absence proven map Offline. Terminal agents retain their
existing contract. ADR-0043's `FleetNodeRow.counts.by_phase` (#2727) uses the
shared `AgentState.phase` computed **after this legacy-lifecycle mapping**,
including ADR-0028's attention/turn precedence. It does not count
`session_lifecycle` names as phase buckets; for example, a pristine focus-only
Waking state never contributes a fabricated Idle. The partition still sums to
the owner's inventory total, including hibernated and offline agents, as
required by #2727.

Extend the owning-node socket operations agent.hibernate/resume for structured
targets with existing authority checks; token frontends need scoped lifecycle
commands and explicit wake-on-focus, not permission to call raw argv start. MCP
gets the narrowed operations only; its message tools reuse custody/send. Expose
the structured projection locally first. Remote gateway delivery and node status
are #2293's work and must carry the same facts, not invent a second home-node
lifecycle. Generated TS types and client compatibility fixtures are part of each
public schema change.

Q10 chose the non-secret `sessions.*` placement on 2026-10-29. The inspected
[`config::env::BLOCKLIST_PREFIXES`][source-39] blocks every `structured.*` leaf,
for explicit UID/credential-file policy. Neither ADR-0041's precedence flip nor
this ADR silently removes that blocklist.

The following registry entries are **not implemented**, coordinated with
ADR-0042 (#2724), not edits to that record or claims of shipped env support. H6
must register them in its single schema registry before release:

- **`sessions.structured_hibernate_idle_ms`**. Non-secret unsigned duration in
  ms; default 0 disables, suggested enabled value 300000; the finite upper bound
  remains pending owner approval on #2717 before coding; global/node scope only,
  no project/agent override in this slice; env
  `FLOCK_SESSIONS_STRUCTURED_HIBERNATE_IDLE_MS`; live reload
  invalidates/recomputes owner deadlines.

- **`sessions.structured_wake_concurrency`**. Non-secret positive integer,
  default 2; the finite maximum remains pending owner approval on #2717 before
  coding; global/node scope only; env
  `FLOCK_SESSIONS_STRUCTURED_WAKE_CONCURRENCY`; live reload gates future starts,
  never kills an already admitted wake.

Both entries declare defaults, file/env admission, provenance, constraints,
redacted read behavior, consumer and reload outcome through ADR-0042's registry.
Flags/runtime layers apply only through that ADR's approved generic surface; no
bespoke env parser or GUI setting is added. The file-only `structured.*`
alternative was not selected. Retain the blocklist on existing credential/UID
keys and diagnose attempts to set them through env. Test source admission as
well as precedence: an ignored env variable is not an effective configuration.
The owner must approve the pending Q2/Q8 ranges and bounds on #2717 before
coding; H6 documents and tests those approved values. Accepted defaults do not
authorise the implementer to choose the open bounds. Configuration cannot relax
lease, permission or unresolved-input gates.

There is no measured savings or latency claim yet. Hibernation removes the model
and MCP processes and their resident memory, but retains a small owner,
queue/history state, durable profiles and transcript bytes. OS caches, disk
usage and provider billing are not eliminated. Wake pays process creation,
version/init validation, history/context loading and MCP initialization;
prompt-cache reuse is not guaranteed. In-flight tools, MCP memory/connections
and volatile harness state cannot be checkpointed by this design; pending work
blocks eligibility instead. Measure resident-child count/RSS, wake durations and
init failures using the fake fixture for regression, not as a real-provider
performance forecast. Any real model cost study is owner-only follow-up tracked
by #2717, using approved test credentials outside CI. No live credentials or
real harness probes in these slices.

## Failure modes and pre-mortem

Imagine the rollout doubled work while reporting agents asleep. The likely
causes and the checks required before enabling it are:

- **Send committed as the timer froze input; trigger lost.** One journal
  serializes admission/intent; durable wake-owed survives cleanup and restart.
  Fake barrier test sends on both sides of freeze.

- **Old process/MCP child survives and new process shares the session.** No
  Hibernated until reap/absence and lease release; uncertain cleanup blocks
  wake. Fake child ignores EOF to exercise cleanup failure.

- **Permission request races freeze or old Allow is reused.** Freeze checks book
  stages atomically; old handles rejected by fresh epoch/sequence. Q4 consent
  policy is visible, not a silent approval restore.

- **Resume session missing, profile unreadable, CLI changed or init id
  mismatches.** Typed resume failure on Crashed, item held/queued according to
  whether input may have reached the harness, no fresh-session fallback.
  Operator repairs or explicitly creates another identity.

- **Disk full, sync fails or torn lifecycle tail.** No successful
  transition/launch acknowledgement; preserve raw data, reopen under lease,
  reconcile intent. Derived EventHub can lag without granting authority.

- **Server dies after EOF but before Hibernated commit.** Intermediate intent is
  recovery-required, not proof of a clean sleep. No automatic launch from
  guessed state.

- **Stop races wake or late init.** Stop intent/epoch fences completion; cleanup
  finishes before Stopped. No later timer, focus completion or send receipt can
  resurrect it.

- **Mass focus/reconnect causes a wake storm.** Only explicit authorised focus
  wakes, per-node capacity and one wake per agent; queued inputs retain
  identity.

- **Background observation is unknown or output never ends.** Ordinary idle
  hibernation is refused; only Q9's separately proven pristine focus-wake exit
  may proceed without an observed idle turn. Surface ineligibility; a
  supplied-clock observation is not proof that tools stopped.

- **Frontend misses events or audit log rotates.** Recover committed lifecycle
  checkpoint plus journal cursor; publish a gap/snapshot, never infer Idle from
  absent events.

- **Focus-only wake never receives input.** The typed focus_wake_unused edge
  uses zero-accepted-input proof and freeze, not silence-as-Idle. An input
  racing its deadline cancels the proof; one admitted item still wakes/delivers
  once. Disabled policy intentionally stays resident.

- **Init waits for input, or preparation never completes.** Q9 permits the real
  first item rather than deadlocking on init. The Q7 readiness fail-ceiling
  cancels a stuck preparation; slot release waits for confirmed cleanup. A
  blocked syscall may delay cleanup and remains recovery-required, never a false
  Hibernated.

- **Device clock changes or server restarts.** Persist intent/reason, not
  monotonic deadlines; start a fresh interval after recovery. No immediate
  hibernate from a wall-time age.

## Ordered implementation slices and acceptance

All accepted slices remain tracked by #2717 until filed as child issues; these
are implementation budgets, not permission to exceed them. Each PR is at most
400 production lines, including wiring and migrations. If its measured scope
exceeds that, split before opening it. Each adds a unique change fragment and
documents its failure/refusal surface. No dormant producer/consumer seams. The
recommended Q1–Q10 options were accepted on 2026-10-29; the unnamed Q2 upper
bound and Q8 maximum remain pending owner approval before coding. H3 must
propose a finite hibernation grace backed by measurements and obtain owner
approval on #2717 before its hibernation implementation. Acceptance supplies no
unmeasured number and does not delegate these gates.

Use only the committed fake in `tests/fixtures/structured-public/claude.py`, an
isolated real flk server and real CLI/MCP/wire clients. Extend its command/ack
protocol with deterministic gates and recorded argv/input IDs. Never seed the
asserted lifecycle or queue directly. Wait on events/readiness with the existing
bounded LIMIT, checking producer liveness and rejecting wrong stages. No sleeps,
retry loops, increased deadlines or real model credentials. For every row retain
one red run before the implementation (or the named mutation), then a green run.
Pure reducer tests supplement these public-path tests; they do not replace them.

### H1 / ≤300 lines

Durable lifecycle fold/versioning, journal-backed projection; existing
start/stop use it.

Fail-first public-path acceptance: CLI start/stop with fake: replay the ordered
Starting → observed Working → Idle → Stopping → Stopped history after isolated
server restart, with same id; incomplete live intent instead becomes
recovery-required. Mutate append-before-publish to publish-only: restart loses
edge and test fails. Unknown-kind journal line is skipped/preserved without
wiping the agent.

### H2 / ≤300 lines

Writable owner without model, same-key offline admission, ownership recovery.
After H1.

Fail-first public-path acceptance: Stop fake, submit via authorised public send;
inspect a durably held queue identity without launching or writing stdin, until
explicit operator start. Repeat same key across server restart; one queue item,
no second writer. Mutate ownership fence to permit a second owner: competing
public request must fail the lease assertion.

### H3 / ≤350 lines

Permission/transport quiesce and explicit structured hibernate, using existing
supervision. After H2.

Fail-first public-path acceptance: Measure EOF-to-exit, propose a finite grace
backed by those measurements and obtain owner approval on #2717 before coding
the hibernation implementation. A fake MCP child gated to exit within that
approved grace produces Hibernated, while one that exceeds it escalates and
reports Crashed. CLI hibernate idle fake: process/MCP absent before sandbox
cleanup, transient files removed, profile/history retained. Fake open
can_use_tool and reserved input each refuse hibernate with no child exit. Inject
prompt at freeze gate: no lost prompt/approval. Delete freeze recheck to produce
red.

### H4 / ≤300 lines

Explicit structured resume launch, validation and epochs. After H3.

Fail-first public-path acceptance: Public resume records exactly `--resume` and
original sid; fake init mismatch/missing-session fails visibly without a fresh
prompt, fallback conversation or second stdin item; a possibly written first
item is held. Late old-epoch output cannot change new state. Fake EOF-resistant
child prevents successful Hibernated/wake. Removing id/cleanup fence must fail.

### H5 / ≤350 lines

Durable wake-owed, coalescing and stop precedence. After H4.

Fail-first public-path acceptance: Hibernate → concurrent CLI send and MCP
message → one new fake process and exactly one recorded stdin envelope per
accepted key; inject send during frozen cleanup too. Stop while init gated:
release gate, no resurrection. Committed AnswerReceipt/LoopWakeReceipt resumes
only the bound confirmed Hibernated recipient/original ItemId once, including
owed receipt replay after restart. Reads, uncommitted events, completed receipts
and stopped/crashed/replaced/uncertain recipients never spawn. Test both sides
of freeze and pending-custody refusal to quiesce; removing receipt
binding/revalidation fails. Kill fake after input before result: same-key
inspection is held, no replay. Remove wake-owed/fence to show red.

### H6 / ≤250 lines

Idle/focus-only timers, Q10 registry/source admission and reload via
ADR-0041/0042. After H5 and config schema/reload prerequisites in #2367/#2724.

Fail-first public-path acceptance: Isolated server with short test policy:
enabled env overrides file through the chosen registered sessions key; the
security blocklist still diagnoses/refuses blocked env. The focus-only wake
sends zero envelopes and uses its current-launch in-memory pristine proof;
restart must not recover that proof or turn interrupted Waking into a clean
hibernate. The input-free wake expires to Hibernating with focus_wake_unused and
then Hibernated; a send gated at the same boundary invalidates the proof and
delivers once. Deleting this exit must fail the no-input test within LIMIT. Fake
result arms idle, hibernates on deadline; busy/prompt/background-unknown
ordinary turns never hibernate. Public reload disable cancels a gated expiry;
invalid value changes nothing and reports rejection. Delete invalidation to
fail. No time is advanced by sleeping.

### H7 / ≤250 lines

Per-node wake admission and bounded progress diagnostics. After H6.

Fail-first public-path acceptance: Gate fake init for two agents at configured
limit 1; public sends retain both keys, only one spawn, second starts after slot
release. Progress-target crossing reports Waking/late rather than a second
process; the readiness fail-ceiling cancels a gated launch and releases its slot
only after cleanup. Remove capacity/coalescing guard to fail counted spawns.

### H8 / ≤300 lines

CLI/MCP narrowed lifecycle verbs and error presentation. After H7.

Fail-first public-path acceptance: Drive same idle/hibernate/resume/stop flow
from both public surfaces; compare state/reason and queue IDs.
Unattested/read-only callers cannot wake or discover target existence through
refusal differences. Remove authority guard to fail, no token contents printed.

### H9 / ≤300 lines

S7 lifecycle projection/commands and GUI focus integration; after H8, #1430 view
contract and #2721 node-count consumer.

Fail-first public-path acceptance: Fake server behind real wire: view survives
hibernation, explicit authorised focus wakes once, reconnect/subscription does
not. An attached TUI capability remains valid across agent hibernation until its
own revocation/expiry; a fresh request handle can be decided, an old one cannot.
FleetNodeRow phase counts agree with mapped AgentState phases and still sum to
total. Revoked/read token focus is refused without spawn. Remove explicit-action
check to fail.

### H10 / ≤250 lines

Combined public-path recovery driver and cost counters; after H9.

Fail-first public-path acceptance: One fake run covers hibernate → send → one
resumed input/result, restart at each gated boundary, permission-policy history,
stop/wake races, cursor replay and child cleanup; assert process counts and
diagnostic durations. Withhold a producer stage while keeping it alive: test
fails within LIMIT, never hangs.

H1–H10 include their tests from their first PR, not postponed to H10. A
sufficiently short test-only policy or supplied-clock owner seam must drive H6's
real public path deterministically; it must not be a production credential or
lifecycle-state injection API. Test design is reviewed before that slice if the
existing fixture controls cannot produce the boundary.

Remaining work has named owners: remote routing/TLS/node aggregation is #2293;
configuration precedence and typed read/write/reload is #2367; native placement
and permission UI is #1430; queue editing, adapter expansion and hot reload
remain #1307's listed slices. Performance validation with a real provider,
implementation of the decided Q4 policy and all H-slice filing remain #2717.
Nothing in this design defers those silently or implements them in this docs PR.

[owner-decision]:
  https://github.com/gerchowl/flock/pull/2728#issuecomment-6485266386
[work-adr]: https://github.com/gerchowl/flock/pull/2732
[answer-adr]: https://github.com/gerchowl/flock/pull/2731
[source-1]: https://github.com/gerchowl/flock/issues/2717
[source-2]: https://github.com/gerchowl/flock/pull/2724
[source-3]: https://github.com/gerchowl/flock/pull/2727
[source-4]: https://github.com/gerchowl/flock/issues/1430
[source-5]: https://github.com/gerchowl/flock/issues/1307
[source-6]: https://github.com/gerchowl/flock/issues/2293
[source-7]: https://github.com/gerchowl/flock/issues/2367
[source-8]: 0036-structured-agent-sessions.md
[source-9]: ../../src/structured/registry/mod.rs
[source-10]: ../../src/persist/snapshot.rs
[source-11]: ../../src/persist/io.rs
[source-12]: ../../src/structured/runtime/mod.rs
[source-13]: ../../src/structured/workers.rs
[source-14]: ../../src/structured/launch.rs
[source-15]: ../../src/structured/supervision/mod.rs
[source-16]: ../../src/structured/queue/mod.rs
[source-17]: ../../src/structured/queue/scheduler.rs
[source-18]: ../../src/app/api/structured_queue.rs
[source-19]: ../../src/structured/workers/queue/recovery.rs
[source-20]: ../../src/structured/permissions.rs
[source-21]: ../../src/structured/transport/control.rs
[source-22]: ../../src/structured/permissions/grants.rs
[source-23]: ../../src/structured/transport/mod.rs
[source-24]: ../../src/structured/history/mod.rs
[source-25]: ../../src/api/event_hub.rs
[source-26]: ../../src/api/agent_state.rs
[source-27]: ../../src/structured/state/mod.rs
[source-28]: ../../src/app/api/structured_agents.rs
[source-29]: ../../src/app/api/structured_start.rs
[source-30]: ../../src/app/api/agents.rs
[source-31]: ../../src/cli/agent.rs
[source-32]: ../../src/mcp/tools.rs
[source-33]: ../../src/web/wire/commands.rs
[source-34]: ../../src/structured/fleet_lite/wire.rs
[source-35]: ../../src/api/events_read.rs
[source-36]: https://github.com/gerchowl/flock/pull/2711
[source-37]:
  https://github.com/gerchowl/flock/blob/e8eded86/src/app/native_auth.rs
[source-38]: ../../src/structured/permissions/auth/tokens/app_sessions.rs
[source-39]: ../../src/config/env.rs
