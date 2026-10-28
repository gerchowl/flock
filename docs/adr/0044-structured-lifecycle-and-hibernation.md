# ADR-0044: Durable structured lifecycle and idle hibernation

- Status: Proposed
- Date: 2026-10-29
- Issues: [#2717](https://github.com/gerchowl/flock/issues/2717)
- Decision owner: repository owner. The questions below are requests for decisions, not approvals.
- Implemented: none by this ADR. This PR changes documentation only.
- Builds on: ADR-0028 (observations), ADR-0036 (structured runtime and custody),
  ADR-0038 (GUI), ADR-0041 (configuration).
- Proposed amendment: ADR-0036 §1/§1a/§3/§4 and its cold-restore rule;
  ADR-0028's observation axes remain separate.

## Why a separate record

[#2717](https://github.com/gerchowl/flock/issues/2717) requests cheap idle
`claude -p` agents with durable lifecycle states and send/focus wakeups. It
references [#1430](https://github.com/gerchowl/flock/issues/1430),
[#1307](https://github.com/gerchowl/flock/issues/1307),
[#2293](https://github.com/gerchowl/flock/issues/2293) and
[#2367](https://github.com/gerchowl/flock/issues/2367); their bodies and comments
inform this proposal. The GUI remains a client of the owning server, not a
second runtime manager. The configuration decision is ADR-0041, including
redaction for every caller and env-over-file precedence, not a new GUI-only setting.

A separate Proposed ADR keeps the accepted ADR-0036 baseline readable while
asking for a material change to restart and permission semantics. On acceptance,
add precise supersession links to ADR-0036; do not silently reinterpret its
current explicit-resume-only rule. Native view integration belongs to #1430,
and home-server routing/TLS to #2293. Neither is claimed implemented here.

## Code baseline and evidence

Inspected dev `b2519c2c49b08498d75ac427a182ac2298ab49c2`. Paths below describe
that tree, not the independently queued S10 branches. Function names identify
stable inspection points; later slices must recheck them against their base.

| Current fact | Code evidence |
| --- | --- |
| Structured identity persists runtime, agent/name, harness, session id, cwd and requested model. Restore is Offline with unknown observations; generation is an in-memory fence, not durable process authority. | [`Record`, `Entry::cold`, `Registry::begin_launch`, `Serialize`](../../src/structured/registry/mod.rs) |
| Session snapshots include the registry. Snapshot replacement syncs the temporary file and renames it; directory-sync failure is logged but not returned as a failed save. This is insufficient as a new safety-critical lifecycle commit acknowledgement. | [`capture`](../../src/persist/snapshot.rs), [`save_json_with_directory_sync`](../../src/persist/io.rs) |
| Process outcomes distinguish Exited, Stopped, Crashed, Refused and Failed; accepted input without a result is not a successful exit. Workers currently publish Offline after confirmed cleanup, Unknown when authority remains uncertain. | [`Outcome`, `Exit`, `collect`](../../src/structured/runtime/mod.rs), [`Workers::poll`](../../src/structured/workers.rs) |
| Launch builds stream-json argv with `--session-id`; retaining a profile does not implement the requested `--resume` wake path. Generated settings/MCP files have a separate lifetime from that persistent profile. | [`LaunchProfile::prepare`, `ProfileConfigDir`, `SessionConfigDir`](../../src/structured/launch.rs) |
| Preparation is not init readiness: registry refusal precedes first stdin, init refusal follows it. A pre-input init handshake is not provided by this runtime. | [`run_controlled`, `collect`](../../src/structured/runtime/mod.rs) |
| Supervision owns process identities, closes stdin, escalates EOF → TERM → KILL, reaps descendants and removes transient files; it does not infer a hibernation intent from exit status. | [`supervise`, `cleanup_transient`](../../src/structured/supervision/mod.rs) |
| A sole queue writer holds an OS lease, syncs changes and publishes committed cuts. Write uncertainty poisons its handle. The scheduler holds unresolved previous submissions rather than replaying them on restart. | [`Journal`](../../src/structured/queue/mod.rs), [`Scheduler::new`, `deliver`, `exited`](../../src/structured/queue/scheduler.rs) |
| Send tickets acknowledge committed admission, not execution. Their keys include generation; lost completions can report input_write_unknown. Cold queue recovery installs a read source, not a running writable scheduler. | [`Ticket`, `structured_enqueue`, `Sends::poll`](../../src/app/api/structured_queue.rs), [`Recovery`, `Recovered::install`](../../src/structured/workers/queue/recovery.rs) |
| Open permission handles and exact-input Always grants live in Book. `end` clears grants; orphan recovery writes closes, not resurrected prompts or approvals. Request sequence is fenced against the durable stream cursor. | [`Book::blocked`, `end`, `resume_after`, `Orphaned`](../../src/structured/permissions.rs), [`Recorder::recover`, `finish`](../../src/structured/transport/control.rs), [`SessionGrant`](../../src/structured/permissions/grants.rs) |
| Durable model/permission stream records publish after sync; History supports replay/live cuts and gaps. EventHub is a separate, rotating audit log; its push logs append failure without returning it to the caller. Neither a frontend cursor nor an EventHub push is a lifecycle transaction acknowledgement. | [`Transport`, `Recorder`](../../src/structured/transport/mod.rs), [`History::subscribe`, `Cut::page`](../../src/structured/history/mod.rs), [`EventHub::push`, `PersistentLog::append`](../../src/api/event_hub.rs) |
| Existing public state separates turn, background, attention, waiting and lifecycle. Lifecycle includes Live/Hibernated/Offline/Gone/Unknown, not the nine states requested here. | [`AgentState`, `Lifecycle`, `Turn`](../../src/api/agent_state.rs), [`SessionState`](../../src/structured/state/mod.rs) |
| Structured get/list project registry facts; start can reuse a registered identity with operator-only resume authority, and stop requests cleanup. Existing agent.hibernate/resume handlers address terminal panes. | [`structured_info`, `handle_structured_stop`](../../src/app/api/structured_agents.rs), [`start_structured_request`](../../src/app/api/structured_start.rs), [`handle_agent_hibernate`, `handle_agent_resume`](../../src/app/api/agents.rs), [`agent_hibernate`, `agent_resume`](../../src/cli/agent.rs) |
| MCP exposes narrowed spawn, get/list, stop, messages and events.read, not raw agent.start argv. S7 commands route agent.send through shared admission; fleet rows carry AgentState. events.read offers bounded waiting. | [`tools`](../../src/mcp/tools.rs), [`dispatch_verb`](../../src/web/wire/commands.rs), [`FleetAgentRow`](../../src/structured/fleet_lite/wire.rs), [`read_events`](../../src/api/events_read.rs) |

## Owner decisions requested

The owner has requested the goal and the nine names, not these policies.
Implementation waits for answers Q1–Q9 on #2717; this docs PR may remain Proposed.

| Decision | (Recommended) option and cost | Alternative and cost |
| --- | --- | --- |
| Q1: state vocabulary | Add structured-only `session_lifecycle` with the nine states below; keep ADR-0028 axes and project legacy lifecycle conservatively. Costs an additive wire/schema/client migration and two explicitly related vocabularies. | Replace the shared Lifecycle enum. Costs a cross-harness migration and risks confusing turn observations with process ownership. |
| Q2: automatic policy | Opt-in `structured.hibernate_idle_ms`, disabled at 0; suggest 300000 ms when enabled. Eligibility requires confirmed idle, no background work, no prompts, no unresolved batch or admission, and no eligible queued item. Costs less savings for unknown/blocked agents and one resident server timer. | Enable fleet-wide by default immediately. Saves sooner but surprises users with cold starts before cost/latency measurements. |
| Q3: exactly once | Guarantee one durable queue identity and no automatic replay of a possibly submitted batch. Wake-only races deliver the waking item once; ambiguous writes become held with a typed reason. Costs operator recovery in ambiguous crash windows. | Promise exactly-once model effects across arbitrary crashes. Cannot be established from stdin writes and result events alone; requires a harness deduplication/transaction contract, so do not advertise it. |
| Q4: permission book survival | Preserve audit history, revocation history and sequence high-water; retire live-runtime Always grants at hibernation and show that fact. No open prompt may hibernate. Costs renewed consent after wake; explicitly narrows “permission book survives” to durable history, not continuing approval authority. | Preserve Always approvals across processes. Requires durable exact-input grants, current issuer/revocation/expiry checks and a separately approved authority model before enabling hibernation; not merely replaying an old Allow record. |
| Q5: wake authority | An already authorised send wakes only its target after admission; explicit operator focus may wake with operate authority. Read/list/subscription, unknown principals and mesh relays without existing send authority never acquire start rights. Costs an explicit wake capability in the GUI path and a typed refusal for read-only focus. | Any focus/subscription wakes. Simpler UI, but read access becomes process creation and a resource-exhaustion capability. |
| Q6: restart and failure | Persist hibernated intent, but cold startup never spawns automatically. An authorised new send/focus can wake a confirmed Hibernated record after ownership recovery; stop/crash states require explicit operator start. Resume failure never starts a fresh conversation silently. Costs visible manual repair for lost sessions or incompatible binaries. | Automatically resume every saved live/intermediate state. Shorter recovery, but can resurrect stopped work or duplicate uncertain input. |
| Q7: wake latency | A 5 s target from committed intent to prepared runtime handle, with a proposed 30 s readiness fail-ceiling that requests bounded supervisor cancellation. Init progress is measured separately after first input (Q9). Neither timer proves non-delivery or retries a spawn. Costs visible slow wakes, possible cancellation of a slow launch, and measurements before a stronger SLO. | Keep readiness unbounded. Avoids cancelling slow launches but can occupy a wake slot indefinitely. |
| Q8: wake concurrency | At most one wake per agent and a configurable per-node ceiling (proposed 2), still subject to existing spawn admission. Excess wakes retain their queue item and a typed capacity reason. Costs queueing delay and new scheduler bookkeeping. | Unbounded simultaneous wakes. Lower burst latency but an idle fleet can exhaust memory/process limits together. |
| Q9: resume bootstrap | Keep the existing pre-input registry guard; allow only the original first queued item to bootstrap init, then validate the reported session before further inputs. A focus-only wake is process-ready with unknown turn until input arrives. Costs possible first-input uncertainty if init refuses, and resident memory after focus without a turn. | Require validated init before any user input. Needs a proven content-free harness handshake; cannot assume it exists or send an invented prompt. |

If Q4's alternative is chosen, reslice its permission-authority prerequisite
under #2717 before implementation; do not quietly ship the recommended narrowing.
Likewise Q3's stronger interpretation is an owner design gate, not an omitted test.

## Proposed ownership and durable state

Use one lifecycle owner at the agent's owning server. AppState keeps pure data;
workers own timers, disk, leases and children. A view remains attached when the
process hibernates, showing saved transcript plus a sleeping status; there is no
fake PTY and rendering never causes a launch. Explicit focus emits an authorised
wake intent through the same service as send. A browser reconnect is only a read.

The nine values describe the structured session manager, not unobserved model
activity. Working/Idle edges require existing S4 evidence; attention/background
remain separate and may make the idle process ineligible for hibernation.
Unknown evidence never manufactures Idle. Preserve the existing Unknown
projection for recovery when none of the nine states can be established.

| From | Required fact / operation | To |
| --- | --- | --- |
| Stopped (new identity) or Crashed/Stopped (explicit operator start) | Durable launch intent and exclusive ownership | Starting |
| Starting/Waking | Validated init and observed turn; init alone does not establish Idle | Working or Idle when evidenced; otherwise retain transitional state with unknown turn |
| Working | Observed top-level result and idle reducer boundary | Idle |
| Idle | Durably reserved queue batch / accepted input | Working; execution remains unconfirmed until harness evidence |
| Idle | Timer/manual request passes all eligibility and freeze checks | Hibernating |
| Hibernating | EOF exit, I/O drain, durable boundaries and complete descendant cleanup proven | Hibernated |
| Hibernated | Authorised, admitted wake trigger; one new launch epoch | Waking |
| Any nonterminal state | Authorised explicit stop, persisted first | Stopping |
| Stopping | Complete process/I/O cleanup or already no owned process | Stopped |
| Starting/Working/Idle/Hibernating/Waking | Unexpected exit, launch refusal or failed cleanup | Crashed, with typed cause and cleanup certainty |
| Crashed with uncertain cleanup | Operator recovery proves old process absent | Remains Crashed, now explicitly startable; never automatically Waking |

Persist `(agent_id, logical_session_id, lifecycle_seq, launch_epoch, state,
intent, reason, stream_through, queue_through)` without credentials, raw tool
input, PIDs as authority, or serialized monotonic clock instants. Placement is
separate real view membership, not ownership. Queue/stream cursors keep their
namespaces; never substitute one cursor for the other.

Proposed storage: lifecycle records in the existing agent queue journal under
its sole writer/lease, with a format-version migration and checkpoint fold.
This serializes lifecycle intent with queue admission; it does not make the
harness stream and queue atomic. Observed stream facts are referenced only after
their fsync; replay deduplicates by source sequence. Persist each lifecycle edge
before publishing it, and checkpoint the last edge so rotation cannot erase it.
Unknown/newer kinds remain preserved raw and inactive per ADR-0028 §3a. Older
writers must refuse mutation of the new format, rather than overwrite it.

Before a new identity launches, its registry metadata must have a durable
acknowledgement, including directory sync; the current log-only sync failure is
not sufficient. A lifecycle journal never resurrects an absent/unknown registry
identity: orphan journals remain inactive for recovery.

The journal owner must be available without a model process: retain a lightweight
writable queue/lifecycle worker for a hibernated agent, lazily recovered on first
mutation after restart. This replaces no active writer with a second one. Cold
read-only history recovery stays read-only. A failed sync is not an acknowledged
transition; expose storage uncertainty, refuse launch, and reopen/reconcile under
the lease. EventHub is a derived notification, never the commit authority.

Persisted epochs fence publications but do not prove process death. On restart,
reconcile the supervisor's identity evidence and session lease before admitting
any wake. An interrupted Starting/Waking/Hibernating/Stopping intent becomes a
recovery-required Crashed record with last intent retained, not a guessed clean
Hibernate/Stop. A durably completed Hibernated record remains Hibernated only
after absence/lease reconciliation. Save placement through its existing owning
schema once #1430 is integrated; no new layout in the lifecycle journal.

## Quiesce, wake and delivery protocol

1. Arm a supplied-monotonic-clock deadline after evidenced idle. Any accepted
   input, prompt, background work, ownership change or stop invalidates it.
   Reload recalculates from the existing idle boundary; disabling cancels it.
   Restart earns a new full interval; wall-clock jumps never create eligibility.
2. At expiry the queue owner checks there is no reserved/uncertain batch,
   unresolved admission, eligible item, read-only journal, or cleanup uncertainty.
   Ask the runtime for a generation-scoped permission/input freeze acknowledgement.
   Freeze atomically checks the book and transport gate before prohibiting new
   stdin/control work. A reserved decision or newly arrived prompt refuses it.
   Recheck queue/state after the acknowledgement, then commit Hibernating.
3. Close stdin through supervision; drain and reap every owned child, including
   MCP descendants. Do not call explicit Stop and relabel Offline after the fact.
   Only a confirmed clean EOF path qualifies. Escalation or unconfirmed cleanup
   records Crashed, retaining recovery evidence. The profile and journals remain;
   transient settings/MCP files and their credentials do not survive.
4. A send arriving before freeze commits cancels the idle attempt. A send after
   freeze commits durably enters the same queue and records wake owed; finish
   cleanup before spawning. Two sends plus focus coalesce to one wake. Stop wins:
   it cancels pending wake intent, leaves unsubmitted items held, and requires an
   explicit new operator start. Late init or timer callbacks cannot undo Stop.
5. A wake reserves current spawn capacity and exclusive session ownership, commits
   Waking with a fresh epoch, regenerates private settings/MCP registration and
   launches `--resume <session>` instead of `--session-id`. Keep stable agent id,
   session/profile, queue item IDs and placement; rotate process-bound credentials.
   Validate the supported binary contract before first input. A prepared handle
   does not prove init readiness: Q9 permits the original first item through the
   existing guarded bootstrap, then checks init session identity before any later
   item. Init refusal after that write is uncertain, never “nothing submitted”.
   A focus-only wake waits without fabricating input or claiming observed Idle.
   Never inject a fresh opening prompt on a resumed session.
6. The existing queue scheduler offers the waking item after process readiness, using
   its original item/batch identity. A result settles it normally. Crash before
   submission permits the still-queued item to run after an explicit repair;
   crash after a possible write holds it, never automatically re-admits it.
   Lost send acknowledgements are resolved by same-key inspection, not new IDs.

“Exactly once” here covers an uncontested hibernate → wake → send sequence and
idempotent admission across duplicate wake triggers. It does not assert that a
model/tool side effect happened once after an ambiguous crash. Existing custody
TTL, expiry correction and reply retention remain ADR-0036 §3's rules, with no
new Store::Outcome or extended TTL. Permission history survives; the selected
Q4 policy determines live grant lifetime. A new process never inherits an open
control-response handle or an MCP process from its predecessor.

## Surfaces, configuration, costs and limits

Expose one typed session-lifecycle projection and reason through agent.get/list,
CLI JSON, fleet stream rows and GUI/TUI status; new CLI/MCP lifecycle operations
must delegate to the same owner, not infer state from rendered text. Keep the
legacy lifecycle mapping: Starting/Working/Idle map Live only with live ownership;
Waking before ownership and recovery uncertainty map Unknown; Hibernating stays
Live until confirmed exit; Hibernated maps Hibernated; Stopped/Crashed with absence
proven map Offline. Terminal agents retain their existing contract.

Extend the owning-node socket operations agent.hibernate/resume for structured
targets with existing authority checks; token frontends need scoped lifecycle
commands and explicit wake-on-focus, not permission to call raw argv start.
MCP gets the narrowed operations only; its message tools reuse custody/send.
Expose the structured projection locally first. Remote gateway delivery and
node status are #2293's work and must carry the same facts, not invent a second
home-node lifecycle. Generated TS types and client compatibility fixtures are
part of each public schema change.

Configuration keys above are proposed schema leaves, validated durations/limits
with provenance and per-section reload outcomes under ADR-0041/#2367. No secrets
in these settings. The owner must approve actual default/range values before
coding. Configuration cannot relax lease, permission or unresolved-input gates.

There is no measured savings or latency claim yet. Hibernation removes the
model and MCP processes and their resident memory, but retains a small owner,
queue/history state, durable profiles and transcript bytes. OS caches, disk usage
and provider billing are not eliminated. Wake pays process creation, version/init
validation, history/context loading and MCP initialization; prompt-cache reuse is
not guaranteed. In-flight tools, MCP memory/connections and volatile harness state
cannot be checkpointed by this design; pending work blocks eligibility instead.
Measure resident-child count/RSS, wake durations and init failures using the fake
fixture for regression, not as a real-provider performance forecast. Any real
model cost study is owner-only follow-up tracked by #2717, using approved test
credentials outside CI. No live credentials or real harness probes in these slices.

## Failure modes and pre-mortem

Imagine the rollout doubled work while reporting agents asleep. The likely
causes and the checks required before enabling it are:

| Failure / pre-mortem hypothesis | Observable handling and prevention |
| --- | --- |
| Send committed as the timer froze input; trigger lost | One journal serializes admission/intent; durable wake-owed survives cleanup and restart. Fake barrier test sends on both sides of freeze. |
| Old process/MCP child survives and new process shares the session | No Hibernated until reap/absence and lease release; uncertain cleanup blocks wake. Fake child ignores EOF to exercise cleanup failure. |
| Permission request races freeze or old Allow is reused | Freeze checks book stages atomically; old handles rejected by fresh epoch/sequence. Q4 consent policy is visible, not a silent approval restore. |
| Resume session missing, profile unreadable, CLI changed or init id mismatches | Typed resume failure on Crashed, item held/queued according to whether input may have reached the harness, no fresh-session fallback. Operator repairs or explicitly creates another identity. |
| Disk full, sync fails or torn lifecycle tail | No successful transition/launch acknowledgement; preserve raw data, reopen under lease, reconcile intent. Derived EventHub can lag without granting authority. |
| Server dies after EOF but before Hibernated commit | Intermediate intent is recovery-required, not proof of a clean sleep. No automatic launch from guessed state. |
| Stop races wake or late init | Stop intent/epoch fences completion; cleanup finishes before Stopped. No later timer, focus completion or send receipt can resurrect it. |
| Mass focus/reconnect causes a wake storm | Only explicit authorised focus wakes, per-node capacity and one wake per agent; queued inputs retain identity. |
| Background observation is unknown or output never ends | Do not hibernate. Surface ineligibility; a supplied-clock observation is not proof that tools stopped. |
| Frontend misses events or audit log rotates | Recover committed lifecycle checkpoint plus journal cursor; publish a gap/snapshot, never infer Idle from absent events. |
| Init waits for input, or preparation never completes | Q9 permits the real first item rather than deadlocking on init. The Q7 readiness fail-ceiling cancels a stuck preparation; slot release waits for confirmed cleanup. A blocked syscall may delay cleanup and remains recovery-required, never a false Hibernated. |
| Device clock changes or server restarts | Persist intent/reason, not monotonic deadlines; start a fresh interval after recovery. No immediate hibernate from a wall-time age. |

## Ordered implementation slices and acceptance

All slices remain tracked by #2717 until accepted and filed as child issues;
these are implementation budgets, not permission to exceed them. Each PR is at
most 400 production lines, including wiring and migrations. If its measured
scope exceeds that, split before opening it. Each adds a unique change fragment
and documents its failure/refusal surface. No dormant producer/consumer seams.
No implementation begins until Q1–Q9 are settled.

Use only the committed fake in `tests/fixtures/structured-public/claude.py`, an
isolated real flk server and real CLI/MCP/wire clients. Extend its command/ack
protocol with deterministic gates and recorded argv/input IDs. Never seed the
asserted lifecycle or queue directly. Wait on events/readiness with the existing
bounded LIMIT, checking producer liveness and rejecting wrong stages. No sleeps,
retry loops, increased deadlines or real model credentials. For every row retain
one red run before the implementation (or the named mutation), then a green run.
Pure reducer tests supplement these public-path tests; they do not replace them.

| Order / budget | Deliverable and dependencies | Fail-first public-path acceptance |
| --- | --- | --- |
| H1 / ≤300 lines | Durable lifecycle fold/versioning, journal-backed projection; existing start/stop use it. | CLI start/stop with fake: replay the ordered Starting → observed Working → Idle → Stopping → Stopped history after isolated server restart, with same id; incomplete live intent instead becomes recovery-required. Mutate append-before-publish to publish-only: restart loses edge and test fails. Unknown-kind journal line is skipped/preserved without wiping the agent. |
| H2 / ≤300 lines | Writable owner without model, same-key offline admission, ownership recovery. After H1. | Stop fake, submit via authorised public send; inspect a durably held queue identity without launching or writing stdin, until explicit operator start. Repeat same key across server restart; one queue item, no second writer. Mutate ownership fence to permit a second owner: competing public request must fail the lease assertion. |
| H3 / ≤350 lines | Permission/transport quiesce and explicit structured hibernate, using existing supervision. After H2. | CLI hibernate idle fake: process/MCP absent before sandbox cleanup, transient files removed, profile/history retained. Fake open can_use_tool and reserved input each refuse hibernate with no child exit. Inject prompt at freeze gate: no lost prompt/approval. Delete freeze recheck to produce red. |
| H4 / ≤300 lines | Explicit structured resume launch, validation and epochs. After H3. | Public resume records exactly `--resume` and original sid; fake init mismatch/missing-session fails visibly without a fresh prompt, fallback conversation or second stdin item; a possibly written first item is held. Late old-epoch output cannot change new state. Fake EOF-resistant child prevents successful Hibernated/wake. Removing id/cleanup fence must fail. |
| H5 / ≤350 lines | Durable wake-owed, coalescing and stop precedence. After H4. | Hibernate → concurrent CLI send and MCP message → one new fake process and exactly one recorded stdin envelope per accepted key; inject send during frozen cleanup too. Stop while init gated: release gate, no resurrection. Kill fake after input before result: same-key inspection is held, no replay. Remove wake-owed/fence to show red. |
| H6 / ≤250 lines | Idle configuration, timer and reload via ADR-0041 surface. After H5 and config schema/reload prerequisites in #2367. | Isolated server with short test policy: fake result arms idle, hibernates on deadline; busy/prompt/background-unknown never hibernate. Public reload disable cancels a gated expiry; invalid value changes nothing and reports rejection. Delete invalidation to fail. No time is advanced by sleeping. |
| H7 / ≤250 lines | Per-node wake admission and bounded progress diagnostics. After H6. | Gate fake init for two agents at configured limit 1; public sends retain both keys, only one spawn, second starts after slot release. Progress-target crossing reports Waking/late rather than a second process; the readiness fail-ceiling cancels a gated launch and releases its slot only after cleanup. Remove capacity/coalescing guard to fail counted spawns. |
| H8 / ≤300 lines | CLI/MCP narrowed lifecycle verbs and error presentation. After H7. | Drive same idle/hibernate/resume/stop flow from both public surfaces; compare state/reason and queue IDs. Unattested/read-only callers cannot wake or discover target existence through refusal differences. Remove authority guard to fail, no token contents printed. |
| H9 / ≤300 lines | S7 lifecycle projection/commands and GUI focus integration; after H8 and #1430 view contract. | Fake server behind real wire: view survives hibernation, explicit authorised focus wakes once, reconnect/subscription does not. Revoked/read token focus is refused without spawn. Remove explicit-action check to fail. |
| H10 / ≤250 lines | Combined public-path recovery driver and cost counters; after H9. | One fake run covers hibernate → send → one resumed input/result, restart at each gated boundary, permission-policy history, stop/wake races, cursor replay and child cleanup; assert process counts and diagnostic durations. Withhold a producer stage while keeping it alive: test fails within LIMIT, never hangs. |

H1–H10 include their tests from their first PR, not postponed to H10. A
sufficiently short test-only policy or supplied-clock owner seam must drive H6's
real public path deterministically; it must not be a production credential or
lifecycle-state injection API. Test design is reviewed before that slice if the
existing fixture controls cannot produce the boundary.

Remaining work has named owners: remote routing/TLS/node aggregation is #2293;
configuration precedence and typed read/write/reload is #2367; native placement
and permission UI is #1430; queue editing, adapter expansion and hot reload remain
#1307's listed slices. Performance validation with a real provider, the pending
Q4 grant decision and all H-slice filing remain #2717. Nothing in this proposal
defers those silently or implements them in this docs PR.
