# ADR 0030 — Goal supervision decides most stops without a judge: parked on a registered wait is silent, the judge is a bounded last step, and the goal survives compaction as hook output

- Status: Accepted
- Date: 2026-10-11 (accepted 2026-10-11)
- Scope of acceptance: the decisions in this record, as the operator answered
  them on 2026-10-11 (PR #1006, "Owner decisions"). Acceptance claims no code.
- Issues: #1001 (area 3), #573 (goal mode in flk), #882–#886 (judge question
  set, owner-reserved clauses, handoff sentinel, breaker and fallback,
  decisions labels), #962 / #965 (delegate table and goal re-inject), #714
  (System One is now Clef), #697 (turn vs background), #850 (injection lock).
- Builds on: ADR-0027 (delegate edges), ADR-0028 (state vocabulary, events,
  registered waits, wake), ADR-0029 (exit and error contract).
- Decision owner: operator.
- Implemented: G1a (#1034): the durable `Goal*` kinds, the in-memory record
  and its re-issue on start and rotation (`src/app/goals.rs`). G1b (#1155):
  `goal.arm`, `goal.status`, `goal.pause`, `goal.resume` and `goal.stop` and
  `flk goal` (`src/app/api/goals.rs`, `src/cli/goal.rs`); the writes are open
  to the operator and to the goal's own agent, as goal-s1's were. G1 was split
  into G1a #1034, G1b #1155, G1c #1156 and G1d #1157 on 2026-10-12. Slices (§7) filed 2026-10-11: G0 #1033, G1 #1034, G2 #1035, G3a #885, G3b #1036, G3c #886, G4 #882 + #883, G5 #884, G6 #965. The cross-ADR order is §8.

## Context

Goal mode today is goal-s1, a Stop hook outside flk. The toolchain survey
(#987; 21 days, three hosts with a decisions log) counted its judged stops:

| Decision | Count |
| --- | --- |
| allow: waiting on background tasks | 1,294 |
| block: bg-checkin | 262 |
| block: routine-ask | 138 |
| allow: parked | 124 |
| allow: legit-ask | 79 |
| block: continue / review / met / park-met / park-expired | 22 / 10 / 7 / 16 / 9 |
| block: compact, then allow compact-handoff | 6 / 6 |

Two thirds of all decisions are an agent waiting on something. The median
allowed wait was 2.4 h to 19 h depending on host, and the 262 `bg-checkin`
pushes are the judge prodding agents whose wait an event would have resolved.
The judge (then Kev) failed on 8.5 % of calls (104 timeouts, 25 breaker-open,
18 connection errors). `park` was used 88 times, almost all on one Linux host,
because its poller dies on macOS. Five compactions were "not identifiable",
owners compacted by hand 92 times, and three hand-rolled wrapper scripts exist
only to re-arm the goal after `/compact`. A 2026-10-09 field note on #573 adds
that a continue-hook and an idle-gated compaction starve each other: every stop
is re-prompted, the pane never idles, and the compaction never fires.

The flock architect's own scaffolding shows the same need: `supervisor.py`'s
`goal_check` re-runs a shell check per delegate and types `[goal] Not done yet`
into its pane, and `maybe_compact` drives a three-phase STATUS.md handshake by
scraping the context percentage off the status line.

### Clef judge measurements (2026-10-11)

System One moved to Clef (#714). #573's latest comment measured the delegate
question set (4 KB screen tail, 10 questions): fast tier p50 1,504 ms, p95
1,548 ms uncached; full tier p50 4,975 ms; identical requests ~22 ms from the
prefix cache; requests are **serialized** (4 concurrent finished at 1.7 / 3.4 /
5.1 / 6.7 s); over-context and over-64 KiB states are refused (400 / 413), not
truncated.

For this ADR the goal judge's own shape was measured against the same fast
endpoint, with goal-s1's real question set (`asks`, `why`, `deferred`,
`file_ask`) and 30 distinct final messages from goal-s1's labelled
re-validation fixtures (median 1.5 KB):

| Measure | Result |
| --- | --- |
| Sequential, distinct states | p50 822 ms, p95 886 ms, max 925 ms, 0 errors, 0 over 3.5 s |
| Same state ×5 | 881 ms, then 22–25 ms; identical scores every time |
| 3 concurrent distinct | 745 / 1,409 / 2,160 ms |
| 6 concurrent distinct | 724 / 1,495 / 2,220 / 2,936 / 3,703 / 4,354 ms |

So the judge is fast and deterministic alone, and its tail is set entirely by
queueing: each request ahead adds about 0.73 s, and the **fifth** concurrent
request (3,703 ms) already exceeds goal-s1's 3.5 s timeout. The samples are
small (30 sequential and 9 concurrent calls, `tests/probes/agent_wake.py
judge-latency`); the per-request queueing cost is the robust part. A per-host fleet with several
agents stopping at once will hit that.

## Decision

### 1. The goal record is server state, folded from durable events

The goal belongs to an agent (`AgentId`, never a pane; #573 owner decision 2)
and lives in the server. It is recorded as durable events in the one log
(`GoalArmed`, `GoalAmended`, `GoalStatusSet`, `GoalSubtaskChanged`,
`GoalVerdict`, `GoalMet`, `GoalPaused`, `GoalResumed`, `GoalDisarmed`) and
folded into a live record, the same pattern as lineage and ADR-0027's edges,
with the same re-issue-on-start rule for open goals so rotation cannot drop
one.

Amended 2026-10-12 (owner decision on #1034): pause and resume are recorded as
two durable kinds of their own, `GoalPaused` (with an optional end time) and
`GoalResumed`.

That answers three #573 requirements at once: crash safety (every write is one
appended event), versioned state (the owner's field report lost STATUS.md to
one bad edit; an append-only record cannot), and an audit of every verdict.
Whether the fold is enough at the measured hot-path cost, or a SQLite store is
needed, is #573's storage spike, and this ADR treats the event fold as the
recommendation to test there, not as settled.

### 2. A stop is decided by a ladder, and the judge is the last rung

At every turn boundary of an armed agent (Claude Stop hook, Codex stop
control, the opencode plugin's idle event), the adapter asks the server, which
decides in this order and stops at the first rung that applies:

| # | Condition | Verdict | Judge? |
| --- | --- | --- | --- |
| 1 | goal paused, disarmed or met | allow | no |
| 2 | a compaction is armed or the handoff sentinel (#884) is present and fresh | allow, so the compaction can run | no |
| 3 | `state.attention` is `needs_human` or `blocked` (ADR-0028) | allow; notify the supervisor or operator (§5) | no |
| 4 | the agent has an open **registered wait** (ADR-0028 §5): parked on a delegate, a PR's CI, a reply, or a `flk events wait` running under the harness's background mechanism | **parked**: allow silently, no push, no check-in timer | no |
| 5 | every open subtask is owned by a live delegate edge (ADR-0027) whose `phase` is `working`, `settling`, `finished_with_background` or `waiting_on_event` | parked (as 4, derived from edges) | no |
| 6 | frozen checks are green | enter the done gate: independent review, then `GoalMet` | no |
| 7 | background work flk cannot attribute (`finished_with_background`) | allow, and schedule check-ins instead of a per-stop push: one check-in after 30 min (`[goal] background_checkin_s`), then the interval doubles (60 min, 120 min, …) while the background work flk cannot attribute is still running | no |
| 8 | otherwise: classify the final message | judge (§3): `asks` / `routine` / `legit` / `deferred` → block with a continue nudge, or allow | yes |

Consequences of the order:

- **Waiting never reaches the judge.** Rungs 4–5 replace the 1,294 judged
  "waiting on background tasks" allows and most of the 262 `bg-checkin`
  pushes, because an agent that waits through flk is visibly parked. Only
  waits flk cannot see (rung 7) get a timer, and the timer is a deadline, not a
  per-stop push.
- **Delegates that will still produce an event park the architect.** A
  `settling` delegate becomes `finished` or `working` within the settle window,
  and a `finished_with_background` delegate becomes `finished` when its build or
  push ends (or hits its own check-in deadline); either way an event follows
  and wakes the architect. Delegates that are `finished`, `needs_human`,
  `blocked` or `gone` do not park it, because each means the architect has
  something to do now (review, answer, re-plan).
- **Compaction beats continuation.** Rung 2 is above every block, which
  removes the 2026-10-09 starvation. A verdict is not a write into the pane, so
  this does not touch the injection priority in §4, where the operator comes
  first.
- **A human-owed answer is never pushed back.** Rung 3 is above the judge, and
  owner-reserved actions (#883: merge, release, deploy, "your call") are
  classified as `legit` before the routine threshold is applied.

### 3. The judge is bounded, deduplicated and never mistaken for success

- **One judge client per host, in the server.** Requests are serialized by the
  server, not by Clef's queue: at most one in flight, and a queued request
  whose turn has moved on (a newer turn cursor for the same agent) is dropped,
  not sent. This keeps the measured ~0.8 s latency for the request that
  matters instead of a 4 s tail behind stale ones.
- **Cache by `(agent, turn_cursor)`.** Clef is deterministic for an identical
  state (measured), so a repeated stop on the same final message reuses the
  verdict instead of paying even the 22 ms.
- **Budget:** a 3.5 s timeout inside a 7.5 s adapter deadline (goal-s1's
  measured choice, and still > 3× the measured p95 for this question set). A
  breaker (#885): two timeouts within 60 s open it for 5 minutes; timeouts
  caused by the adapter's own deadline do not count. States over the model's
  context are cut to the final message tail before sending, since Clef refuses
  rather than truncates.
- **Fallback:** optional `[goal] fallback_judge` (a model invoked with the
  remaining budget). Without one, a judge failure means **allow the stop and
  mark the goal `degraded`**, visible in `flk goal status` and sent to the
  supervisor. A judge failure is never `met`, and never a silent HOLD
  (claude-config#12's opencode bug).
- **Calibration is owned in this repo** (#882): the question set and thresholds
  are versioned together, the labelled corpus is committed (aggregated,
  redacted), and `flk goal judge revalidate` re-scores it on every model change.
  The routine AUC drop on Clef (0.935 → 0.723, claude-config#70) is a known
  open item that this command exists to track.
- **Every decision is logged with all scores and a label slot** (#886), as a
  `GoalVerdict` event, so labels can be attached later from the owner's next
  reply.

### 4. One injector, with a fixed priority

Goal nudges are typed only when the agent's turn has ended without a block (an
idle agent the stopwatch decided to nudge). They go through the one guarded
injection of #850, like every other wake (ADR-0028 §5). When several writes are
due at one boundary the order is: **operator input**, then an armed
compaction, then a wake for a registered wait or mail, then a goal
continuation. That is the order ADR-0028 §5 states and the one #850 and
ADR-0027 §3 already imply: an operator typing always wins, and a compaction
waits for the operator's quiet window like any other write. A goal nudge never
interleaves with a compaction.

### 5. Goals report up through ADR-0027's edges and ADR-0028's digest

- A delegate may run under its own goal (`flk delegate start … --goal "…"
  --check CMD…`), linked to its edge (#965). Its goal events (`goal.met`,
  `goal.degraded`, `goal.needs_owner`, `goal.cap_reached`) are digest lines for
  the edge's architect, delivered and woken by ADR-0028's rules.
- `needs_owner` (a `legit` ask, an owner-reserved action, `needs_human`) also
  files an operator notification (ADR-0016) and is posted to any channel
  binding that subscribes to it (ADR-0028 §8). That is the phone answer path
  #1005 asks for.
- An architect's own goal parks on its edges (rung 5), so "I am waiting for my
  delegates" is a deterministic verdict, not a judgement.

### 6. Goal state survives compaction as hook output, never as typed text

- On SessionStart with `source=compact` or `resume`, flock's hook asks the
  server and returns, as `additionalContext`: frozen goal, gated checklist,
  current status, the delegate table (ADR-0027 §4), in that order, bounded.
  This covers armed and unarmed compactions (#962, #965). Codex and opencode
  use their equivalents where verified, and otherwise the agent calls `flk goal
  status`.
- **The pre-compaction handshake is a server field, not a file.** When a
  compaction is armed (by the agent, or by the supervisor at the configured
  context band, the owner's 50–60 % rule), the server asks the agent once, via
  the fixed-sentence wake, to record its status with `flk goal status set`
  (MCP `flock_goal_status_set`), a versioned `GoalStatusSet` event. The
  compaction proceeds after the status is set or after a bounded wait, as
  `supervisor.py`'s three phases do today.
- **The handoff sentinel** (#884) is honoured as rung 2 and is the same
  mechanism self-compact uses, so an agent can hand off without the judge
  pushing it on.
- **Continuations accept what agents send.** A multi-line continuation passed
  to self-compact is written by the server to a file under its state dir and the
  typed continuation names the file (4 of 12 MCP self-compact calls in the
  survey were refused for a newline).

### 7. Slices

Skew: every goal verb is a new `Method` variant. The state-changing ones (`arm`,
`amend`, `status set`, `pause`, `stop`) are gated by #868 and refused locally
with exit 78 against an older server; `goal.status` is read-only and gets exit
78 via `normalize`. Adapters (the Stop hook, the opencode plugin) talk to the
server in their own pane, so a hook binary newer than its server treats any
`server_version_gap` from a goal call as "no goal armed" and allows the stop:
an old server never makes a new hook block. The `Goal*` events are new
persisted kinds and take ADR-0028 §3a's reader check. Goals are per host;
mesh peers relay none of it.

| # | Slice | Depends on | Replaces / amends | Acceptance |
| --- | --- | --- | --- | --- |
| G0 | Storage decision (#573's spike): event fold vs SQLite, measured on the Stop-hook hot path | — | #573 spike | A short ADR with the measured cost of `goal.status` and a verdict write |
| G1 | Goal record (arm / status / pause / resume / stop, CLI + MCP), Claude and opencode adapters returning the server's verdict for rungs 1, 6 | G0 | #573 core | Restart the server, move the pane, duplicate an event: the goal and its verdicts survive (#573's acceptance probe); a hook against an older server allows the stop |
| G2 | Deterministic rungs 2–5 and 7: compaction first, attention, registered waits, edge-derived parking, background check-in deadline | G1, ADR-0028 E5, ADR-0027 slice 2 | #573 "parked" comment, #697 consumer | An architect parked on two delegates stops 10 times with no judge call and no push; a delegate going `finished` wakes it once |
| G3a | Server judge client: timeout inside the adapter deadline, breaker (own-deadline timeouts excluded), optional fallback judge, `degraded` state, never `met` on failure | G1 | #885 | Fake judge: two timeouts open the breaker for 5 min; an outage yields allow + `degraded`, never `met` |
| G3b | One request in flight per host, stale-turn drop, `(agent, turn_cursor)` cache, state cut to the final-message tail under the model's context | G3a | — | Fake judge with 0.73 s per queued request: six concurrent stops send one request per live turn, and none waits behind a stale one |
| G3c | `GoalVerdict` events with all scores, thresholds version and a label slot; `flk goal decisions` export | G3a | #886 | Every judged stop appends one `GoalVerdict` with the full score set; a label can be attached later |
| G4 | Question set, versioned thresholds, committed corpus, `flk goal judge revalidate`; owner-reserved clauses extracted from the goal text | G3c | #882, #883 | Revalidate prints per-question AUC against the corpus; an ask about a reserved action is never `routine` |
| G5 | Handoff sentinel, compaction priority over continuation, pre-compaction `GoalStatusSet` handshake, multi-line continuations via a file | G2 | #884, survey item 10 | A continue-armed agent at the context band compacts within one boundary once the operator is quiet; a multi-line continuation is accepted |
| G6 | SessionStart re-inject of goal + checklist + status + delegate table; goal link on edges; goal events as digest lines and operator notifications | G1, ADR-0027 slices 2 and 4, #961 | #962, #965 | An unarmed auto-compaction of an armed architect yields the four parts as hook output and types nothing |

### 8. Cross-ADR implementation order

The three ADRs of this spike and ADR-0027 interleave. Dependency order, with
each line shippable on its own:

1. **C1** exit and error registries (ADR-0029), **C4a** `typed_not_submitted` — honest outcomes first.
2. **E1a** state object, **E7** PR/CI events, **C5** target resolution — independent.
3. **E1b**, **E1c** background and attention axes; **C2**, **C3** output contract; **C4b**, **C4c**, **C4d** submit by default, exit moves, docs.
4. **E2** turn-keyed wake and self-compact; **E3** derived events; **E4a** → **E4b**, **E4c** event reads; ADR-0027 slice 2 (#960; slice 1, #959, has landed).
5. **E5** wait registry; **E6** wake-follows-wait; ADR-0027 slice 3 (#961, now woken by E6).
6. **G0** → **G1** → **G2**, **G3a** → **G3b**, **G3c** → **G4**, **G5**; ADR-0027 slice 4 (#962).
7. **G6** with ADR-0027 slice 8 (#965); **E8** channel contract, then **E9a** → **E9b** → **E9c** questions with options (#1005).

Retirement checkpoints for the flock fleet's own scaffolding: after step 5,
`supervisor.py`'s status loop, CI watcher and typed `[fleet]` lines; after
ADR-0027's reap slice, its reap loop; after step 6, `goal_check` and the
STATUS.md compaction phases; after C4b and C4d, the send-keys fallback in briefs and
the dialog scraping in `spawn-claude.sh` (with #605 already landed).

## Consequences

- Most stops cost nothing: no judge call, no push, for every agent that waits
  through flk.
- Judge load is bounded per host and tied to live turns, so Clef's serialized
  queue no longer turns into timeouts.
- Goal state is versioned and auditable, and it outlives compactions, restarts
  and pane moves.
- goal-s1 shrinks to a shim (claude-config#24) once G1–G3 ship.

## Alternatives considered

- **Keep the judge first and add parked as a judge question.** It pays the
  judge's latency and failure rate on the two thirds of stops that are waits,
  and a model cannot know what flk already knows.
- **Codex native goals** as the mechanism. Prior art and, for Codex workers, at
  most a stop-control bridge (#573 owner decision 1).
- **Let Clef's queue serialize concurrent judge calls.** Measured: the fifth
  concurrent call misses the timeout.
- **Goal state in repo files (GOAL.md / STATUS.md).** Owner decision 2 on #573,
  and the field report's lost STATUS.md.
- **Typed re-inject after compaction.** Only armed self-compactions take that
  path, and typing races the operator (ADR-0027 §4).

## Risks

- **Rung 4 trusts registered waits.** An agent could park on a predicate that
  never opens. Every park carries a mandatory deadline (ADR-0028 §5), and an
  expired deadline is a wake plus a check-in.
- **Rung 5 depends on edge attestation.** Delegates started by the operator or
  across hosts have no edge (ADR-0027 §8) and so do not park an architect; it
  falls through to rung 7.
- **Clef calibration** has not been re-fit for the routine question; G4 makes
  that measurable before any threshold change.

## Test strategy

- Per-harness adapter tests with the fake Claude and opencode fixtures (#573
  scope addition 1): the same server verdict through both.
- A ladder table test: one case per rung, asserting the first matching rung
  wins and the judge is not called above rung 8.
- A fake System One server with latency, refusal (400 / 413) and outage modes
  for G3.
- The #573 acceptance probe in an isolated flk: restart the server, move the
  pane, duplicate an event, lose the judge.
- An offline corpus re-validation in CI for G4 (no network: scores are
  recorded, the command checks the scoring code and thresholds).

## Owner decisions

Decided by the operator on 2026-10-11 (PR #1006): every item below was
accepted as recommended. The rejected alternative is kept for the record.
The decisions for ADR-0028 and ADR-0029 are listed at the end of each.

1. **Judge failure default.** Decided: allow the stop and mark the goal
   `degraded` (visible, reported to the supervisor, never `met`). Rejected:
   hold the stop until the judge returns, which turns a System One outage into
   a fleet that cannot stop (the opencode twin's bug, claude-config#12).
2. **Compaction vs operator.** Decided: the operator first, compaction
   second (§4). Compaction beats goal continuation in the stop ladder (rung 2)
   either way.
3. **Background check-in deadline** for work flk cannot attribute (rung 7).
   Decided: 30 minutes, then one check-in, then doubling. The survey's
   median allowed wait was 2.4 h to 19 h, so a shorter default would bring back
   the `bg-checkin` pushes this ADR removes.
4. **G0: goal state as an event fold in the log vs a SQLite store.**
   Decided: the event fold, unless G0 measures the Stop-hook read path over
   budget; it is the pattern lineage and ADR-0027 already use, and it gives the
   versioned, auditable state the field report asked for.
5. **Parking on delegates that are `settling` or `finished_with_background`**
   (rung 5). Decided: yes, because each produces a waking event; `finished`
   and `needs_human` delegates do not park the architect.

