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
- Amendment (2026-10-15, #1479, clarified by #1519): §1a records accepted
  Codex lifecycle authority. The owner accepted option 1, the bounded rollout
  format/version policy, on 2026-10-16. Approval of #1615 satisfies the docs
  review gate. S1 supplies version eligibility; runtime authority follows in
  #1661–#1664 and is not implemented by this slice.
- Proposed amendment (2026-10-15, #1574): §1b specifies detection-sample
  receipts, output cursors and legacy revision deprecation. Owner decision
  required; this subsection is not accepted or implemented.
- Amendment (2026-10-16, #1052): §5a records the architect's decision that
  input `Ok` means completed PTY write. Error/deadline/capability details are
  proposed for review, with **Owner decision required** items named below.
  No implementation is claimed by this amendment.
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
**turn** axis on 2026-10-15 and accepted the option-1 rollout format/version
policy on 2026-10-16. The source and continuity contract below specifies the
planned adapter. The docs review gate is satisfied by approval of #1615.
Version eligibility is S1; binding, reading and authority remain #1661–#1664.
It amends §1's source table, not the Claude harness or its Stop hook.
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

The following reducer rules apply only after binding validation succeeds.
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
precedence. Scope lifecycle trust to the pane's **launch generation**, as in the
binding key above: track whether that generation has held a successfully validated
binding. An explicit retirement through a known launch or resume ends that scope
and resets trust for the new generation. A new SessionStart claim, binding
generation or rebind request alone does not reset it.
Pending validation yields unknown. If binding validation fails before lifecycle
has been trusted in the current launch generation, reject the claim and its
queued callbacks,
publish `unavailable: binding_validation_failed` with the failure reason, and
return to the existing screen fallback. A Working screen therefore means working;
unclassifiable evidence remains unknown. This fallback does not manufacture idle
or require another SessionStart before screen-based submission can be eligible.
Without a known retirement, failed revalidation or unexplained loss of a
currently-bound validated authority yields unknown; a rejected claim cannot erase
that generation's trust history to regain fallback. This same split applies to
all binding-validation and attribution failures below. Only a freshly validated
binding can establish lifecycle authority again.

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
or mismatched metadata fail binding validation: use screen fallback if the current
launch generation has not trusted lifecycle, or unknown after validation without
an explicit known retirement.

Known launch/resume generation changes are observable to flk before the hook:
retire the old binding, discard its queued callbacks and reset lifecycle trust for
the new launch generation, then return to `unavailable: awaiting_first_turn` and
screen fallback until the next SessionStart. If that new claim fails validation,
use diagnosed screen fallback: authority in the retired generation cannot strand
the new launch at unknown.

**Future work, not a current producer (#1479):** an attributed runtime confirmation
of an in-TUI thread replacement would permit the same retirement. Today's Codex
integration [installs only SessionStart](../../src/integration/mod.rs#L1831-L1838);
it has no real producer of immediate clear/new/resume/fork confirmation. Typed
command text, an idle composer, a label and a timeout are not confirmation.
A model test of this future contract must not be presented as a working runtime
replacement path.

**Observability limit and false idle:** after an in-TUI `/clear`, `/new`, `/resume`
or `/fork` without confirmation, keep the old binding until the next SessionStart
atomically replaces it. If the old binding is idle, it still wins over the new
thread's first visible Working frame during the deferred-hook window. If the hook
report is lost, for example during a socket restart or hook-command timeout,
this false idle can persist for the whole new thread. Do not claim that an early
retirement or pre-hook fallback was observed.

Publish the lifecycle/screen disagreement diagnostic for this idle-versus-Working
case, including the old binding generation/thread ID, last lifecycle boundary,
verified file identity/cursor, latest successful EOF observation and the classified
screen evidence. Publish it when disagreement is observed and keep the current
diagnostic available while it persists, including when no new hook ever arrives.
These facts expose possible stale attribution; they do not prove replacement.

**Retirement decision:** Working chrome plus an unchanged bound file is
insufficient to retire a binding. It also occurs after a real completion with
stale Working chrome, and an empty read is normal. Preserve the binding, its
lifecycle idle and the diagnostic in both cases. Neither elapsed time nor repeated
EOF checks may turn this into fallback or a new thread claim. A later validated
SessionStart replaces the generation; an operator-requested rebind must validate
attribution before publishing another lifecycle decision. Test both indistinguishable
cases and the lost-report case explicitly. Unexplained loss of authority trusted
in the current launch generation still yields unknown, rather than bootstrap
fallback.

For a resume, record the pre-launch file identity, verified prefix and complete
byte offset, and buffer appended records until binding validation succeeds.
At the deferred SessionStart, use the new writer's attributable start after that
snapshot, not an inherited unfinished turn or old completion, as the current
boundary. A copied snapshot also needs destination verification and local
ownership. Missing/bad paths or metadata and ambiguous writers fail validation
under the current-generation untrusted versus validated rule above. Discontinuity
after validation yields unknown. No empty-history or pre-launch completion
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
If the delimiter/owner cannot be verified before lifecycle trust in the current
launch generation, reject the claim and use screen fallback with
`binding_validation_failed` and the attribution reason. If this failure instead
invalidates authority already validated in this generation without known
retirement, yield unknown. Neither branch treats imported starts or completions
as the child's turn.

A reducer state may transfer only with proven continuity of the same local
writer, thread, file and cursor; equal thread IDs alone do not suffice. In
particular, SessionStart `compact` during a running turn from a validated binding
preserves working when that continuity is proven. Otherwise it yields unknown with
`codex_compaction_continuity_unproven`; compaction is not a fresh-session idle
boundary. Delayed records from a revoked binding are discarded, including
within one launch after `/new` or `/clear`.

A move (#1500) revokes the source pane's binding before the destination claims a
new launch generation. Rebind to the destination's **local** writer and Codex
home; copied history remains history. Replace the pane/socket integration
environment with the destination's route. The old hibernated record cannot
consume destination events. Reject a report routed only by an old name/alias.
Known duplicate live ownership of a thread fails attribution. Before lifecycle
has been trusted in the destination's launch generation, reject that claim and
use diagnosed screen fallback. If it invalidates a currently validated binding
without known retirement, yield unknown until resolved. Never use
last-callback-wins. These are the adapter's requirements
on #1500's move transaction; name lookup, move UI and narrow resumed panes
remain outside this amendment. A rollout copy alone grants no authority.

#### Accepted: format dependency and version policy

**Accepted by the owner, 2026-10-16
([#1479, option 1](https://github.com/gerchowl/flock/issues/1479#issuecomment-6203978023)).**
Rollout JSONL is an internal storage format, not a promised stable integration
API. The owner accepted the bounded compatibility policy below for lifecycle
authority; this decision authorizes implementation, not a claim that it has
shipped.

The **accepted policy** is:

- Record `session_meta.cli_version` for every binding. Initially the tested
  range contains only **0.160.1**; widen it only with explicit compatibility
  evidence and review, never an assumed semver-compatible range. Resume preserves
  creation metadata, so also verify the version of the actual writer executable
  resolved for the launch; old metadata alone cannot authorize a newer writer.
  Obtain the actual writer version by running that resolved executable once
  with `--version`, with one 5000 ms monotonic deadline shared by process exit
  and stdout/stderr reads, and no retry. This applies ADR-0037's principle
  (missing evidence fails, never succeeds), not its test-binary sizing rules.
  Cap each captured pipe at 4 KiB and fail on overflow.
  Parse at most 256 bytes of UTF-8 as exactly `codex-cli MAJOR.MINOR.PATCH`,
  optionally ending in one LF or CRLF. Each component is nonempty canonical
  decimal with no leading zero except zero itself; reject extra lines/whitespace,
  prerelease/build suffixes and banners as missing version evidence. A syntactically
  valid version outside the tested range is unsupported. Spawn/read failure,
  nonzero exit, unparseable output or exceeding
  the ceiling counts as a missing version: diagnose
  `unsupported_codex_version` and select screen fallback as below. Keep the
  direct child unreaped while collecting both pipes, so its group id cannot
  be reused. On timeout or read failure, terminate that owned process group,
  including a launcher descendant holding stdout open, and reap the direct
  child. Once reaped with both pipes at EOF, a strict deadline failure must
  return without signaling the relinquished group id. Do not wait beyond the original
  deadline for inherited pipe EOF or retry the probe. Both metadata and writer
  versions must be covered by the tested compatibility declaration. See the pinned
  [new versus resumed recorder setup](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/rollout/src/recorder.rs#L935-L994).
- A version in that range uses the lifecycle adapter. Any other or missing
  version marks this source unavailable with `unsupported_codex_version` and
  selects screen fallback, with the reason exposed. A version mismatch itself
  never means idle: before fresh classifiable screen evidence, state is unknown;
  a visible Working screen means working. Any later idle must come from actual
  fallback evidence, not an unsupported-version default or copied completion.
- CI pins a declared tested-version constant, initially **0.160.1**, and
  recorded/scripted rollout fixtures checked against it, rather than installing
  or launching a real Codex binary. Compatibility tests check fixture metadata
  and scripted writer `--version` responses against that constant and cover
  unsupported/missing versions. CI fails when the pin changes without updating
  the declared range and compatibility tests. This requirement belongs to the
  implementation. S1 supplies `codex_version::TESTED_VERSION` and the
  discovered `scripts/test_codex_version_pin.py` check against rollout metadata
  and writer-output fixtures; later slices consume that eligibility. No live
  Codex binary or model is launched in CI for this pin.

An alternative is Codex's supported, versioned **app-server** interface, whose
v2 notifications include `turn/started` and `turn/completed`; see the pinned
[versioned protocol modules](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/app-server-protocol/src/protocol/mod.rs#L10-L13)
and [notifications](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/app-server-protocol/src/protocol/common.rs#L1943-L1947).
It is not selected by the accepted policy because flk currently runs the Codex
TUI in a pane; owning or attaching an app-server connection for that same thread
needs a separate transport/ownership design. It cannot be replaced by launching a
second independent session and observing its events. The owner selected the
bounded rollout compatibility policy; an app-server interface change remains
a separate transport/ownership design.

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
| No binding yet, or old binding retired on a known launch/resume change (future: attributed in-TUI replacement confirmation) | Screen fallback with `awaiting_first_turn`; no lifecycle claim until SessionStart. |
| Binding validation failed before this launch generation held a validated lifecycle binding, including after known launch/resume retirement | Reject the claim; screen fallback with `binding_validation_failed`, its reason and selected source. Screen Working stays working; unclassifiable evidence stays unknown. |
| Claimed and still validating, or current-generation validated source lost, unreadable, discontinuous or ambiguous without known retirement | Unknown with source and reason; hook/screen cannot conceal source loss. |
| Validated old binding idle, visible Working, file unchanged, no replacement report | Retain lifecycle idle and binding; publish the disagreement diagnostic. This covers the new-thread false-idle window and stale Working chrome. |
| Explicitly unavailable (absent/disabled adapter, known disabled persistence, or version excluded by the accepted policy) | Hook then screen fallback for absent/disabled adapters; the accepted unsupported-version policy selects screen. Publish the reason and selected source; unclassifiable evidence stays unknown. |

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
After a validated source is lost it cannot quietly become unavailable for fallback.
Failed initial binding validation in an untrusted launch generation is the
bootstrap exception above. Known launch/resume retirement explicitly ends the old
trust scope and returns the new generation to awaiting-first-turn fallback;
rejected claims without that retirement cannot hide source loss. Confirmed in-TUI
replacement remains future work.
Explicit version/config unavailability is diagnosed at binding, under the accepted
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
is accepted as a residual limitation of the policy above. **Residual risks:**
if the writer remains alive but its end record is lost, the last start holds
working indefinitely. Publish
a diagnostic when lifecycle and screen disagree in either direction, including
the false-idle new-thread window and lost SessionStart report described above.
Expose both observations and the active boundary so an operator can request a
rebind. Neither the diagnostic nor requesting a rebind changes state on its own: rebind must validate
and reconstruct a trustworthy current boundary before publishing a new decision.
A resume of a file whose last turn never ended is a second residual risk:
before SessionStart, screen fallback shows the current prompt as idle when the
existing detector classifies it idle (or working/unknown from its actual screen
evidence), rather than inheriting the old unmatched start. Submission remains
eligible under today's screen/composer gates, so that history cannot deadlock
bootstrap. On binding, the verified new writer's start establishes working;
if attribution fails before validation in this new launch generation, reject the
candidate and use diagnosed screen fallback. If ambiguity invalidates a currently
validated authority without known retirement, yield unknown. Revalidation must
not conceal an observed gap in that authority; resynchronize and require a
trustworthy current boundary before publishing state.

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
   A claimed binding still validating yields unknown. Failed first binding
   validation before lifecycle trust in this launch generation rejects the claim
   and returns to screen:
   test Working, idle and unclassifiable screens independently, plus submission
   eligibility with no later SessionStart. After a validated binding, a rejected
   claim without known retirement yields unknown even if that new claim was never
   validated; changing binding generations must not erase the current launch's
   trust. Writer exit yields unknown;
   PID reuse yields unknown; a supervised wedged/dead reader yields unknown;
   replacement or truncation yields unknown. Bounded integration waits must name
   a missing condition rather than hang. Test idle wake, self-compact, guarded
   submit, mail delivery and idle waits on quiet known idle: each remains eligible
   under its existing gates. For source-loss unknown, each follows the table above.
3. After successful binding validation, exercise each reducer case independently,
   with one expected turn state:

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
   - Known resume after lifecycle trust: explicitly retire the old binding,
     advance the launch generation and reset trust. If the new SessionStart fails
     validation, publish `binding_validation_failed` and use screen fallback,
     not unknown. Test Working, idle and unclassifiable screen evidence; no later
     SessionStart is required to leave the rejected claim. Old callbacks cannot
     affect the new generation.
   - Counter-case: a validated binding followed by a rejected SessionStart claim
     without a known retirement keeps the same launch-generation trust scope and
     yields unknown. A binding-generation change alone cannot authorize fallback.
   - Future model-only contract, with no real producer today: an attributed
     in-TUI replacement confirmation retires the old binding, enters screen
     fallback, then the next turn's SessionStart binds as working. Delayed old
     callbacks cannot affect fallback or the new turn. Track the producer in
     #1479; do not claim this simulated path is runtime integration evidence.
   - For each in-TUI `/clear`, `/new`, `/resume` and `/fork` without confirmation,
     start from an old validated idle binding. Feed a new Working screen while
     the bound file remains unchanged: retain old lifecycle idle and generation,
     and publish the diagnostic with both observations and the verified cursor.
     Deliver the deferred SessionStart and validate the new start: only then
     retire the old generation and report working for the new binding.
   - Lose that SessionStart report while the writer remains alive. Advance the
     supplied clock and repeat successful EOF checks: false idle and the
     diagnostic persist; no silence-based retirement or guessed rebind occurs.
     Replay the same observations after a real completion with stale Working
     chrome: the same binding-retention decision must hold.
   - Fork: imported parent completion precedes the child's owned
     thread_settings_applied delimiter; only the child's subsequent start
     establishes working. Cover missing and ambiguous delimiters separately in
     both trust scopes: in an untrusted launch generation (including after known
     retirement), reject the claim and queued callbacks, publish
     `binding_validation_failed` with the attribution reason and use screen
     fallback. Test Working, idle and unclassifiable screens plus submission
     eligibility without another SessionStart. After authority validated in the
     current generation, the same failures without known retirement yield unknown;
     changing the binding generation cannot regain fallback. Imported parent
     boundaries cannot establish child state in either branch.

   Same-name/cwd panes with different thread IDs remain isolated: starting only
   pane A yields working for A and idle for an already completed B. Each claimed
   but still-validating SessionStart binding yields unknown. Failed initial
   validation uses screen fallback when the current launch generation is untrusted,
   including after known launch/resume retirement. Rejection after current-generation
   trust without known retirement yields unknown. Compaction during an
   active turn with proven writer/thread/file/cursor continuity stays working;
   unproven continuity yields unknown with
   `codex_compaction_continuity_unproven`. A stale callback while the rebound turn
   runs leaves working. After a move the old pane remains hibernated; the
   destination starts in screen fallback until its own deferred hook binds.
   Duplicate live writers and a mismatched transcript_path/meta pair fail binding
   validation: untrusted launch generations use screen fallback; current-generation
   validated authority without known retirement yields unknown. Another thread's
   end through fork/revert metadata leaves the current active turn working. Private
   guardian child completions, including their deferred dispatch, cannot complete
   the parent turn.
5. Lifecycle start against hook/screen idle yields working. Lifecycle complete
   against stale Working chrome yields idle. A screen approval prompt against
   lifecycle idle yields `phase: needs_human` with turn unknown; clearing that
   prompt restores idle from the retained completion. Validated source loss against
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

### 1b. Proposed: detection-sample receipts and output cursors (#1574)

**Status: Proposed — Owner decision required.** This subsection is a design
proposal, not part of this ADR's accepted implementation contract yet. Approval
and implementation are tracked by [#1574](https://github.com/gerchowl/flock/issues/1574).
No product behavior changes with this amendment. The existing `turn_cursor`,
lifecycle authority (§1a), and durable event `seq` (§3) retain their meanings.

#### Problem and historical evidence

The consecutive hollow-spinner replay in #1570 can match text already on screen
and read the old Working status before a detector samples the redraw. A paint
acknowledgement is insufficient: a detector pass already in flight can later
publish a result computed from an older snapshot. An unchanged screen also needs
an acknowledgement, so a content-change counter alone cannot solve the problem.

`git log -S 'revision'` traces the wire field to
[`fd1e3b8b`](https://github.com/gerchowl/flock/commit/fd1e3b8b056b2937fc0a5c90bf482e01d269f6b2)
(2026-03-29, initial socket API). That commit introduced `PaneInfo.revision`,
`PaneReadResult.revision`, `OutputMatched.revision`, and
`PaneOutputChanged { min_revision }`; its live replies in `src/app/mod.rs`
already returned zero. The read/wait/filter relationship suggests an output
snapshot revision, not a detector-sample counter. This is an inference from the
schema and its consumers, rather than a recovered explicit design decision.

`86adf883` (2026-05-17) forwarded pane revision through the new agent facade;
`a36da363` extracted `TerminalState.revision`, initialized to zero, and
`866aaf74` (2026-05-21) retained zero in the split read handlers. Searches with
`git log -S` and `git log -G` for incrementing writers, plus inspection of these
commits, found no implemented advancing writer. Today
`src/app/api/panes.rs` and `src/app/api/agents.rs` hardcode read revision zero;
`src/app/agents.rs` forwards the terminal field. This is an unfinished output
revision contract, with no evidence here of a recently removed detection writer.

`turn_cursor` counts execution/state/working transitions, not observations.
`emit_pane_state_update` emits a status event only when status or presentation
changes. Neither proves that a stable Working screen was sampled. The current
screen detector reads the entire current terminal screen: the 24-row constant
is a geometry fallback, not a guaranteed region in which a visible header is
ignored (`src/pane/terminal.rs::ghostty_detection_text`).

#### Recommended signal and its causal boundary

Use two distinct opaque cursors, scoped to one local pane runtime incarnation:

| Value | Advances when | Does not mean |
| --- | --- | --- |
| `output_cursor` | The parser accepts a nonempty PTY byte batch, including identical redraw bytes, or a resize/restore/style mutation changes inputs available to detection. | A detection pass completed, a turn ended, or an operator viewport changed. |
| `detection_cursor` | A completed pass captures a snapshot and its result is applied to this pane's server state, including passes over unchanged content. | Content changed, state changed, or lifecycle authority changed. |

Each cursor contains a version, runtime observation epoch, and unsigned 64-bit
counter; the API represents it as an opaque string, not a JSON number susceptible
to precision loss. Illustrative forms are `o1:<epoch>:<counter>` and
`d1:<epoch>:<counter>`. Clients pass these strings back, not parse or compare
their internal fields. Counter zero means no output/pass yet; it is not a
completed sample. Counter overflow rotates the observation epoch and invalidates
old cursors, never wraps or silently saturates.

The scope is the pane's owned terminal runtime, not an agent label, session
binding or workspace. Renaming an agent or moving its pane within the same
runtime does not reset the cursors. An agent-facing wait resolves and pins the
agent identity and pane incarnation at registration; it cannot follow a reused
name to another agent. A sample does not validate or retire a lifecycle binding.

Capture detection text, relevant styles/geometry and `output_cursor` under the
same parser lock. Advance the output counter at the parser commit boundary,
not at socket accept, queued input, or a later API read. Operator scrolling does
not advance it: detection remains anchored to the bottom, independent of the
user-visible viewport. The cursor describes parser progress, not equality of
all possible transformed `pane.read` outputs. Read handlers return the output
cursor captured with their returned text; they must not fetch a newer cursor
after releasing the snapshot lock.

After classification, send an internal observation carrying the captured output
cursor, outcome and raw detector facts. Each producer has an ordered capture
ordinal within its epoch; discard stale-epoch or out-of-order observations before
they mutate state. Increment `detection_cursor` only after the app applies that
observation and arbitrates effective state. Publish one immutable receipt:

- detection cursor and the output cursor actually sampled;
- classified agent/terminal identity and the existing turn cursor;
- outcome: `classified`, `skipped`, `unidentified`, or `process_exited`;
- raw screen state when classified, effective agent status, and its authority.

The receipt is one committed observation, not a mixture of a new cursor and old
status. Blank or intentionally skipped content still completes a pass and
advances the detection cursor with its honest outcome; it cannot certify that a
retained Working state was freshly classified. A pass that fails before capture
or application does not advance anything. Failure/worker death is surfaced to
waiters. Both ordinary and adopted-pane detection loops must use this boundary.
No full screen text is copied into receipts or emitted in telemetry.

#### Runtime lifetime, restart and handoff

**Owner decision required — recommendation: invalidate rather than persist.**
Mint a fresh cryptographically random 128-bit observation epoch for each owned
runtime at server startup, pane respawn, and ownership transfer during live
handoff. Epochs must be unique across process lifetimes, not just within one
process: never derive them from a process-local counter, pane id or timestamp
alone, and never restore an old epoch from persisted state. Fail runtime
observation initialization if secure randomness is unavailable; do not fall back
to a reusable value. Preserve normal pane/agent identity as today, but reset
these two counters to zero in the new epoch. Do not restore a receipt as proof
that the new owner has sampled anything. The importer must capture and apply its own first observation. Old-owner late messages are
rejected by epoch, even if their numeric counters are larger.

This deliberately does not promise a globally increasing integer across runtime
replacement. It promises monotonic comparison within an incarnation and explicit
invalidation across incarnations. Pending waits fail on handoff/restart/respawn,
producer loss or identity replacement; a caller must re-read and explicitly
register a new wait. No silent cursor rebasing or automatic successful resume.
The observation counters are ephemeral: no per-pass session-file writes, fsync,
or durable event replay. This is independent of ADR-0005's restart-stable `seq`.

#### Reads, waits and mixed versions

Add optional `output_cursor`, `detection_cursor` and `last_detection_sample` to
local `pane.get` / `agent.get` and their corresponding local list records.
Before the first applied pass, the detection cursor is zero and the last sample
is absent. Add `output_cursor` to `pane.read` / `agent.read` and output-match
replies, captured atomically with the returned text. Remote/fleet summaries
omit observation fields; absence means unsupported, never a zero sample.
Advertise this contract with an additive `detection_samples_v1` capability.

Recommend a new read-only method, `pane.wait_for_sample`, and CLI
`flk pane wait-for-sample <pane> --after-sample <D> --min-output <O>
--require-classified --timeout <duration>`. An `agent wait-for-sample` alias
may resolve an agent and call the same method. Parameters are a pinned pane identity, mandatory
`after_sample`, optional `min_output`, optional `require_classified` (default
false), and mandatory integer `timeout_ms` in `1..=540000`. Reject zero or a
value above 540000 with `invalid_sample_timeout` / CLI `USAGE` before admission;
never clamp it. The maximum is §6's 540 s read-wait ceiling, leaving 60 s below
the 600 s MCP cap. Tests retain a smaller per-stage failure ceiling (for example
5000 ms), within their ADR-0037 binary termination ceiling; the API maximum is
not permission to widen a test deadline. The success predicate is both
`sample > D` and, when supplied, `sampled_output >= O`, all within the same
epoch, plus `outcome == classified` when `require_classified` is true. Already
committed matching receipts may return at once. Return the receipt and its
effective status together. With the flag false, skipped/unidentified
observations can return as such; they cannot be read as a Working assertion.

This is a registered, server-woken bounded wait (§5, ADR-0037 §1), with no
polling loop in the client or server. It owns admission and wakeup independently
of `WaitRegistry`: that registry records nothing for callers outside agent panes,
has no held-read admission cap, and its predicate wakeup awaits E6 (#1032).
Sample waits do not depend on E6 or treat registration as a wake mechanism.

Recommend at most **8 pending sample waits per pane incarnation and 256 globally
per server**, counting every caller, including operator CLI, tests and unattested
socket clients. Validate subject, epoch, parameters and producer liveness first.
An already matching receipt returns immediately without consuming a pending slot.
Otherwise atomically check both counts and register, or refuse immediately with
`sample_wait_limit_exceeded` / CLI `REFUSED` if either cap would be exceeded.
Never queue for admission or allocate an unbounded list of blocked requests.
Registration and a recheck of the latest receipt, epoch and liveness share the
receipt-commit synchronization boundary, preventing a lost wakeup. A slot owns
both counts and releases them on every completion, timeout, cancellation,
disconnect, producer loss, subject removal or epoch replacement.

Use a dedicated per-pane notification fired by the detection pass's app-side
receipt commit, after classification and effective-state arbitration, including
unchanged passes. Notify all registered waiters for that pane, not just one. The waiter suspends on that notification, disconnect/liveness
signals and its single monotonic deadline. Producer death, pane removal and epoch
replacement signal pending waiters directly rather than waiting for another
sample. Wakeups only cause a predicate/liveness recheck; they cannot satisfy the
wait themselves. Arm notification and recheck atomically with registration (and
on resuspension), so a commit between checks cannot be lost. There is no periodic
receipt scan, timer-based retry or client read loop. A live silent producer or a
stream of nonmatching outcomes fails at the original deadline; neither EOF nor
missing observations implies success.

Proposed failures are typed `observation_reset` / `subject_replaced` (CLI
`REFUSED`), `detection_producer_gone` / removed pane (`GONE`), `timeout`
(`TIMEOUT`), malformed cursors (`USAGE`), and unsupported capability/method
(`PROTOCOL_GAP`). Register any new codes in `src/api/error_code.rs`; reuse
ADR-0029's named exits, not new numeric literals. An unavailable detector is
not a permanent quiet wait. Epoch mismatch fails even if the new counter is
numerically greater. An agent identity change cannot retarget an open wait.

**Owner decision required — recommendation: dedicated method, no new global
sample event.** Do not extend an old method with silently ignorable wait fields:
an old server might ignore them and return stale success. A new method is
refused by old servers and normalized as `server_version_gap` / exit 78 (§3a).
New clients must fail closed if capability or required response fields are
missing; no fallback to text matches, sleep or ordinary status waits. Old clients
ignore additive response fields. Follow §3a's one protocol bump per release for
the new request shape, checking the latest released tag at implementation time.

There is no `DetectionSampled` durable or global-ring event. Updating sample
metadata does not change `state_seq`, working entries, turn cursor, derived
phase events, seen state, settle/dwell timestamps, or render dirtiness. Repeated
samples must not restart a settle window or manufacture progress for stall checks.
Only registered local sample waiters are notified. The dedicated per-pane
notification is neither a second event broker nor a replayable stream. A
disconnected waiter must read current cursors and register again, not resume
from hub `seq`. An attested agent caller may additionally use the existing wait
registry's attribution/audit rules for registration/closure; this optional record
provides neither admission nor wakeup. Unattested callers have the same sample
wait limits and liveness guarantees without an agent-state record.

#### Settle the legacy `revision` contract

**Owner decision required — recommendation: deprecate the legacy field.**
Keep serializing existing numeric `revision` fields as zero on every old response
shape. This explicitly includes `EventData::PaneOutputChanged.revision`
(`src/api/schema.rs`), alongside pane/agent info, read and output-match fields;
retain that legacy event field/schema without adding a new producer for it. Do
not reinterpret these fields as detection samples or suddenly mix a new writer
with old zero read handlers. Add the real parser-progress writer only for
`output_cursor`, with the snapshot semantics above. New clients use the two new
cursors; they must not infer capability from a nonzero legacy integer.

Document the deprecation in unreleased API docs/changelog when #1574 implements
it. Retain the field, wire type, legacy event schemas and old log readability
through compatible releases; actual removal requires a separately approved major
wire change. No removal is authorized by this proposal. Legacy
`PaneOutputChanged.min_revision` is not a detection barrier and has no live
producer today. A capable server must reject requests relying on that unfinished
contract with `unsupported_legacy_revision` / `PROTOCOL_GAP`, rather than silently
translate it or leave it waiting as if it could be satisfied. This named behavior
change also needs the owner's approval and a release note. Old clients that just
read revision continue to receive zero; old filter users get a visible refusal.
Mixed-version and downgrade tests must use released clients/servers, not assume
that unknown fields or error codes are accepted. No sample events enter old logs.

#### Cost and implementation acceptance

At the current 300 ms detection cadence, N owned panes produce about `3.33 × N`
receipts/second, even with unchanged content. For 100 panes that is about 333
internal applications/second. This is an analytic rate, not a measured benchmark.
Reuse each pass's existing snapshot and classification; add counters and a small
fixed-size latest receipt, not a new screen parse/hash or full-state copy. Keep
metadata below 1 KiB per pane as an initial budget, roughly 100 KiB for 100 panes,
plus the separately capped sample-wait entries (at most 8 per pane and 256
server-wide). The implementation must measure and report bytes per pending
wait and total memory at the global cap. No history grows with uptime.
Output revisions reuse the parser lock already held for writes/snapshots.

No waiters means no sample network traffic, disk I/O, hub sequences, diagnostic
line per pass or UI redraw. Wake only that pane's registered waiters, and return
one receipt per satisfied request. Enforce the dedicated admission caps above,
release slots on every termination path, and do not queue every missed sample
for slow clients. A later receipt covering the minimum output suffices;
this is latest-observation progress, not an every-sample delivery guarantee.

**Owner decision required — recommendation:** approve these lifetime/exposure/
legacy-compatibility choices and a benchmark gate of less than 1% of one core
additional idle CPU for 100 owned panes, plus the memory bound above, on a
specified reference environment. Record before/after measurements in the
implementation PR; the number is a proposed gate, not a performance claim.
If it fails, revise this proposal rather than dropping unchanged-pass sampling
or adding unbounded coalescing that hides the requested causal boundary.

Implementation and all cases below belong to #1574. This docs PR neither adds
those tests nor claims their success. The present baseline lacks the capability,
method, receipts and advancing output cursor, so the new contract checks must
fail before implementation. Preserve these failures once as regression evidence.

| Acceptance case | Required evidence / negative control |
| --- | --- |
| Unchanged passes | Two captured identical screens advance the sample counter while raw/effective Working, turn cursor and durable `seq` stay unchanged. Current API has no sample field/method. |
| Parser output progress | Identical PTY bytes advance output cursor although detection text is identical; read text and cursor describe the same locked snapshot. Current read revision remains zero. |
| Causal race | Hold a classification of output R, paint R+1, then release the old result. `min_output=R+1` must remain pending despite a newer sample number, and only open after a pass covering R+1. |
| Application boundary | Hold app-side receipt application after capture; no read/wait may advertise the sample as applied yet. Release it and receive one coherent receipt. |
| Snapshot race / stale producer | Write during capture, reorder queued observations, and deliver an old owner's receipt after handoff. Never attach a newer output cursor to old text or apply stale results. |
| #1570 redraw | Save prior detection cursor D. With input echo off, issue marker plus the complete identical Working redraw in **one PTY write**. An output match on the unique marker **and redraw tail** in `recent` returns the atomically captured output cursor O. Prove marker absent from `detection` and detection text equal to the original. Call `pane.wait_for_sample` with `after_sample=D`, `min_output=O`, `require_classified=true` and a bounded timeout, then assert the receipt is classified Working. No sleep is needed; the output match acknowledges the entire redraw and the sample wait acknowledges its classification. Neither a status read nor marker alone proves completion. |
| Marker isolation | Current-screen detection includes all visible rows. The fixture must prove its marker is in scrollback and absent from the detector snapshot; a header assumed to be outside 24 rows is not sufficient. |
| Real turn end | After a real completed/aborted-turn source, normal authority rules still settle; samples do not revive Working or retire a binding (§1a). Test raw Working with authoritative lifecycle idle separately. |
| Skipped and unidentified | Blank/skipped/unidentified passes acknowledge their honest outcomes without presenting retained state as fresh classified evidence. |
| Lifecycle boundaries | Respawn, restart and live handoff issue a new epoch, invalidate open waits, reject old cursors/late receipts and require a fresh applied pass. Rename/move within the runtime does not reset it. |
| Pre-restart cursor | Save D and O before stopping the server, restart it with counters reset, then submit a wait using those saved cursors. Return typed `observation_reset` / `REFUSED` immediately, even if new counters later exceed the old ones; never return satisfied. Repeat for respawn and handoff, and assert epochs differ across process lifetimes. |
| Admission / timeout bounds | Keep 8 unmatched waits open on one pane; the ninth returns `sample_wait_limit_exceeded` / `REFUSED`. Reach 256 across distinct panes; the next returns the same refusal. Repeat with unattested operator/test callers. Verify each termination path releases both counts; immediate matches consume no slot. Zero, 540001 and `u64::MAX` timeouts return `invalid_sample_timeout` / `USAGE`; 540000 is admitted when capacity exists, and tests use their smaller failure ceiling. |
| Liveness / lost wake | Producer dies, peer/subject disappears, a live producer stays silent, caller disconnects, and commit races registration. Each fails/cancels within its existing ceiling or succeeds on an actual matching receipt; never hangs or misses a committed match. |
| Mixed versions | New client/old server fails with protocol gap; old client/new server still parses zero legacy revision; missing fields never become success. Verify legacy filter refusal and previous-release log readability. |
| Overflow / identity | Counter exhaustion changes epoch without wrapping; reused pane/agent names cannot satisfy an earlier subject's wait. |
| Cost / quiet state | Benchmark cadence and memory at 100 panes with and without waiters; no per-sample fsync, hub traffic, render invalidation or unbounded backlog. |

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

### 5a. Completed PTY write acknowledgements (#1052)

**Part of #1052. Design first; implementation follows approval.** The architect
accepted the completed-write contract on 2026-10-16. The details explicitly
marked **Owner decision required** below are proposals, not implemented policy.
This section composes with injection priority (§5), guarded-submit write
receipts (#1463, #1517, #1522), and ADR-0037 §1's condition-wait rules.

#### Decision and scope

A successful input `Ok` means **every encoded byte of that request reached the
PTY master through successful write syscalls**. It does not mean the child read
those bytes, a key was acted on, a command ran, or an agent accepted a turn.
Queue admission, an actor wake, and a healthy writer flag are insufficient.
Existing guarded submission still requires its separate harness evidence.

Apply this boundary to `pane.send_input`, `pane.send_keys`, and any successful
input acknowledgement from `pane.send_text`, non-submit `agent.send`, and the
raw type/Enter steps used by `pane run`. Existing paste/guarded result envelopes
retain their meanings; none may claim successful writing before its receipt.
Internal admission may return an explicitly queued ticket, never a completed
write acknowledgement. An admitted lease-held raw request remains pending,
including paths that currently return `held_by_injection` early; its payload
must not survive a terminal no-write response. Terminal-generated replies are
not caller input.

Reject **queue acceptance followed by a failure event**: it leaves each caller
to discover that earlier success was false. No new public or persisted event
kind is required for this contract. Existing guarded-submit events remain.

#### One request, one cancellable write owner

Retain request identity, response ownership, absolute deadline, ordered encoded
payloads, and a cumulative successful-write byte count from admission through
lease holding, backlog, `data_rx`, pending writes, and the final syscall.
Count bytes after text/key/bracketed-paste encoding, not Unicode characters or
key names; multiple payloads share one request's count and terminal outcome.
Use a unique internal admission ticket: JSON request IDs can repeat across
connections and are not a global ownership key.
Keep existing FIFO and operator/injection priority. Do not merge payloads in a
way that loses cancellation, byte accounting, or originating request identity.

Extend the existing writer-owned receipt/cancellation gate. A nonblocking write,
its byte-count update, completion, and cancellation serialize through that gate.
No actor dequeue or syscall may bypass it. The request resolves exactly once;
response ownership may not fall between queue and worker. Keep admission and
pending-reply retention bounded, including requests outside the actor channel;
never create a blocking worker/thread per input request.

At success, the receipt contains the final-write timestamp and total byte count.
A later writer failure cannot revoke a completed write. A partially written or
untouched request is pending and must not become successful when its channel is
closed, its queue is discarded, or its producer disappears.

#### Deadline, lease and cancellation

The server derives one monotonic deadline at request admission. Queue, lease,
backlog and write time all consume that same budget; moving a request never
restarts it. Waiting is event-driven and leaves the app loop responsive.

- If expiry/cancellation wins before the first write, remove **the payload**
  from every queue/lease/backlog and retire its ticket before replying. Report
  `bytes_written: 0` and guarantee it can never be written later, including
  after a lease releases, the actor drains `data_rx`, or the writer rearms.
- A channel that cannot physically remove an entry may retain only an inert,
  cancelled ticket: no live bytes and no authority to write. Keeping bytes
  queued and merely promising to skip them eventually does not meet removal.
- If some bytes reached the PTY, cancel under the same writer gate and discard
  the remainder before replying. Never roll back or silently replay the prefix.
  Today `WriteReceipt::cancel()` (`src/pty/write_receipt.rs:66-96`) returns
  `Partial` without setting `cancelled`, allowing the writer to continue.
  Implementation must set `cancelled` for partial cancellation under the gate
  and discard the unsent remainder, while retaining the exact prefix count.
  Audit the #1522 guarded-submit path (`src/app/guarded_submit.rs:1224`) against
  this changed shared-receipt semantics: partial paste remains unconfirmed,
  never safely abandoned/retried, and must not proceed to Enter or emit a second
  terminal outcome after cancellation.
- Check expiry **inside the write/cancel gate, before each syscall**, and compare
  final completion time with the deadline. Completion by the deadline succeeds;
  a late final syscall is an error with the actual count, which can equal the
  total. This late-completion
  case does not justify another syscall or a false zero-byte guarantee.
- Observed caller disconnect, pane death/restart and handoff invalidate pending tickets:
  cancel them under the same gate, release payloads/quotas, and prevent an old
  owner from writing after the transition. Handoff must resolve or cancel each
  pending request before releasing ownership, not migrate an uncorrelated write.

For ordinary single-request connections, `src/api/server.rs::handle_connection`
waits for `handle_request` before attempting the response write; it does not
observe socket EOF concurrently with that wait. This design relies on the
request deadline alone to bound an unobserved caller disconnect, not immediate
cancellation on client close. Bytes can still be written before expiry after
that close, so the disconnected caller has an unknown outcome. If disconnect
is observed earlier, apply the same gated cancellation; no pending request may
outlive its write deadline merely because the response owner is waiting.

**Owner decision required — deadline interface and bounds.** Propose optional
`write_timeout_ms` on the affected raw input requests, default `10000`, valid
`1..=30000`. An omitted field uses the bounded default. Invalid values are
refused before admission. The CLI's bounded response wait must outlast this
server deadline and its cancellation reply; propose a `1000` ms reply margin.
Require `write_timeout_ms` to be strictly below the effective caller cap minus
that reply margin, accounting also for a shorter remaining caller deadline and
transport/forwarding time. The implementation must fix the in-tree delegate
caller (`src/cli/delegate.rs:268`): `PaneSendInput` currently has a 10 s socket
cap, equal to the proposed default write deadline. Without reserving reply time,
a valid cancellation or completion reply can be lost and become unknown. Keep
the caller cap bounded and explicitly select a shorter write budget (for
example 8000 ms within a 10000 ms cap and 1000 ms reply margin), reducing it
further when the remaining caller budget requires it; refuse before dispatch
if no positive write budget fits. Apply this budgeting to the new write method
cap entries too, rather than letting them inherit the generic 2 s poll cap.
A client timeout alone is not proof of cancellation. Propose at most 32
outstanding input requests per pane and 1024 per server, counting held, queued
and writing requests together, alongside existing actor admission limits;
preserve any existing payload-size restrictions.
Admission at either limit returns zero-byte cancellation before retaining input.
Approve these deadline, reply-margin and admission limits before implementation.

#### Typed results and retry policy

**Owner decision required — error names, details and exit mappings.** Reuse
existing numeric CLI exits, but propose two precise server error codes and one
client-side uncertainty code. Current `pane_send_failed` and `write_failed`
have retry class `maybe`; reclassifying them globally would change unrelated
callers. They cannot stand in for the new guarantees. Existing target, key and
policy errors remain unchanged when refused before admission.

| Result/code | Required evidence | Retry rule | CLI exit |
| --- | --- | --- | --- |
| Existing `Ok` | Final successful write by deadline; all encoded bytes counted | Do not resend | `SUCCESS` (0) |
| New `input_write_cancelled` | `bytes_written = 0`; payload removed and ticket irrevocably cancelled | `later`: safe to retry the full request once the reported condition clears | `REFUSED` (10) |
| New `input_write_failed` | Exact positive `bytes_written`; remainder discarded; includes a fully written request that missed its deadline | `after_inspecting`: never automatically retry the full request | `UNCONFIRMED` (8) |
| New local `input_write_unknown` | Connection/server loss without an authoritative final receipt | `after_inspecting`: count/effect unknown, never assume zero or automatically resend | `UNCONFIRMED` (8) |
| Existing `invalid_params` | Invalid write deadline, no admission/effect | `never`; correct the call | Existing `FAILURE` (1); CLI argument validation uses `USAGE` (2) |
| Existing `server_version_gap` | Required write contract absent; no payload sent | `after_handoff`; upgrade the other endpoint | `PROTOCOL_GAP` (78) |

Register the proposed codes in `src/api/error_code.rs`. Give their exits explicit
registry mappings so both guarded and ordinary send helpers, JSON output and
MCP agree; a send-like-only override is insufficient for ordinary key sends.
Do not use generic `TIMEOUT` (124, retry `yes`) for uncertain or partial input.

Propose additive optional `error.data.input_write` with `bytes_written`,
`bytes_total`, `reason` and `remainder_discarded`. Counts are exact nonnegative
encoded-byte integers for server cancellation/write failures, and
`remainder_discarded` is true only after cancellation has completed. Reasons
include `deadline`, `writer_broken`, `pane_gone`, `owner_changed` and
`admission_limit`; the code
and fields, not prose, decide retry safety. A local unknown-outcome error uses
null counts and makes no no-ghost-write claim. This is a proposed extension to
`ErrorBody`, which currently contains only `code` and `message`.

A partial prefix may include an escape sequence, a multibyte character, or a
submitted key. The caller must inspect the pane and re-establish its input
state before issuing a new operation. Sending the nominal suffix is not a
universal safe retry: it may target a different parser/composer state. Automatic
suffix or full-request retries are unsupported. A lost success response is
also uncertain even if the server actually completed all bytes; this design
does not add idempotency or exactly-once execution.

If the writer breaks while requests remain in `data_rx` or pending writes,
each incomplete request gets its own typed zero/positive-byte failure. Clearing
a queue or dropping a completion sender must resolve the saved response owner,
not log and forget it. Already completed requests retain their honest success.

#### Wire compatibility and rollout

**Owner decision required — write method names, capability and additive schema.** Propose
`ServerCapabilities.pane_input_write_ack`, absent/default false on older
servers. Its true value attests this entire cancellation/count/deadline
contract, not merely that some guarded writes have receipts. Also propose
`pane.write_text`, `pane.write_keys` and `pane.write_input`, sharing the input
semantics/parameters of the corresponding `pane.send_*` methods plus the write
deadline. Existing names remain available and get the stronger acknowledgement
on a new server. New method tags bind the stronger contract to the operation
that performs the write, not just a separate preflight observation.

An older client may keep sending the same JSON request and accepting the same
`Ok` envelope. Old clients now wait for the write, up to the write deadline
(default 10 s), instead of receiving an immediate queue-level `Ok`. A new
server provides stronger success evidence with the bounded default; an old
client treating `Ok` as accepted remains correct. Error fields
are additive: tolerant clients ignore unfamiliar details; an older CLI treats
an unknown error code as failure (1), never success. That older client cannot
claim exact counts or the new retry classification. Raw JSON consumers that
reject unknown error fields need updating; do not promise compatibility with
strict third-party schemas. An older client with a shorter socket timeout can
lose the reply while bytes are being written; it must inspect rather than
blindly retry. Stronger success does not make transport-timeout retries safe.

A new client requiring completed-write guarantees checks the capability and
uses only the new write method tags. If the capability is absent it returns
`server_version_gap` / 78 before issuing a write request. An old server also
rejects the new method tag before writing, so a stale capability observation
across restart/handoff cannot produce an old queue-level `Ok`; normalize that
specific unknown-method refusal to `server_version_gap`. The current API reads
one initial request per connection, so a separate ping is not a connection-bound
attestation. A cached capability by socket pathname is not sufficient by itself.

Do not fall back to a legacy send method or put a mandatory-looking field on it:
an older server can ignore unknown parameter fields and still enqueue input.
After any transport failure following dispatch, report unknown effect, not safe
cancellation. Disable generic transport retries for these state-changing calls.

Cross-host input forwarding across a mixed-version mesh is included in this
contract. The node owning the destination PTY must support the completed-write
contract; a forwarding node's own capability is insufficient. An old remote
node can return a queue-level `Ok` for a legacy `pane.send_*` request; that is
not completed-write evidence and must never be upgraded to a strong `Ok` by a
new forwarding node. Require destination capability and a contract-bound write
operation through every forwarding hop. An old destination without that
capability, or an old hop unable to carry the operation and its final receipt,
must cause the requiring client/forwarder to return `server_version_gap` / 78
before input is sent. An old destination presented with a new method tag
rejects it without writing; the client/forwarder normalizes that refusal to 78.
Never fall back to legacy input forwarding. With capable hops, relay the final
PTY owner's exact receipt/count/error, not hop admission, and consume one
end-to-end budget including transit and reply margins; forwarding cannot reset
it. Loss of the remote reply after dispatch is unknown, never zero-byte
cancellation. This requirement does not change interactive terminal keystroke
forwarding into a completed-write RPC or promise that legacy mesh input already
has receipts.

This proposes NDJSON request/capability/error additions with unchanged success
shapes, not a new bincode input message. Do not change protocol constants in
this design PR. If implementation changes binary wire shapes, apply the normal
latest-release version comparison and compatibility/golden-fixture rules; do
not allocate a second unreleased bump or conflate it with mesh VERSION.

#### Acceptance cases for the implementation PR

| Controlled case | Required observation; the old behaviour must fail |
| --- | --- |
| #1052 reproduction | Sender gets queue-level `Ok` with 25 bytes still in `data_rx`; close a native socket peer before actor drain. Current code clears all bytes and only logs. New code returns typed cancellation, zero written, and no `Ok`. |
| Healthy and empty writes | No success while bytes remain queued; exactly one success after final write. An empty encoded request resolves as a zero-byte completed operation without a syscall. |
| Lease/backlog expiry | Hold the actual lease or consumer, enqueue, expire the request through the deadline path, receive zero-byte cancellation, then release and drain. No cancelled bytes arrive; later live input still arrives in FIFO order. |
| Partial failure | Fill/gate a real nonblocking socket to allow an observed prefix, then break it. Error count equals the bytes observed; no suffix is written after error or recovery. No product test write hook. |
| Partial cancellation / #1522 | Cancel after an observed prefix while the writer remains healthy, then make it writable again. The cancelled flag prevents all remainder writes; guarded paste stays unconfirmed, sends no Enter, and concludes once. |
| Deadline after prefix/final syscall | Control write/cancellation order, including expiry while waiting to acquire the gate. Expiry is checked inside the gate before the syscall. Check exact positive count, including the late-full-write case, one terminal response, and no further writes. |
| Many queued requests at break | Mix queued and pending requests; each incomplete request answers once with its own count. A completed request stays successful, and later refused input does not hang. |
| Competing completion/cancel | Exercise both gate orders. Cancellation before any byte proves permanent removal; completion wins only with timely final-write evidence. |
| Caller/pane/owner loss | Disconnect a single-request caller while the writer is held: without an EOF observer, the deadline still retires its ticket and payload. Unobserved disconnect permits pre-deadline writes and gives the caller no zero-byte guarantee. Also close/restart the pane and hand off; no stale-owner or post-cancellation lease-release ghost writes. |
| Live silent producer | Hold a live writer without progress; the existing request deadline produces typed cancellation/failure, not success or an unbounded wait (ADR-0037 §1). |
| Caller budget / delegate | Exercise the actual delegate call with its 10 s cap and shorter caller deadlines. The selected write deadline stays below the effective cap minus reply margin; replies fit, exhausted budgets refuse before dispatch, and lost replies become unknown without retries. |
| Mixed-version forwarding | New forwarder to old destination or incapable hop refuses with 78 before input; legacy remote queue-level success is never strong success. With capable hops, hold/break the destination writer and observe its final count/error at the origin, one end-to-end deadline, and unknown effect after a lost remote reply. |
| API/CLI compatibility | Start at real request dispatch, not a prefilled receipt. Check success/failure codes, byte fields and exits; new client refuses an old server before PTY input (also after a stale capability/restart race), old client reads new-server `Ok`, typed errors are not retried, lost replies become unknown. |

The first row was demonstrated on dev `5c315be5`: two native actor fixtures ran,
with the untracked loss assertion failing and the tracked-receipt control
passing, nextest exit 100 (0.032 s total). The replayable fixture and final log are
referenced by the #1052 round-1 receipt. Other rows are required red/green controls
for the implementation PR, not claims of completed verification. Fixtures use
real channels/sockets, explicit gates and bounded receives, with no sleeps,
product test hooks, widened windows or application side effects on a live server.

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

Decided by the operator on 2026-10-11 (PR #1006): items 1–6 below were
accepted as recommended. The rejected alternative is kept for the record.
Items 7–9 are new §1b proposals and require a separate owner decision; the
earlier acceptance does not cover them. Item 10 records the #1052 architect
decision; item 11 lists its proposed details requiring owner approval.
Item 12 records the separate 2026-10-16 option-1 acceptance.

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
7. **Observation lifetime (§1b). Owner decision required.** Recommend per-runtime
   sample/output cursors with fresh cryptographically random 128-bit epochs
   across process lifetimes. Restart, respawn and handoff invalidate cursors and
   pending waits; persist no receipts and never silently rebase old cursors.
8. **Sample wait and event policy (§1b). Owner decision required.** Recommend a
   dedicated capability-gated `pane.wait_for_sample`, additive local reads, no
   global/durable sample event, and per-pane receipt-commit notifications
   independent of E6. Cap pending waits at 8 per pane and 256 per server, with
   `sample_wait_limit_exceeded` / `REFUSED`; cap timeout at 540000 ms, with
   `invalid_sample_timeout` / `USAGE` outside `1..=540000`. Approve these bounds
   and the proposed memory/CPU measurement gate before implementation.
9. **Legacy revision (§1b). Owner decision required.** Recommend deprecating all
   legacy numeric revision fields, explicitly including
   `EventData::PaneOutputChanged.revision`, retaining zero and compatible wire
   schemas/log readability, and refusing the unfinished `min_revision` filter
   with `unsupported_legacy_revision` / `PROTOCOL_GAP`. Approve that named
   behavior change with a release note; removal requires separate major-wire
   approval. Give only the new output cursor a parser-progress writer.

10. **Input acknowledgement (§5a, #1052).** Decided by the architect on
    2026-10-16: `Ok` means completed PTY write using writer-owned receipts.
    Deadline expiry before any write removes the input permanently; partial
    failures disclose the byte count and never write the remainder. Rejected:
    queue acceptance followed by a failure event that callers must reconcile.
11. **Write-ack details (§5a). Owner decision required.** Approve the proposed
    deadline field/default/range and client reply margin, pending-request bounds,
    error codes/details/CLI exit mappings, write method names, and capability/compatibility rollout.
    Core input-acknowledgement acceptance in item 10 does not approve these
    proposed schema details; item 12 covers the separate rollout policy.
12. **Codex rollout format/version policy (§1a). Accepted by the owner,
    2026-10-16, option 1** ([#1479 decision](https://github.com/gerchowl/flock/issues/1479#issuecomment-6203978023)).
    Use rollout lifecycle authority within the tested compatibility range,
    initially only 0.160.1; validate creation metadata and the resolved writer's
    version, diagnose unsupported/missing versions and select screen fallback.
    CI pins the declared version constant and fixtures checked against it.
    The format's inability to prove a silently omitted well-formed record is
    an accepted residual limitation. Implementation of the source/continuity
    contract has passed the docs review gate through approval of #1615.
    S1 supplies version eligibility only; #1661–#1664 complete the adapter.
