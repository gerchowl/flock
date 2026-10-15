# ADR 0028 — An agent's state has one cross-harness vocabulary, supervisors read it as a resumable event stream, and waiting is a registered wait that the server wakes, never a poll

- Status: Accepted
- Date: 2026-10-11 (accepted 2026-10-11)
- Scope of acceptance: the decisions in this record, as the operator answered
  them on 2026-10-11 (PR #1006, "Owner decisions"). Acceptance claims no code.
- Issues: #1001 (the spike this records, area 1 "push events, stop polling").
  Amends the slice plans of #663, #697, #961 and #922's follow-ups. Composes
  with ADR-0027 (delegate tracking), ADR-0029 (agent-facing CLI contract),
  ADR-0030 (goal supervision) and #1005 (external channels, §8).
- Builds on: ADR-0005 (durable event log), ADR-0008 / ADR-0018 / ADR-0019 (mail,
  wake, channel push), ADR-0026 (mesh custody), #850 (injection lock).
- Decision owner: operator.
- Amendment (2026-10-15, #1479): §1a records accepted Codex lifecycle authority;
  its source/continuity contract is proposed for review before implementation;
  format/version policy awaits an owner decision (§1a).
- Implemented: none. Slices (§10) filed 2026-10-11: E1a #1019, E7 #1020, E1b #1022, E1c #1023, E2 #697, E3 #1027, E4a #1028, E4b #1029, E4c #1030, E5 #1031, E6 #1032, E8 #1037, E9a #1038, E9b #1039, E9c #1040. Order: ADR-0030 §8 and #1001.

## Context

### What the fleet does today

The October 2026 toolchain survey (#987; 21 days, four hosts, 9,494 agent
turns, 938 h of turn time; aggregated counts only) found that agents watch each
other by polling and scraping:

| Observation | Count |
| --- | --- |
| Observation calls (`pane read` 3,151, `agent get` 983, `agent list` 955, `agent read` 823) | ~5,900 |
| Blocking waits (`flk wait`, `agent wait`, `delegate wait`) | 343 |
| Explicit `sleep` inside shell commands | 57 h |
| Time spent inside `flk agent get` (mostly `until … = idle` loops) | 12 h |
| `sleep N; <check>` commands refused by the harness itself | 449 |
| watch-nudge hook firings for poll loops or long sleeps | 412 |
| goal-s1 stops allowed as "waiting on background tasks" | 1,294 of 2,007 |
| Median such wait | 2.4 h to 19 h depending on host |
| goal-s1 `bg-checkin` pushes produced by those waits | 262 |

The owner's most frequent complaint is an idle or stuck agent that nobody
noticed. The flock fleet's own architect directory carries `supervisor.py`
(a 90 s poll loop over `flk agent get`, `gh pr view` and screen scraping that
types `[fleet] …` lines into the architect's pane), `landbatches.sh`
(`sleep 60` loops over `gh pr checks`), and `spawn-claude.sh` (a dialog
scraper plus `pane run`). Those scripts are what this ADR must make
unnecessary.

### What flk offers today (verified at `ab54ba61`)

- **Status.** `AgentStatus` has seven values (`idle`, `working`, `blocked`,
  `done`, `unknown`, `hibernated`, `offline`). `done` is "idle and not yet seen
  by the operator", a presentation flag. Claude, Codex and opencode status
  comes from per-harness screen detectors; hooks report state only for kimi and
  qodercli. `settled_with_background_shells` exists for Claude only (#911,
  #922). Nothing separates turn state from background activity (#697), and
  `needs_human` exists only in ADR-0027's text. Provider-limit detection exists
  for opencode only.
- **Events.** Every `EventKind` except three is persisted to
  `event-log.jsonl` with a monotonic `seq` (ADR-0005). Rotation keeps four
  files at 32 MiB / 100,000 events each. `events.subscribe` streams NDJSON
  from a 4,096-entry in-memory ring. The guarantee is **at-most-once with
  silent loss**: no resume cursor, no read from disk, no lag notice, and plain
  kind subscriptions start at sequence 0 and replay the ring, which ADR-0005
  calls a non-goal. `EventsWait` exists in the schema with no handler. There is
  no `flk events` CLI.
- **Waits.** `flk wait reply` and MCP `flock_msg_wait_reply` are server-side
  long-polls on the hub condvar. `flk wait agent-status` subscribes. `flk
  agent wait --status settled` and `flk delegate wait` poll `agent.get` every
  200 ms from the client.
- **Wake.** An agent learns of mail by (1) the Stop hook's `decision: block`
  nudge at a turn boundary, (2) the typed idle-wake sentence through the
  guarded injection (#850 lease, draft stash), Claude only, or (3) an opt-in
  channel push (ADR-0019, default off). Per ADR-0018, **an `fyi` message never
  idle-wakes**; only `needs_reply`, `blocking` and answers to a question do.

That last fact matters for ADR-0027 §3: its digest is specified as intent
`fyi`, so as written it would never wake a parked architect even after #697.
§5 resolves that.

### Measurements taken for this ADR

All in an isolated flk (sandbox `XDG_CONFIG_HOME`, copied 1.1.0 binary, a real
throwaway Claude Code session on a small model) or read-only from one host's
event log. The runbook that produced them is `tests/probes/agent_wake.py`
(subcommands `idle-wake`, `busy`, `background`, `event-volume`,
`judge-latency`). The samples are small (5 idle wakes, 3 busy runs, one
background run, one host's log), so read them as orders of magnitude, not
tails.

**Event volume** (one host's durable log, last 7 days, 168 h, a median of 5
and a peak of 22 agent panes active per hour):

| Stream | Per hour |
| --- | --- |
| All persisted events | 169 |
| `pane_agent_status_changed`, raw | 74.1 |
| … that survive a 5 s settle filter | 38.7 |
| … 20 s settle filter | 34.0 |
| … 60 s settle filter | 25.2 |
| Settled (20 s) transitions into or out of `idle` / `done` / `blocked` (the digest triggers of ADR-0027 §3) | 12.0 |
| `checks_heartbeat` | 58.7 |
| `notification_filed` | 13.9 |

Status flapping is a detector property, not noise at the edges: 53 % of Codex
transitions and 40 % of Claude transitions last under 5 s. Codex produced 8,578
of the 12,453 raw transitions. The densest second held 217 events, all
`notification_seen` (a bulk mark-read), which is why per-kind filtering must
happen in the server, not in the consumer.

**Wake latency to an idle Claude** (sandbox, `msg send --intent needs-reply`
to an agent idle for ≥ 20 s, 100 ms sampling): send → turn started in 381,
265, 498, 265 and 261 ms (median 265 ms). That includes the 120 ms
type-then-Enter gap.

**Mail to a busy Claude** (message sent 3 s into a 12 s foreground command):
read 12.8 s, 13.2 s and 12.9 s after send, i.e. at the turn boundary. flk
never observed the agent idle in between: the Stop hook's block keeps the turn
going straight into `flock_msg_read`.

**`fyi` to an idle Claude:** no wake in 60 s, as ADR-0018 specifies.

**Background shell** (Claude asked to start a blocking `cat <fifo>` with
`run_in_background` and end its turn):

- flk read the pane `working` and then `idle` 5.4 s after the turn ended, with
  the background task still alive. The #697 symptom (stuck at `working` for 12+
  minutes) did **not** reproduce on this harness version, so it depends on what
  the harness draws, which is exactly why the turn/background split must be an
  explicit, tested axis rather than a detector accident.
- A `needs_reply` message sent while the shell ran woke the agent in 0.5 s and
  was read 1.4 s after send.
- When the background task finished, the harness itself started a new turn
  352 ms later: harness-native background waiting is a working wake path, with
  no flk involvement.
- The same run shows the cost of the recap sentinel: the Stop hook blocked once
  for `※ recap:` and the agent spent one extra turn writing it (the survey
  counts 520 such blocks).

**Submit reliability, incidentally:** one guarded `agent send --submit` of a
three-row prompt left the text typed in the composer and not submitted. The
return value of that call was lost to output buffering, so whether it reported
`unconfirmed` is unknown; ADR-0029 makes that outcome impossible to miss.

## Decision

### 1. One state, decided once in the server, with orthogonal axes

The server publishes, on `agent.get`, `agent.list`, `delegate.list` and every
agent event, a `state` object with four independent axes. The existing
`agent_status` field stays for compatibility and presentation; supervision
reads `state`.

```text
state {
  turn:       working | idle | unknown         // is the harness mid-turn?
  background: none | running { count, oldest_age_s, source } | unknown
  attention:  none
            | needs_human { reason: question | permission | dialog | trust | login
                                    | unknown }
            | blocked     { reason: provider_limit | auth | resume_failed | crashed,
                            retry_after_ms? }
  waiting:    none | on { wait_id, predicate, since }    // §5, registered waits
  lifecycle:  live | hibernated | offline | gone
  phase:      <derived, below>
  held_ms:    how long `phase` has held
}
```

`phase` is the one word a supervisor branches on. It is derived, in this order,
and the order is part of the contract:

| `phase` | Definition |
| --- | --- |
| `gone` | lifecycle `gone` |
| `hibernated` / `offline` | the lifecycle value |
| `limit_reached` | attention `blocked{provider_limit}` (carries `retry_after_ms` when the harness shows one), whatever `turn` and `background` say |
| `blocked` | any other attention `blocked`, whatever `turn` and `background` say |
| `needs_human` | attention `needs_human`, whatever `turn` and `background` say |
| `working` | turn `working` |
| `waiting_on_event` | turn `idle`, held ≥ settle, and a registered wait is open |
| `finished_with_background` | turn `idle`, held ≥ settle, background `running`, no wait registered |
| `finished` | turn `idle`, held ≥ settle, background `none` |
| `settling` | turn `idle`, held < settle |
| `unknown` | otherwise |

Rules:

- **Turn and background are separate facts** (#697). An agent with a live
  `run_in_background` shell whose turn has ended is `turn: idle, background:
  running`, never `working`. Anything that means "at the end of the turn"
  (idle wake, self-compact, guarded submit's settle gate, goal verdicts) keys
  on `turn`, never on `background`.
- **`settled` is one server constant.** The settle window is applied once, in
  the server, before an event is emitted (default 20 s for pushed supervisor
  events; the measured 5 s filter already removes 48 % of raw transitions and
  20 s removes 54 %, so the longer window costs little and buys protection
  against compaction and quota screens). An explicit wait may ask for a shorter
  window; it never gets a longer-lived guess than the server's.
- **`seen` never drives supervision.** `done` vs `idle` stays a sidebar
  concept.
- **Each axis names its source** (`screen`, `hook`, `plugin`, `process_tree`,
  `wait_registry`). Per harness, today's sources are:

  | Axis | Claude Code | Codex | opencode |
  | --- | --- | --- | --- |
  | turn | screen detector, Stop / UserPromptSubmit hooks | local rollout lifecycle events; hook then screen only if unavailable (§1a) | plugin `session.status` |
  | background | screen footer ("N shells"), process tree | process tree (#663) | process tree |
  | needs_human | screen (permission / question / trust / login dialogs) | screen (approval, question, trust, login) | plugin + screen |
  | provider_limit | screen (limit footer, `/rate-limit-options` menu, last turn's limit notice) | screen (last turn's usage-limit error) | `detect/provider_limit.rs` |

  A harness that cannot provide an axis reports `unknown` for it, and
  `phase` falls back accordingly. `finished` is never inferred from an
  `unknown` background.
- **Today's `blocked` splits.** The detector's `blocked` (a visible permission
  or question prompt) becomes `needs_human`. `blocked` is reserved for states
  no answer from a human at the pane resolves quickly (limits, auth, crashes).
  `agent_status` keeps reporting `blocked` for both, and a quota screen the
  harness draws over an idle prompt box reads `blocked` there too.
- **Attention outranks turn and background** (#1023). A prompt drawn over a
  running turn is `needs_human`, not `working`, and a limit hit with a
  background shell alive is `limit_reached`, not `finished_with_background`.
  While attention is set, `turn` reads `unknown` (a visible prompt is not a
  turn fact) and `background` keeps its own value for when it clears. A limit is scoped to the turn that hit it: the first
  later turn that runs clears it, so a stale notice in scrollback never holds
  `limit_reached`. A `blocked` report whose source cannot name the prompt (a
  hook, a harness without matchers) is `needs_human{unknown}`, sourced to
  that hook. Neither Claude nor Codex shows a duration to the reset, only a
  wall-clock time, so their `retry_after_ms` is absent.

### 1a. Codex lifecycle turn authority (#1479, 2026-10-15)

The operator chose **lifecycle event > hook state > screen** for Codex's
**turn** axis on 2026-10-15. The source and continuity contract below is the
proposed amendment implementing that decision, submitted for review before
code. It amends §1's source table, not the Claude harness or its Stop hook.
PR #1491 remains independent hollow-glyph hardening; neither that PR nor this
amendment implements lifecycle authority.

The isolated #1479 reproduction held one real Codex turn open while a background
command ran and commentary streamed. Flock reported idle for 77.832 seconds,
then working again before that turn completed. Codex deliberately hides its
Working row during streaming and restores it after commentary; the composer
and background controls also exist after a real turn end. See the pinned
[streaming behavior](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/tui/src/chatwidget/streaming.rs#L500-L524).
Lifecycle records resolve that ambiguity locally without depending on an
upstream UI change or treating background activity as a turn. This does not
claim an exact historical frame was retained for every original false sample.

#### Selected source and version evidence

Read the active thread's **local rollout JSONL**, under the `CODEX_HOME`
resolved for its launch (the harness default only if no override was supplied).
The compatibility baseline is **Codex CLI 0.160.1**, observed in #1479's
isolated probes. All upstream references here pin `rust-v0.160.1`.

| Candidate | Baseline behavior | Decision |
| --- | --- | --- |
| Hooks | All 12 are supported: PreToolUse, PermissionRequest, PostToolUse, PreCompact, PostCompact, SessionStart, SessionEnd, UserPromptSubmit, SubagentStart, SubagentStop, Stop and Interrupt. Tool, permission, compaction, session and subagent hooks are not parent-turn boundaries. UserPromptSubmit can reject input and Stop can block completion; Interrupt is not a committed paired end. | Keep SessionStart for binding. None supplies a committed parent-turn start/end pair. |
| Legacy `notify` | `agent-turn-complete` carries thread/turn IDs, but has no paired start notification; dispatch spawns a process without waiting for delivery. | Do not replace the user's notification program or use an unpaired notification as authority. |
| Rollout JSONL | Turn start, complete and abort are persisted in both legacy and paginated history. Records are newline-delimited and flushed. | Selected: local, attributable and replayable evidence of actual boundaries. |

Evidence: [hook names](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/hooks/src/lib.rs#L20-L34),
[rejectable prompt submission](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/hooks/src/events/user_prompt_submit.rs#L174-L246),
[Stop's blocking outcome](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/hooks/src/events/stop.rs#L99-L112),
[notify payload and dispatch](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/hooks/src/legacy_notify.rs#L13-L74),
[persistence policy](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/rollout/src/policy.rs#L94-L119),
and [newline/write/flush](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/rollout/src/recorder.rs#L2072-L2095).

For `type: event_msg`, consume `payload.type: task_started` / `task_complete`,
including the documented aliases `turn_started` / `turn_complete`. Start means
working; completion with the matching `turn_id` means idle. Terminal non-abort
task failures also emit completion with an error; interruptions emit abort. An
error notice alone is not an end boundary. A matching `turn_aborted` ends the
active turn; an abort with no attributable turn ID yields unknown, never guessed
idle.

See the pinned [names and aliases](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/protocol/src/protocol.rs#L1406-L1418),
[start/complete payloads](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/protocol/src/protocol.rs#L2152-L2198),
[abort payload](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/protocol/src/protocol.rs#L4256-L4277),
[terminal task-error handling](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/core/src/tasks/mod.rs#L629-L658),
and [completion/abort construction and dispatch](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/core/src/tasks/mod.rs#L811-L857).
Private guardian-review completions skip the initial dispatch at lines 853–857;
they are sent after clearing the active turn at lines 873–876, rather than
omitted altogether. See [deferred guardian dispatch](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/core/src/tasks/mod.rs#L853-L883).
These are internal child reviewers, not the parent pane's bound thread; their
completion cannot end its turn.

Tool output, commentary, usage, compaction and subagent completion never start
or end the parent's turn. In particular, ExecCommandBegin and McpToolCallBegin
are transient and are not persisted: a long tool call can legitimately produce
no rollout records. See the pinned [transient-event policy](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/rollout/src/policy.rs#L140-L178).
Unknown record/event kinds are skipped without
changing state. Partial final lines wait for a newline. Any unterminated final
line, including a partial `task_started`, blocks publishing idle until completed
and reduced; retain working if already working, otherwise yield unknown with
`codex_partial_record`. Malformed complete
records, gaps and conflicting turn IDs invalidate authority. Duplicate records
are idempotent, and a late completion for an older turn cannot end a newer one.

#### Session attribution, resume and cross-host moves

Bind the reader to `(node identity, pane/terminal ID, launch generation,
binding generation, Codex thread ID)`. Use the SessionStart report from that
pane, its resolved Codex home and a verified local writer identity (PID plus
process start identity). Names, labels, cwd and nearest timestamps are not keys.
The reader and process/file checks live in runtime; the reducer is pure data.

**Before the first SessionStart, there is no lifecycle source for the pane:**
publish `unavailable: awaiting_first_turn` and use the existing screen evidence
exactly as today. Idle wake, guarded submit, mail and waits retain their existing
screen-based eligibility; unavailable does not itself mean idle. A visible
Working screen means working and unclassifiable screen evidence means unknown.

SessionStart is deferred in Codex 0.160.1. Initialization only
[queues its source](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/core/src/session/session.rs#L1905-L1935);
[the hook runner consumes that queue](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/core/src/hook_runtime.rs#L128-L170)
inside [run_turn](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/core/src/session/turn.rs#L322-L324),
including [after compaction](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/core/src/session/turn.rs#L640-L642).
RegularTask [emits TurnStarted first](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/core/src/tasks/regular.rs#L48-L52).
Thus a launch, resume or clear can sit at its prompt without any SessionStart.
Do not require that hook to permit the prompt that causes it. On the first
SessionStart, validate and bind, catch up the rollout and normally report
working from that turn's already-written start. From then on lifecycle has
precedence; validation failure after a binding claim yields unknown.

**Every SessionStart rebinds**, even within one OS launch and even when the
reported thread ID has not changed. Increment the binding generation and revoke
old callbacks for `startup`, `resume`, `clear`, `compact` and `fork`. This includes
`/new` and `/clear`; a launch-generation check alone is insufficient. The pinned
[source enum](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/hooks/src/events/session_start.rs#L24-L39)
and [command payload](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/hooks/src/events/session_start.rs#L130-L141)
provide the source and `transcript_path`.

Use **SessionStart's `transcript_path` as the primary file key**. Resolve and
validate it as a local file in this launch's Codex home, then validate the first
`session_meta.id` against the bound thread ID. Do not substitute root
`session_id`, parent/fork IDs or a filename suffix: physical rollout IDs can
differ after revert and a root session can contain several threads. See the
pinned [metadata definitions](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/protocol/src/protocol.rs#L3105-L3155)
and [first-metadata rule](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/rollout/src/recorder.rs#L1116-L1145).
An index may assist validation, not choose a different file over the hook's
path. A missing path needs an explicit unavailable-source explanation or leaves
an expected local source initializing; never guess a file by name/cwd or search
an unrelated profile. Do not follow remote-reported paths. Ambiguous writers
or mismatched metadata yield unknown.

On a known launch/resume or confirmed thread replacement (`/new`, `/clear`),
retire the old binding and discard its queued callbacks, then return to
`unavailable: awaiting_first_turn` and screen fallback until the next
SessionStart. Use an attributed runtime replacement confirmation, not typed
command text, an idle composer, a label or a timeout, as replacement evidence.
**Observability limit:** today's Codex integration [installs only SessionStart](../../src/integration/mod.rs#L1831-L1838);
it does not receive an immediate clear/new confirmation. For a command entered
inside the TUI that flk cannot observe as a confirmed replacement, retirement
cannot occur before the next SessionStart. Keep the old binding until that
report atomically retires it and claims the new binding; do not pretend a
pre-hook fallback transition was observed. Known launch/resume generation
changes are observable to flk before the hook and do enter fallback immediately.
An observed source failure without replacement evidence still yields unknown,
never this bootstrap fallback. The implementation must expose this limitation
and test both confirmed and initially unobserved replacement paths.

For a resume, record the pre-launch file identity, verified prefix and complete
byte offset, and buffer appended records until binding validation succeeds.
At the deferred SessionStart, use the new writer's attributable start after that
snapshot, not an inherited unfinished turn or old completion, as the current
boundary. A copied snapshot also needs destination verification and local
ownership. Missing/bad paths or metadata, ambiguous writers and discontinuity
after the binding claim yield unknown. No empty-history or pre-launch completion
boundary establishes lifecycle idle.

For a copied fork, the boundary after imported history is the child's
`event_msg: thread_settings_applied` with `payload.thread_id` equal to the bound
child ID: [fork seeding appends it after the copied prefix](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/core/src/session/mod.rs#L1635-L1663),
[the payload preserves the owner's thread ID](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/protocol/src/protocol.rs#L2200-L2207),
and [the child checkpoint sets that ID](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/core/src/session/thread_settings.rs#L125-L131).
Copied parent settings retain their parent ID. Begin child boundary reduction
after the verified child settings record; this record is an attribution delimiter,
not a turn end. In referenced history, inherited records remain behind
`session_meta.history_base`; the same child settings record begins local history.
If the delimiter/owner cannot be verified, yield unknown rather than treating
imported starts or completions as the child's turn.

A reducer state may transfer only with proven continuity of the same local
writer, thread, file and cursor; equal thread IDs alone do not suffice. In
particular, SessionStart `compact` during a running turn preserves working when
that continuity is proven. Otherwise it yields unknown with
`codex_compaction_continuity_unproven`; compaction is not a fresh-session idle
boundary. Delayed records from a revoked binding are discarded, including
within one launch after `/new` or `/clear`.

A move (#1500) revokes the source pane's binding before the destination claims a
new launch generation. Rebind to the destination's **local** writer and Codex
home; copied history remains history. Replace the pane/socket integration
environment with the destination's route. The old hibernated record cannot
consume destination events. Reject a report routed only by an old name/alias.
Known duplicate live ownership of a thread is ambiguous and yields unknown
until resolved, never last-callback-wins. These are the adapter's requirements
on #1500's move transaction; name lookup, move UI and narrow resumed panes
remain outside this amendment. A rollout copy alone grants no authority.

#### Owner decision required: format dependency and version policy

**Pending owner decision.** Rollout JSONL is an internal storage format, not a
promised stable integration API. Selecting it requires an explicit compatibility
policy; the lifecycle-authority decision alone does not settle that policy.

The **recommended current proposal** is:

- Record `session_meta.cli_version` for every binding. Initially the tested
  range contains only **0.160.1**; widen it only with explicit compatibility
  evidence and review, never an assumed semver-compatible range. Resume preserves
  creation metadata, so also verify the version of the actual writer executable
  resolved for the launch; old metadata alone cannot authorize a newer writer.
  Both must be covered by the tested compatibility declaration. See the pinned
  [new versus resumed recorder setup](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/rollout/src/recorder.rs#L935-L994).
- A version in that range uses the lifecycle adapter. Any other or missing
  version marks this source unavailable with `unsupported_codex_version` and
  selects screen fallback, with the reason exposed. A version mismatch itself
  never means idle: before fresh classifiable screen evidence, state is unknown;
  a visible Working screen means working. Any later idle must come from actual
  fallback evidence, not an unsupported-version default or copied completion.
- CI pins the tested Codex version and versioned fixtures and fails loudly when
  that pin changes without updating the declared range and compatibility tests.
  This is a requirement for the later implementation PR, not a CI change here.

An alternative is Codex's supported, versioned **app-server** interface, whose
v2 notifications include `turn/started` and `turn/completed`; see the pinned
[versioned protocol modules](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/app-server-protocol/src/protocol/mod.rs#L10-L13)
and [notifications](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/app-server-protocol/src/protocol/common.rs#L1943-L1947).
It is not selected by this proposal because flk currently runs the Codex TUI in
a pane; owning or attaching an app-server connection for that same thread needs
a separate transport/ownership design. It cannot be replaced by launching a
second independent session and observing its events. The owner must choose the
bounded rollout compatibility policy or that interface change before code.

**Persistence-disabled configurations:** `codex exec --ephemeral` runs without
session files; effective `Config.ephemeral` bypasses thread persistence. The
baseline also permits a test/debug in-memory thread-store configuration. These are known
unavailable local sources, with `codex_rollout_disabled`, and use explicit
fallback as above. An unexplained absent file for an expected persistent source
is source loss or initialization, not proof of ephemeral mode. `history` controls
`history.jsonl`, not the canonical turn rollout; do not infer disabled rollouts
from that setting. Evidence: [exec flag](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/exec/src/cli.rs#L35-L37),
[ephemeral persistence bypass](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/core/src/session/session.rs#L1006-L1012),
[local versus in-memory store](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/core/src/config/mod.rs#L597-L604),
and [distinct history/ephemeral fields](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/core/src/config/mod.rs#L960-L965).

#### Precedence, fallback and source continuity

| Evidence for this binding | Decision |
| --- | --- |
| Screen approval/question prompt (`needs_human`), even with lifecycle idle | Attention wins: `turn: unknown`, `attention: needs_human`, `phase: needs_human`, per §1. Preserve the underlying lifecycle boundary for when attention clears. |
| Available, synchronized start without a matching end | Working until matching end or observed source loss; ignore hook/screen idle, DONE text and hidden Working chrome. |
| Available, synchronized matching complete/abort, no attention | Idle despite stale Working chrome; background remains independent. |
| No binding yet, or old binding retired on confirmed replacement | Screen fallback with `awaiting_first_turn`; no lifecycle claim until SessionStart. |
| Expected/claimed but validating, lost, unreadable, discontinuous or ambiguous | Unknown with source and reason; hook/screen cannot conceal source loss. |
| Explicitly unavailable (absent/disabled adapter, known disabled persistence, or version excluded by the proposed policy) | Hook then screen fallback for absent/disabled adapters; the proposed unsupported-version policy selects screen. Publish the reason and selected source; unclassifiable evidence stays unknown. |

**No silence lease.** The last validated lifecycle boundary holds, however old,
until evidence invalidates the source: writer exit or PID/start-identity change,
file replacement or truncation, revoked ownership, supervised failure/stall of
flk's own reader, or rebind. A successful empty read is normal. An eventless long
tool call remains working; an eventless idle session remains idle. Neither a
missing tool-begin record nor increasing age is a state transition.

Publish `last_evidence_age` as a fact measured with the server's monotonic clock,
plus source health and degradation reason. Consumers must not reinterpret age as
a lease or a second idle/unknown timeout. Reader supervision checks flk's own
progress, including successful EOF checks, not Codex event frequency. Observed
source loss yields unknown immediately; an idle composer cannot override it.
After a claimed source is lost it cannot quietly become unavailable for fallback.
Confirmed launch/thread replacement is the explicit exception: retire ownership
and return to awaiting-first-turn fallback as specified above.
Explicit version/config unavailability is diagnosed at binding, under the pending
policy above. Rebind again to switch policy/version, rather than reinterpret an
active binding's cached state.

Define a **gap** as loss of verified file/cursor continuity. Track file identity,
complete-record byte offsets and verification of previously consumed bytes.
Legacy mode [emits no ordinal](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/rollout/src/ordinal.rs#L25-L41);
the serialized [field is optional](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/rollout/src/recorder.rs#L2061-L2067).
Missing ordinals are normal for legacy files, not gaps. Detect replacement, shrink/truncation,
a changed verified prefix, skipped/unreadable byte ranges and malformed complete
records. Where ordinals exist, check their format-defined continuity too.
Do not infer a gap from silence, transient events missing by persistence policy,
or a non-contiguous sequence of turn IDs. Legacy files cannot prove that an
upstream producer silently omitted a well-formed record; this format limitation
belongs to the owner decision. **Residual risks:** if the writer remains alive
but its end record is lost, the last start holds working indefinitely. Publish
a diagnostic when lifecycle and screen disagree, exposing both observations
and the active boundary so an operator can request a rebind. Neither the
diagnostic nor requesting a rebind changes state on its own: rebind must validate
and reconstruct a trustworthy current boundary before publishing a new decision.
A resume of a file whose last turn never ended is a second residual risk:
before SessionStart, screen fallback shows the current prompt as idle when the
existing detector classifies it idle (or working/unknown from its actual screen
evidence), rather than inheriting the old unmatched start. Submission remains
eligible under today's screen/composer gates, so that history cannot deadlock
bootstrap. On binding, the verified new writer's start establishes working;
ambiguous attribution remains unknown. Revalidation must not conceal an observed
gap; resynchronize and require a trustworthy current boundary before publishing
state.

Background remains independent (§1). A child process or terminal count cannot
supply missing turn evidence. Completion with a live child remains idle with
background running. Source loss is unknown, not finished. Existing attention,
process-exit and hibernation precedence remains; DONE text and widened redraw
holds do not determine turn state.

#### Consumers of unknown and quiet idle

| Consumer | On source-loss `unknown` | On quiet, known idle with a healthy source |
| --- | --- | --- |
| Waiters | An idle/working/settled predicate is not satisfied. An explicit unknown predicate can match with its reason. Keep deadlines, cancellation and source/producer failure reporting bounded (ADR-0037). Mail/event predicates independent of turn still match their own data. | Idle/settled waits satisfy normally after their existing settle condition; growing evidence age does not reset the held interval. |
| Idle wake | Do not type an idle wake. Retain the pending wake and re-evaluate when idle is known. | Wake normally once the existing idle/injection gates pass; no new rollout record is required. |
| Self-compact | Keep the request armed; do not initiate compaction on an unknown turn. | Compaction remains eligible under its existing gates, including a fresh composer check. |
| Guarded submit | Refuse submission with the existing not-ready outcome and source reason; send no keys. | Existing idle, draft, lease and fresh-composer gates apply. Read the current buffer; do not require newly emitted PTY bytes or a new lifecycle record. |
| Mail delivery | Durable routing, custody, enqueue and reads remain available. Hold only automatic typed wake/submission until eligible; do not drop or claim delivery failed merely because turn is unknown. Registered mail waits wake on their own predicates. | Delivery and eligible needs-reply wake work as before; fyi still does not idle-wake (§5). |

Thus a quiet idle agent does not become unknown with age: it can still receive
mail, satisfy idle waits, wake, compact and accept guarded submissions under
the existing gates. Source failure, attention or rebind can change eligibility;
silence cannot. Additive source/age fields follow §3a.

#### Test plan for the subsequent implementation PR

Use a scripted fake Codex source in an isolated flk with sandbox HOME/CODEX_HOME,
short explicit sockets and inherited FLOCK variables removed. Feed real-shaped
JSONL and captured bottom-buffer text/ANSI; use a pure reducer with a supplied
monotonic clock. No live model, sleeps, retries or production-only test hooks.

1. Hidden-Working replay: after start, commentary/background/composer frames
   yield working. Matching complete with background retained yields idle.
   Preserve #1284's reverse case. Demonstrate the streaming reproduction fails
   before and passes after implementation through agent get/list, events and waits.
2. Long tool call with no records and a live writer yields working after hours
   of supplied-clock advance. Silence after completion yields idle after hours.
   A claimed but unverified binding yields unknown. Writer exit yields unknown;
   PID reuse yields unknown; a supervised wedged/dead reader yields unknown;
   replacement or truncation yields unknown. Bounded integration waits must name
   a missing condition rather than hang. Test idle wake, self-compact, guarded
   submit, mail delivery and idle waits on quiet known idle: each remains eligible
   under its existing gates. For source-loss unknown, each follows the table above.
3. Exercise each reducer case independently, with one expected turn state:

   | Input and prior state | Expected turn |
   | --- | --- |
   | Either start alias | Working |
   | Either completion alias matching the active turn | Idle |
   | Duplicate matching completion after completion | Idle |
   | Partial matching completion line after start | Working |
   | Unterminated final start line after previously idle | Unknown (`codex_partial_record`), never idle |
   | Completed newline-terminated matching completion record after start | Idle |
   | Unknown record/event kind after start | Working |
   | Malformed complete record | Unknown |
   | Late old-turn completion while a newer turn runs | Working |
   | Conflicting unrecognized turn ID | Unknown |
   | Matching abort | Idle |
   | Unattributable abort | Unknown |
   | Non-boundary output after start, including an error notice | Working |
   | Terminal task failure's matching completion with error | Idle |
   | Legacy file without ordinals after start | Working |
   | Verified prefix mutation | Unknown |
   | Ordinal discontinuity | Unknown |
   | Valid resynchronization with attributable current start | Working |

   Only matching boundaries advance the turn cursor; age metadata alone emits
   no phase transition. Separately simulate a lost end with a live writer:
   working persists and screen disagreement publishes a diagnostic. A rebind
   request alone cannot convert it to idle; reconstructing a verified active
   start retains working, or failed validation leaves unknown.
4. Follow the pinned real event order, with separate scenarios:

   - Fresh launch: `awaiting_first_turn` uses screen fallback; the prompt is
     eligible under existing gates. Submission emits task_started before
     SessionStart; the validated hook binds as working, then matching
     task_complete makes it idle.
   - Resume: screen fallback permits guarded submit under existing draft, lease
     and fresh-composer gates; task_started precedes SessionStart, which binds
     as working. Cover completed history and a last turn that never ended,
     including verified copied history; neither supplies a bootstrap idle
     lifecycle boundary or blocks screen-based submission.
   - Confirmed `/clear` or `/new` during a session: retire the old binding,
     enter screen fallback, then the next turn's SessionStart binds as working.
     Delayed old callbacks cannot affect the fallback or new active turn.
   - A manually entered clear/new without observable replacement confirmation
     retains the old binding until the next SessionStart atomically replaces it.
     Assert this observability limitation rather than inventing an early signal.
   - Fork: imported parent completion precedes the child's owned
     thread_settings_applied delimiter; only the child's subsequent start
     establishes working. Missing/ambiguous delimiter yields unknown.

   Same-name/cwd panes with different thread IDs remain isolated: starting only
   pane A yields working for A and idle for an already completed B. Each claimed
   but unvalidated SessionStart binding yields unknown. Compaction during an
   active turn with proven writer/thread/file/cursor continuity stays working;
   unproven continuity yields unknown with
   `codex_compaction_continuity_unproven`. A stale callback while the rebound turn
   runs leaves working. After a move the old pane remains hibernated; the
   destination starts in screen fallback until its own deferred hook binds.
   Duplicate live writers yield unknown. A mismatched transcript_path/meta pair
   yields unknown. Another thread's end through fork/revert metadata leaves the
   current active turn working. Private guardian child completions, including
   their deferred dispatch, cannot complete the parent turn.
5. Lifecycle start against hook/screen idle yields working. Lifecycle complete
   against stale Working chrome yields idle. A screen approval prompt against
   lifecycle idle yields `phase: needs_human` with turn unknown; clearing that
   prompt restores idle from the retained completion. Claimed source loss against
   screen idle yields unknown. Unsupported version with a visible Working screen
   yields working from screen with `unsupported_codex_version`; with unclassifiable
   screen it yields unknown. Resume with tested creation metadata but an untested
   writer and a Working screen yields working from screen with the same named
   version reason. Supported 0.160.1 with a start yields working from
   lifecycle. Ephemeral/in-memory configuration with a Working screen yields
   working from fallback with `codex_rollout_disabled`. Preserve Claude tests;
   validate additive source fields and unknown persisted kinds (§3a).

The code PR must include captured-frame provenance, the deterministic failing
baseline, source-loss diagnostics and these results. This docs PR implements
no reader, hook, detector, protocol field or configuration setting.

### 2. Supervisor events are derived facts, persisted once, in the existing log

The server emits a small family of **derived, durable** event kinds, computed
from `state` after the settle window, so that no consumer re-derives them:

- `agent.phase_changed { agent_id, pane, from, to, state, held_ms }`, emitted
  only for settled transitions between supervisor-relevant phases;
- `agent.needs_human`, `agent.limit_reached`, `agent.gone` as their own kinds,
  because notification routing (§8, ADR-0016) keys on them;
- `pr.ci_completed { repo, branch, pr, head_sha, conclusion, failed_checks }`
  and `pr.merged` / `pr.closed` from the batched PR poller (#663 part B, no
  per-worktree `gh` exec). `pr.ci_completed` is emitted once per head SHA:
  the first conclusion wins, and a re-run of CI on the same SHA is never
  announced again. A PR already merged or closed when the server starts is
  history and is not announced;
- ADR-0027's `AgentDelegated`, `DelegateReaped`, … and ADR-0030's `goal.*`.

Raw `pane_agent_status_changed` stays (the sidebar uses it), but it is not
part of the supervisor contract. ADR-0005 P5 still holds: these are new kinds in
the one log, not a second event system.

### 3. Delivery guarantee: at-least-once from a cursor the consumer holds

- **Order.** One total order per server: `seq`. No order is promised across
  hosts. An event relayed from another host carries `origin_node` and
  `origin_seq`, and a consumer orders per origin.
- **At-least-once.** A consumer reads `events.read { after: <seq>, kinds?,
  subjects?, limit, wait_ms }` and gets every matching event with `seq > after`,
  plus `next_after`. The server serves from the ring and falls back to the
  rotated files on disk, so a reader that was away for hours misses nothing the
  log still holds. Consumers dedupe on `seq` (or `origin_node, origin_seq`).
  Exactly-once is not offered; idempotent consumers are cheaper than a
  delivery ledger.
- **Gaps are explicit.** If `after` is older than the oldest retained event,
  the reply starts with `gap { from, to }` and the consumer decides (re-read
  `delegate.list`, which is the current state). Silent loss is a bug.
- **Back-pressure is pull.** `events.read` is request/response with a `limit`;
  a slow consumer only slows itself. The streaming `events.subscribe` stays for
  the TUI and the sidebar, gets the same cursor semantics (start at `after`, or
  at "now" when omitted, never at 0), and reports `lagged { skipped_to }`
  instead of dropping silently.
- **Coalescing happens at delivery to a recipient, never in the log.** The log
  records each fact once. A recipient that is an agent (the ADR-0027 digest),
  or a channel adapter (§8), gets a per-recipient buffer that keeps the latest
  state per subject for state-like kinds (`agent.phase_changed`) and keeps every
  edge-like fact (`pr.merged`, `DelegateReaped`, `goal.met`) as its own line.
  At the measured 12 settled transitions per host-hour, an architect with eight
  delegates gets well under one digest per delegate per hour, which matches
  the cadence `supervisor.py` already had to batch to.

### 3a. Mixed fleets: every change here is additive on the wire

Clients and servers of different versions meet daily: a CLI updated before the
server it talks to, an MCP server process started by an older binary, and mesh
peers on older releases (#868). Requests carry no client protocol version, so a
server cannot tailor a reply to an old client. Therefore:

- **Responses only gain fields.** `state`, `phase`, `waiting` and the derived
  kinds are additions. Old clients print the envelope as-is and ignore what they
  do not know; MCP passes it through. A new client reading an old server treats
  a missing `state` as `phase: unknown` and falls back to `agent_status`.
- **New methods are new `Method` variants**, gated by #868: a state-changing one
  (`wait.park`, channel binding calls) is in `needs_current_protocol()` and
  refused locally with `server_version_gap` (exit 78) against an older server;
  a read-only one (`events.read`) reaches the old server, fails as an unknown
  variant, and `normalize` turns that into the same exit 78 with the handoff
  hint. No new method is ever sent to an old server and misread as success.
- **One protocol bump per release** that adds methods or changes request
  shapes (AGENTS.md rule: compare `PROTOCOL_VERSION` with the last released
  tag, bump only if not already ahead). Field additions alone need no bump.
- **Behaviour changes on existing methods are named.** `events.subscribe`
  starting at "now" instead of replaying the ring from 0 is visible to an old
  client talking to a new server; nothing in-tree relies on the replay, and
  ADR-0005 calls it a non-goal, so it ships as a changelog line.
- **The durable log must stay readable by the previous release.** New kinds
  are appended to `event-log.jsonl`; an older server (after a downgrade, or the
  old side of a live handoff) must skip kinds it does not know rather than
  quarantine the file. E3 verifies that against the last released binary before
  it adds a kind, and fixes the reader first if it does not hold.
- **Mesh peers on an older version** relay nothing new. This ADR's events and
  waits are local to a server; cross-host agents appear with whatever their own
  server reports, so an older peer's agents read `phase: unknown`, and a wait on
  a remote subject is refused with `peer_incompatible` rather than left open
  forever.

### 4. Background work is attributed, and harness background waits count as waits

The sandbox measurement (Context) shows both halves: today's Claude detector
read a turn that ended with a live background shell as `idle` (so the wake
fired), and the harness woke the agent 352 ms after its background task
ended. Neither is guaranteed by a contract today, and the field report in #697
saw the opposite reading. So:

- `background` is reported from the harness where it says so (Claude's footer
  counter) and from the pane's process tree otherwise (#663 A).
- A background process **whose ancestry is a `flk` wait call on this server**
  is not unknown background work: the server sees the wait request (§5) and
  attributes it. An agent that runs `flk events wait …` under its harness's
  background mechanism (Claude `run_in_background`, `Monitor`) is therefore
  `waiting_on_event`, not `finished_with_background`, and the goal supervisor
  (ADR-0030) parks it silently instead of sending a `bg-checkin`.

### 5. How an agent waits: a registered wait, woken by the server

An agent never polls. It **registers a wait** and is told when it opens. There
are exactly two ways to register one, and both go through the server, so the
server always knows what an agent is waiting for:

1. **Block inside the turn** — `flk events wait --kinds … --subject … --after
   <seq> --timeout <dur>` or MCP `flock_events_read { after, wait_ms }`. A
   server-side long-poll on the hub condvar (no client loop). For short waits
   within a turn, bounded by the harness's own tool timeout (MCP calls are
   capped at 600 s today). The same verb run as a harness background task is
   the harness-native form of 2.
2. **Park and end the turn** — `flk wait park --until <predicate>` (MCP
   `flock_wait_park`), predicates such as `delegate:<id> phase in
   [finished, needs_human, gone]`, `pr:<n> ci_completed`, `reply:<corr>`,
   `any-delegate-of-mine changed`, plus a mandatory `--deadline`. The call
   returns at once with a `wait_id`; the agent ends its turn.

The server records each open wait against the **attested caller** (the same
ancestry walk `msg.send` and `spawn_caller()` use, ADR-0014), which is what
fills `state.waiting` and lets ADR-0030 tell "parked" from "stalled" without a
judge.

When a registered wait opens (predicate true, deadline hit, or the subject
gone), the server delivers a **wake**, through exactly one of:

| Recipient's turn | Path | Measured |
| --- | --- | --- |
| mid-turn (blocking form) | the long-poll returns | n/a (same process) |
| mid-turn (parked, then busy with other work) | Stop hook output at the next turn boundary | read ≈ at the boundary, no idle gap |
| turn idle | guarded idle-wake injection of a fixed sentence (#850 lease, draft stash) | 261–498 ms send → turn |
| turn idle, channel push enabled | ADR-0019 push first, injection after the grace | not measured here |

Rules:

- **The body is never typed.** The wake is the fixed sentence, naming the tool
  to call (`flock_events_read` / `flock_msg_read`). The payload is pulled.
- **One injector.** Every typed wake, digest wake, goal nudge and self-compact
  write goes through the one guarded injection (#850). One lease per pane; the
  operator's keys are buffered, a one-row draft is stashed and restored, a
  multi-row draft makes the wake wait. Priority when several are due at the
  same boundary: **operator input first** (as #850 and ADR-0027 §3 already
  decide: a human typing is never overridden), then an armed compaction, then a
  wake, then a goal continuation (ADR-0030 §4). Compaction above continuation
  settles the 2026-10-09 starvation between a goal continue-hook and an
  idle-gated compaction; operator input above compaction means a compaction
  waits for the operator's quiet window like every other write.
- **Wake follows the wait, not the intent.** ADR-0018 keeps `fyi` from waking
  an agent. That rule stands for peer mail, and gains one clause: **a message
  or event that opens a wait the recipient registered wakes it, whatever its
  intent.** An architect that parked on its delegates is woken by the `fyi`
  digest; an architect that did not park is not interrupted by it and reads it
  at its next boundary. This amends ADR-0027 §3 (whose digest is `fyi`) without
  changing ADR-0018 for peer messages.
- **Idle wake extends past Claude.** It is Claude-only today because only
  Claude is assumed to have the inbox tool. A harness whose flock MCP server is
  configured (Codex, opencode) gets the same wake through the same guarded
  submit, which already supports their composers. A harness with no inbox tool
  gets a typed sentence telling it to run `flk events read`.
- **Polling is unsupported, not forbidden.** `agent get` and `pane read` stay,
  but the docs, skills and briefs stop teaching loops over them (§9).

### 6. Settle, coalesce and wake thresholds are configuration with measured defaults

| Key | Default | Why |
| --- | --- | --- |
| `[events] settle_ms` | 20000 | §1; 54 % of raw transitions removed, compaction and quota screens filtered |
| `[events] read_max_wait_ms` | 540000 | 60 s of headroom below the 600 s MCP tool-call cap, so the server answers (with an empty batch and `next_after`) before the harness kills the call |
| `[digest] max_lines` | 12 | one screen; overflow → `+k more: flk delegate list --mine` |
| `[wake] operator_quiet_ms` | 15000 | unchanged from idle wake |

### 7. What does not change

Mail custody (ADR-0026), the inbox as the source of truth, ADR-0018's intent
semantics for peer messages, ADR-0019's opt-in push, and the screen-detector
evidence rule in AGENTS.md.

### 8. External channels (#1005) are a consumer and a producer of the same contracts

An external channel (Telegram first, through `gerchowl/telegram-bot`) plugs in
as an **adapter process outside the server**, speaking the socket like any
other client. It gets no private path.

- **Outbound = an `events.read` consumer.** The adapter holds its own cursor
  per channel binding and gets §3's at-least-once delivery, explicit gaps and
  pull back-pressure. It dedupes on `seq` before posting, so a crash between
  "posted" and "cursor saved" re-posts at most the unacknowledged tail. The
  server keeps a per-binding coalescing buffer exactly like an architect's
  digest (latest state per subject, every edge-like fact kept), so a phone
  sees one line per changed agent, and the adapter's own rate limit (Telegram's
  per-chat limits) only ever drops to "coalesce harder", never to "drop facts".
  What a binding receives is a server-side filter: fleet (needs_human,
  limit_reached, releases), repo (that repo's delegates and PRs), agent (that
  agent's phases, needs_human, its outbound mail).
- **Inbound = mail, never keystrokes.** A message in an agent's channel becomes
  a `msg.send` into that agent's inbox, with sender `channel:<binding>/<external
  user>` and intent `needs_reply` (so an idle agent is woken by §5's injection
  path), or a `msg.reply` when it answers an agent's question (the main use: an
  agent stuck on a question to the owner). It inherits mail custody, the
  injection lock and the wake rules. Typing the text into the pane through
  `agent send` is not offered to channels: a remote, delayed sender is exactly
  the case where a composer draft or a changed turn would be clobbered.
- **What the remote sender learns** follows ADR-0029's submit/confirm
  contract, reported back to the chat: `queued` (in custody), `woken` (the wake
  was accepted by the harness), `read` (the agent pulled it), `replied`.
  `abandoned`/`unconfirmed` wakes are reported as such, never as delivered.
- **Identity and rights are the server's, not the adapter's alone.** The
  adapter is not a pane, so ancestry attestation cannot identify it. It
  authenticates with a per-binding capability token the server issued; the
  binding in server config declares scopes (`notify`, `send_to_agent`,
  `reply`), and the server refuses anything outside them and logs every inbound
  message with the external user id. The adapter's own allow-list of chat and
  user ids is a second check, not the only one.
- **One owner per binding.** A binding is served by exactly one node, named
  statically in the binding's config (`owner_node`); there is no lease and no
  automatic failover. An offline owner means the channel is quiet and resumes
  from its cursor when the node returns, not that another node double-posts.
  Moving a binding is a config change. Cross-host agents are
  reached through mesh mail (ADR-0026), not through a second bot.
- **Not decided here:** Telegram formatting, attachments, the onboarding flow
  and whether flk embeds the adapter later. The contract above is what any
  embedding must keep.

### 8a. Open questions are first-class: structured options, a reply path, and one answer

Added by the owner on 2026-10-11 (#1005, two additions: options, then a reply
path). The main reason to reach an agent from a phone is to answer the question
it is stuck on, and most such questions have choices. A question, and its
choices, must therefore travel as data, not as screen text.

- **What counts as an open question.** Every `needs_human{question}` (a harness
  question or choice dialog on screen) and every unanswered `needs_reply`
  message addressed to the operator or a channel. Each has a stable
  `question_id`, with or without options.

- **Payload.** `needs_human{question}` gains an optional `question` object, and
  so does a `needs_reply` message whose sender attached options:

  ```text
  question {
    question_id,                 // stable for one dialog instance or one message
    source:       screen | message,
    prompt,                      // the question text
    options:      [{ id, label, description? }],
    multi_select: bool,
    allow_other:  bool,          // free text accepted ("Other")
    default?:     option id,
    timeout_ms?,
    state:        open | answered { by, option_ids | text, at } | expired | withdrawn
  }
  ```

- **Sources.** On screen: the harness's choice dialog, detected per the
  AGENTS.md evidence rule (Claude Code's AskUserQuestion dialog with its
  numbered options and descriptions, Codex and opencode choice prompts). The
  `question_id` is derived from the agent, the turn cursor and a fingerprint of
  the prompt and options, so a redraw is the same question and a new dialog is
  a new one. The turn cursor component includes the event-log sequence at
  open (#1038), which is durable and only grows, so no new open, after a
  restart or a handoff or for a revisited tab, reuses an id that was closed.
  From a message: `msg.send` gains an optional `options` block, and
  the `question_id` is the message's correlation id.
- **Events.** `agent.question_opened`, `agent.question_answered` (with who
  answered and through which binding) and `agent.question_closed` (`expired`,
  or `withdrawn` when the dialog leaves the screen unanswered) are durable
  kinds, so the answer is part of the agent's event stream and a channel can
  update its rendering.
- **One answer verb.** `question.answer { question_id, option_ids | text }` is
  the only way in, from the CLI, MCP or a channel adapter (which needs the
  `reply` scope). The first valid answer wins. A second tap, a late poll answer
  or an answer to a question that has closed is refused with
  `question_closed` (ADR-0029 exit 13), so retries and duplicate deliveries are
  harmless.
- **Delivery into the agent.** For an on-screen dialog the server selects the
  option itself through the guarded injection under the #850 lock: before each
  key it re-checks that the same `question_id` is still on screen, it uses the
  harness's own selection keys (number or arrows, space to toggle for
  multi-select, Enter to confirm, the Other field for free text), and it
  confirms that the dialog closed. The result is an ADR-0029 outcome:
  `accepted`, `abandoned` (the dialog changed or the operator typed, nothing
  selected) or `unconfirmed`. For a message the answer becomes a `msg.reply`
  with `in_reply_to` set and the chosen option ids in a structured field beside
  the label text.
- **Answering locally.** `flk question list` shows open questions across
  agents with age, source and options; `flk question answer <id> <text>` or
  `--option <ID>` (repeatable for multi-select) calls `question.answer`. MCP
  has `flock_question_list` and `flock_question_answer`. The same idempotency
  applies, and a refusal says who answered first and when.
- **Every answer is recorded with its source** (`cli`, `mcp`, or
  `channel:<binding>/<external user>`) in `agent.question_answered`, so the
  event stream shows who decided what.
- **Defaults and timeouts are never applied to an on-screen dialog by flk.**
  For a message, the server picks `default` on timeout only when the sender
  asked for it (`auto_default: true`); otherwise the question expires.
- **Channel rendering** (the adapter's job, not specified in depth): a single
  choice renders as inline buttons, multi-select or several questions as a poll
  (`tg-poll` in `gerchowl/telegram-bot`), and `allow_other` as a free-text
  reply. The adapter maps Telegram's short callback data to `question_id` and
  option id itself, and edits the message to show the outcome once
  `agent.question_answered` or `agent.question_closed` arrives.
- **Reply path.** Every channel post of an open question carries its
  `question_id` (the adapter keeps the mapping from the chat's message id). A
  native reply-to-message on that post (text, an option button, or a poll
  answer) answers exactly that agent and question, with no target or id to
  type. A plain message in an agent's channel answers that agent's **oldest**
  open question, or becomes an ordinary inbound message (§8) when none is open.
  Either way it goes through `question.answer`, so a reply to a question that
  someone already answered locally is refused and the chat is told by whom.

### 9. Migration: what each hand-rolled piece becomes

| Today | Becomes | Slice |
| --- | --- | --- |
| `until [ "$(flk agent get …)" = idle ]; do sleep …` | `flk events wait --subject <agent> --phase finished,needs_human,gone` | E4b |
| `supervisor.py` status loop, `[fleet] …` typed lines | ADR-0027 digest (#961) triggered by `agent.phase_changed`, delivered via §5 | E3, #961 |
| `supervisor.py` CI watcher (watcher-s1 around `gh pr checks --watch`) | `pr.ci_completed` events | E7 |
| `landbatches.sh` `sleep 60` over `gh pr checks` | `flk events wait --kinds pr.ci_completed,pr.merged --subject pr:<n>` | E7 |
| `delegate wait` / `agent wait --status settled` client polling at 200 ms | thin wrappers over `events.read` with a server-side predicate | E4c |
| goal-s1 `bg-checkin` on harness background waits | registered waits make the agent `waiting_on_event`; ADR-0030 parks it | E5 |
| watch-nudge hook pointing at a skill agents rarely load | the hook points at `flk events wait` and Claude's `Monitor`, one story | E4b |
| briefs' "send fallback: send-text + send-keys Enter" | ADR-0029 | C4b, C4d |

Skills and docs to update in the same slices: the `architect` skill, the
`watch-job` skill and watch-nudge text, `flk agent wait --help`, the brief
template's Rules section, and `docs/next` CLI reference pages.

### 10. Slices

Each is ≤ ~400 diff lines and independently shippable. The cross-ADR order is
in ADR-0030 §8.

| # | Slice | Depends on | Replaces / amends | Skew (§3a) | Acceptance |
| --- | --- | --- | --- | --- | --- |
| E1a | `state` object with `turn`, `lifecycle`, `phase`, `held_ms` and per-axis `source` on `agent.get` / `agent.list`, from today's detectors only; `background` and `attention` present as `unknown` | — | #697 (first half), #922 follow-up | Fields only, no bump. Old client: prints them. New client, old server: no `state`, falls back to `agent_status`. Old mesh peer: remote rows `unknown` | Fixture screens per harness map to the documented `phase`; precedence table drift-locked |
| E1b | `background` axis: Claude footer counter, process-tree attribution for Codex / opencode (#663 A) | E1a | #663 A (predicate half) | Fields only | A Claude pane with a live background shell after its turn reads `turn: idle, background: running`, `phase: finished_with_background`; a Codex pane with a running child reads the same |
| E1c | `attention` axis: detector `blocked` becomes `needs_human{reason}`; `blocked{provider_limit}` for Claude and Codex quota screens (captured per the AGENTS.md evidence rule) | E1a | — | Fields only; `agent_status` keeps reporting `blocked` for old consumers | Captured screens for each dialog kind and each quota screen map to the right `attention` |
| E2 | Idle wake, self-compact and the guarded-submit settle gate key on `turn`, not on background | E1b | #697 (second half) | Server-internal, no wire change | Sandbox probe: Claude with a `run_in_background` loop ends its turn, an armed self-compact fires, a `needs_reply` message wakes it |
| E3 | Derived durable kinds `agent.phase_changed`, `agent.needs_human`, `agent.limit_reached`, `agent.gone` after the server settle window | E1a (E1b, E1c enrich it) | #663 A (event half) | New persisted kinds: first verify the last released server skips unknown kinds in the log, fix the reader if not | A status that flips back inside the window emits nothing; a real `PaneAgentStatusChanged` sequence produces exactly one `agent.phase_changed` |
| E4a | `events.read { after, kinds, subjects, limit, wait_ms }` on the socket: ring-then-disk replay, `gap`, server-side long-poll on the hub condvar | — | `EventsWait` schema stub | New method, protocol bump. Old server: exit 78 via `normalize` | Property test: random rotations and restarts, a reader resuming from any retained cursor sees every event once or more, never a silent gap; a reader past retention gets `gap` |
| E4b | `flk events read` / `flk events wait` and MCP `flock_events_read`; watch-nudge text and the `watch-job` skill point at them | E4a | — | Client side only; against an old server the verbs exit 78 | `flk events wait --kinds agent.phase_changed --subject <agent> --timeout 5s` returns the event, or exits 124 |
| E4c | `events.subscribe` takes `after`, starts at now when omitted, reports `lagged`; `delegate wait` and `agent wait --status settled` re-based on `events.read` | E4a | — | Changes `events.subscribe` defaults (changelog line); a new client detects support from the `protocol` number in the server's `ping` reply (the probe #868's `begin_cli` already makes and caches per invocation; MCP probes once per process): at or above the protocol E4a bumps to it uses `events.read`, below it keeps today's 200 ms polling | `delegate wait` makes one request per state change instead of 5 per second; existing wait exit codes unchanged |
| E5 | Wait registry: blocking and `park` waits recorded against the attested caller, `state.waiting`, `phase: waiting_on_event`, deadline expiry event | E1a, E4a | — | `wait.park` is state-changing: gated, exit 78 against an old server | A parked agent reads `waiting_on_event`; a `flk events wait` started under Claude `run_in_background` makes the pane `waiting_on_event`, not `finished_with_background` |
| E6 | Wake-follows-wait: a message or event that opens a registered wait wakes regardless of intent; idle wake for Codex and opencode where the inbox tool is configured | E2, E5 | ADR-0027 §3 wake gap (#961) | Server-internal. Mail relayed from an older peer is woken by the local rule, since the rule runs at the recipient | An architect parked on its delegates is woken by an `fyi` digest within one injection; an unparked one is not; a Codex agent with the flock MCP server is woken |
| E7 | PR poller adds check-run conclusions; `pr.ci_completed`, `pr.merged`, `pr.closed` kinds | — | #663 B | New persisted kinds (same reader check as E3) | One `pr.ci_completed` per head SHA with failed check names; no added per-worktree `gh` exec |
| E8 | Channel adapter contract: binding config, capability token, per-binding coalescing buffer, inbound `msg.send` / `msg.reply` attribution and scope checks; a reference adapter test double | E4a, ADR-0029 C1 | #1005 (contract half) | New state-changing methods, gated; an adapter refuses to start against a server without them | #1005's first-slice acceptance against a fake adapter; an unauthorised sender is refused and logged |
| E9a | Structured `question` payload on `needs_human{question}` from the Claude AskUserQuestion dialog (then Codex / opencode choice prompts); `options` on `msg.send`; `agent.question_opened` / `agent.question_closed` | E1c | #1005 (questions) | Fields and new kinds only; old clients print them; an old server ignores no new request because there is none yet | Captured AskUserQuestion screens (single, multi-select, with Other) yield the documented payload; a redraw keeps the `question_id`, a new dialog changes it |
| E9b | `question.answer`: first answer wins, `question_closed` afterwards (naming who answered); on-screen selection through the guarded injection with a per-key re-check; message answers as `msg.reply` with option ids; `agent.question_answered` with source; `flk question list` / `flk question answer`, MCP `flock_question_list` / `flock_question_answer` | E9a, ADR-0029 C1 | #1005 (questions) | New state-changing method, gated (exit 78 against an older server) | Fake Claude dialog: `flk question answer <id> --option 2` selects option 2 and the dialog closes (`accepted`); a second answer is refused and names the first answerer; a dialog that changed mid-selection is `abandoned` with nothing selected; `flk question list` shows open dialogs and unanswered `needs_reply` messages |
| E9c | Channel side of questions: binding receives `agent.question_*`; adapter test double renders buttons, poll and Other; reply-to-message routing by `question_id`, plain message to the oldest open question (or ordinary inbound mail when none); outcome edits back to the chat | E8, E9b | #1005 (questions) | Adapter refuses to enable questions against a server without `question.answer` | Fake adapter: a button answer reaches the agent once and a duplicate callback is refused; a reply-to-message on a question post answers that question even when the agent has two open; a plain message answers the oldest; a multi-select poll selects every chosen option |

## Consequences

- Supervisors stop deriving state. One server-side settle window replaces
  every hand-rolled false-idle filter.
- A consumer that was offline loses nothing the log still holds, and learns
  explicitly when it lost more.
- The server knows what every waiting agent is waiting for, which is what
  makes "parked" a deterministic verdict (ADR-0030).
- `fyi` gains a narrowly-scoped wake, tied to a wait the recipient chose.
- The event log grows by the derived kinds, bounded by the settle window
  (≈ 12–35 per host-hour at the measured load), well inside rotation.
- Scripts and skills that poll keep working but are no longer the documented
  way.

## Alternatives considered

- **Keep `agent_status` and add fields ad hoc** (`settled_with_background_shells`,
  `blocked_reason`, …). That is how today's vocabulary grew; each consumer still
  re-derives "finished". Rejected for one derived `phase` with a fixed
  precedence.
- **Exactly-once delivery with server-side acks per consumer.** Needs a ledger
  per consumer and still fails at the consumer's side effect. At-least-once
  with `seq` dedupe is simpler and sufficient.
- **A separate supervisor event store or a REST/SSE API as the core.** ADR-0005
  P5 and #663's own comment rule out a second system; SSE stays a possible
  adapter over `events.read`.
- **Coalesce in the log.** Loses facts a later reader needs (audit, lineage,
  reap evidence). Coalescing belongs to the recipient's buffer.
- **Harness-native waiting only (background tasks, `Monitor`).** It works for
  Claude and costs nothing in flk, but it is invisible to flk without §4's
  attribution, absent for harnesses without it, and it leaves goal-s1 guessing.
  Kept as one registration form, not the model.
- **Type digests or event bodies into the pane.** The hand-rolled failure
  (claude-config#18, #559). Rejected again.
- **Make every `fyi` wake.** Undoes ADR-0018's reason for intent and would
  interrupt agents with chatter they did not ask for.
- **Channels as a pane-typing client.** Clobbers drafts and races turns from
  the most delayed sender there is. Inbound channel text is mail.

## Risks

- **Detector gaps for `needs_human` and `provider_limit`** on Claude and Codex.
  E1a ships the axes with `unknown` where evidence is missing, and the
  detector work follows the evidence rule in AGENTS.md.
- **Process-tree background attribution** can mis-count shells that outlive
  their purpose (a forgotten `tail -f`). `finished_with_background` is a
  distinct phase precisely so that supervisors can treat it differently from
  `finished`; ADR-0030 gives it a deadline rather than trusting it.
- **Wake latency depends on the idle gates** (settle 2 s, fresh screen 2.5 s,
  operator quiet 15 s). Measured end-to-end it is sub-second once the gates
  pass; an operator typing in the architect's pane delays wakes by design.
- **Rotation under a busy fleet** could outrun a long-offline consumer; `gap`
  makes that visible, and `delegate.list` is the recovery read.

## Test strategy

- **Vocabulary:** table tests from captured screens per harness to `state`
  (AGENTS.md evidence rule: capture with `flk pane read --source recent`), plus
  a drift-lock test over the `phase` precedence table.
- **Events:** a property test over random event sequences, rotations,
  restarts and torn tails asserting the §3 guarantee; a test that starts at a
  real `PaneAgentStatusChanged` and ends at `agent.phase_changed`.
- **Waiting and wake:** the sandbox runbook used for this ADR's measurements
  (isolated flk, throwaway Claude, `msg send` → turn start timing) is
  `tests/probes/agent_wake.py`; E2 and E6 run it against the build before and
  after the slice.
- **Channels:** a fake adapter over the socket exercising cursor resume,
  coalescing under a rate limit, scope refusal and inbound attribution.

## Owner decisions

Decided by the operator on 2026-10-11 (PR #1006): every item below was
accepted as recommended. The rejected alternative is kept for the record.

1. **Wake follows the wait (§5).** Decided: amend ADR-0027 §3 through this
   rule, so the digest stays `fyi` and wakes only an architect that parked.
   Rejected: give the digest a waking intent, which would interrupt
   architects that did not ask to be woken.
2. **Injection priority (§5).** Decided: operator input first, then
   compaction, then a wake, then a goal continuation, which keeps #850 and
   ADR-0027 §3 intact. Rejected: compaction above the operator, which would
   be the only write allowed to override a human and is not proposed.
3. **Defaults (§6).** Decided: 20 s settle (5 s already removes 48 % of raw
   transitions, 20 s removes 54 % and also covers compaction and quota
   screens), a 12-line digest, and 540 s maximum read wait.
4. **Channels (§8).** Decided: inbound text defaults to `needs_reply` (it
   wakes, since a person sent it on purpose); adapters authenticate with a
   per-binding capability token issued by the server; one statically named
   node per binding, no failover.
5. **Channel adapter placement.** Decided: out of process (the
   telegram-bot daemon over the socket) now; embedding later must keep §8's
   contract.
6. **Open questions from channels (§8a), added by the owner.** Decided: a
   structured `question` payload with options sourced from the harness's choice
   dialog or a message's `options`; channel rendering as buttons, poll and
   free-text Other; a reply path (reply-to-message answers that question, a
   plain message answers the oldest open one); `flk question list` / `answer`
   and MCP equivalents; first answer wins, every answer recorded with its
   source. Slices E9a–E9c.
