# ADR 0033 — Agent-experience reviews are answered by a fork of the warm agent, against a versioned rubric, recorded as events, and filed only after a search; builder briefs put acceptance and size first

- Status: Proposed
- Date: 2026-10-11
- Issues: #1043 (the spike this records). Composes with ADR-0027 (delegate
  edges, #632), ADR-0028 (events, registered waits, channels, #1005),
  ADR-0029 (exit-code registry), ADR-0030 (goal supervision, #573),
  ADR-0032 (compiled context, #1042, Proposed on its own PR), ADR-0014
  (spawn ceiling), ADR-0015 / ADR-0023 (filing), #987 (toolchain survey),
  #1041 (proactive interactions).
- Decision owner: operator.
- Implemented: none. The measurement runbook is `tests/probes/fork_cache.py`.
  The slices (§10) are not filed; the architect files them after the owner
  accepts.

## Context

### What the architect does today, by hand

The architect skill makes friction reporting "half the job" (§6): record
tooling friction with exact commands and errors in HANDOFF, then file in
batches after searching. In practice that happens when the architect has
spare context, which it rarely does. The flock fleet's own state directory
shows the result. Its 800-plus-line `STATUS.md` mentions friction on six lines,
and the rest of it is scattered through status lines ("send
accepted", "my wait loop matched 'DONE: http…' inside a recap line", "3 of 4
slice builders overran 400 lines"). Filing is a side task, competing with
landing for the scarcest resource in the system, which is the architect's
context (skill §0). Every compaction then loses whatever was not written
down.

#1043 proposes to move that work out of the architect: at defined moments a
**fork** of the architect (`flk agent fork` / `flock_agent_fork`), which
carries the whole warm context, answers a fixed questionnaire and files
issues from its answers. The live architect is neither interrupted nor
charged context. The owner added two requirements on the issue:

- the rubric also covers **how the architect prompts builders**: which brief
  structure produced DONE in one round, which send paths worked, brief length
  against outcome, where builders asked back or went off-scope;
- data point (2026-10-11): 3 of 4 ADR-slice builders went over the brief's
  "~400 lines, if over, stop and report the split" and opened one PR anyway.

### How fork works today (verified at `6fece78f`)

- `agent.fork` (`handle_agent_fork`, `src/app/api/worktrees.rs`) resolves the
  target pane's agent session, runs ADR-0014's spawn ceiling for an agent
  caller, **creates a linked worktree and a new workspace**, and starts
  `claude --resume <session> --fork-session [<pivot>]` there
  (`agent_resume::branch_plan`). The pivot is the fork's opening turn.
- Only Claude has a conversation-fork affordance. Every other harness is
  refused with `ForkUnsupported` rather than double-attaching a session id
  (#175 F2). A Codex or opencode architect cannot be forked.
- The fork refuses up front when the parent's transcript is not on disk
  (#178). Claude turns transcript saving off for a session that inherits
  `CLAUDECODE` / `CLAUDE_CODE_CHILD_SESSION` from a parent Claude process, so
  a flk server started from inside a Claude session spawns Claude panes that
  cannot be forked.
- The fork records a durable `AgentForked` event, so it appears in
  `agent.lineage` and, once ADR-0027 slice 1 is everywhere, as an edge with
  `via: fork`.

### Measurements taken for this ADR

All in an isolated flk (copied 1.2.0 binary, sandbox `XDG_CONFIG_HOME` and
`XDG_STATE_HOME`), Claude Code 2.1.296, a throwaway session on Haiku 5.5,
the operator's Claude profile (whose cache writes are 1-hour TTL). Token
figures come from the `usage` block of each assistant message in the
transcript (`tests/probes/fork_cache.py usage`). The samples are single runs
per variant, so read them as the shape, not a distribution.

**Fork cost.** The parent read four ADRs (≈ 84k tokens of context) in one
turn and went idle. Its own last turn read 83,496 cached tokens and created
231. Then:

| Variant | Delay after the parent's turn | First turn: cache read | First turn: cache created | Hit |
| --- | --- | --- | --- | --- |
| A: `flk agent fork` (new worktree, different cwd) | 10 s | 83,900 | 1,287 | 98.5 % |
| B: in place (`agent start -- claude --resume … --fork-session`, same cwd) | 44 s | 83,900 | 954 | 98.9 % |
| C: in place, 15 min after the prefix was last read | 15 min | 83,900 | 957 | 98.9 % |
| D: in place, onto a different model (`--model` Sonnet 5.5) | 44 s | 14,115 | 70,887 | 16.6 % |
| Prototype: `flk agent fork` of a 136k architect stand-in (below) | 8 s | 135,967 | 1,292 | 99.1 % |

- **A fork right after the parent's turn is a near-full cache hit:** 98.5 to
  99.1 %. Its first turn costs about what one more parent turn costs; the
  roughly 1k created tokens are the pivot turn plus about 330 tokens for a
  different working directory.
- **The worktree does not break the cache** in this harness version (A and B
  differ by 333 created tokens). It is still an unnecessary cost for a fork
  that only reads (§3).
- **The TTL is the profile's.** C hit after 15 minutes because this profile
  writes 1-hour cache entries. A 5-minute-TTL account would miss there. A
  trigger should therefore fire at the turn boundary, not "some time later".
- **A fork must keep the parent's model.** D re-used only the harness's shared
  system prefix (14,115 tokens) and paid full creation for the conversation.
  A cheaper model for the fork is more expensive than the same model warm.
- The fork started answering 2 s after the call (A). The parent's transcript
  gained no entries during any fork: its context was not touched.

**Prototype run.** A second throwaway session stood in for the architect: it
read the flock fleet's real `STATUS.md`, the `architect` skill and the
supervisor log tail (≈ 136k tokens). `flk agent fork` then started a fork
with the pivot "Read `<rubric>` and answer it exactly", using the draft
rubric `ax/1` from §2 in dry-run mode (no filing):

| Measure | Result |
| --- | --- |
| Fork call → answer JSON | 47 s (4 API calls) |
| Fork call → pane settled | ≈ 5 min, all of it the profile's Stop hooks and a recap turn |
| Whole run | 563,943 tokens read from cache, 16,356 created, 9,660 output |
| Answers | all 8 questions, each citing log timestamps; "unknown" where the stand-in had no tool history (it had read the log, not lived it) |
| Candidate issues | 5, with confidence 0.5–0.7 |
| Dedup searches | failed inside the sandbox (`gh` reads `$XDG_CONFIG_HOME/gh`, exit 4); re-run read-only by hand |
| After the search | 4 of 5 match existing issues: #892 (shipped, not yet rolled out), #639 / #911 (shipped), #663 / #819 (open, covered by ADR-0028 E4), claude-config#49 (closed). The fifth (builders ignore the size budget) is new but is exactly §7 of this ADR |

So the cost is small and the answers were grounded, but **filing without a
search would have produced 4 duplicates out of 5**, and a confidence of
0.5–0.7 did not separate them from the new one. That sets the filing policy
(§6).

The same probe also hit friction worth recording. A guarded `agent send
--submit` of a one-paragraph prompt returned `unconfirmed /
owned_composer_not_visible` with the text left in the composer (ADR-0029's
`typed_not_submitted` case), and `flk agent send <target> <text> --submit`
on 1.2.0 typed `--submit` into the composer as text, because the flag is only
recognised before the target.

### Brief styles against review rounds

Data: every brief in the fleet's `briefs/` directory (255 files), mapped
through `agents.json` to its PR, and each PR's review history (`gh pr view
--comments`, the follow-up briefs, STATUS.md verdict lines). 115 initial
briefs map to a PR (68 Codex, 47 Claude Opus). A round is one follow-up brief
(`-fix`, `-fix2`, `-fix3`, `-nits`) sent to the same builder after a review,
plus three rounds STATUS.md records as sent inline. That makes it a lower
bound, mostly for Codex.

| Rounds | 0 | 1 | 2 | 3+ | n | zero-round share |
| --- | --- | --- | --- | --- | --- | --- |
| all | 50 | 55 | 9 | 1 | 115 | 43 % |
| Codex | 36 | 25 | 6 | 1 | 68 | 53 % |
| Claude | 14 | 30 | 3 | 0 | 47 | 30 % |

- **Brief shape barely varies, so it cannot explain much.** Every brief comes
  from one generator: Do steps, a Rules section, an explicit test command
  (111 of 115), file paths (115), a changelog-fragment rule. Out-of-scope
  appears in 9, a size budget in 6, known traps in 1. Median length is 19–22
  lines, 400–480 words.
- **Length tracks difficulty, not outcome.** Briefs over 600 words reach zero
  rounds 18 % of the time, against 46–53 % below; they are the hard mesh
  slices. Briefs with five or more Do steps: 25 % against 52 %. Same cause.
- **PR size is the strongest predictor we have:**

  | PR diff (add + del) | n | zero-round |
  | --- | --- | --- |
  | < 60 | 5 | 5 of 5 |
  | 60–250 | 29 | 76 % |
  | 250–600 | 29 | 24 % |
  | > 600 | 52 | 31 % |

  Median diff by rounds: 227 lines (0 rounds), 615 (1), 1,263 (2), 1,790 (3+).
- **The prose size budget does not hold.** Of the four slice briefs carrying
  "at most ~400 diff lines; if it grows past that, stop and report the split",
  one builder split before opening its PR (#1017 → #1048, 375 lines, with the
  rest announced as a follow-up). Three opened one PR over the budget with a
  split offered in the body: #1050 (514), #1056 (558 at open, 775 after
  fixes; the reviewer ruled "don't split"), #1057 (592; split only after a
  sequencing blocker). The owner's data point is confirmed. Builders almost
  never stop: no `BLOCKED:` final line occurs in the 115 PRs, and only two
  builders stopped to report (623-1h, which the architect confirmed as right,
  and #1017's self-split).
- **What reviewers block on** (share of the 65 PRs with a follow-up,
  non-exclusive, regex-classified then spot-read, so upper bounds):
  correctness 75 %, often a fix that breaks the neighbouring path (#582,
  #899, #919, #893, #1013) or a silent drop that still returns `Ok` (#1012,
  twice); false or overbroad claims 66 % (`Fixes` on a partial fix: #642,
  #907, #1048); missing or weak tests 52 %, including tests that cannot fail;
  changelog, docs and ADR 48 %; CI, guardrail and platform 45 % (a Linux-only
  clippy failure, the guardrail gates); scope creep 9 %; size 9 %; rebase
  damage 6 %. 55 of the 65 needed only one round: fix briefs (median 13
  lines: verdict, then `file:line` + required behaviour per item, then
  "merge, test, push, `DONE:`") converge.
- **Send paths.** Initial briefs went out through `spawn*.sh` (agent start,
  a dialog scrape, then `pane run`) and later `flk delegate start`. Briefs
  sat unsubmitted in #785 (Codex cold start), #723 (update and limit
  banners), #864 (Claude's composer hint read as `input_not_empty`) and #892
  (a faint suggestion). Fix rounds went through `flk agent send`, and STATUS.md
  records the `send-text` + `send-keys Enter` fallback about seven times. The
  survey's aggregate (#987) matches: 951 sends, 704 send-then-Enter pairs,
  32 uses of `--submit`.

**Confounders.** Later briefs are longer and for harder issues; the harness
changed from Codex to Claude Opus at #872, together with a stricter fresh
Opus reviewer on every PR and Codex quota exhaustion; Codex rounds are
undercounted; some nits became follow-up issues instead of rounds. No brief
style was tried against another on the same kind of issue, so this is a
correlational survey, not the "measured comparison" the owner asked for.
§10 slice X9 makes the comparison measurable from here on.

## Decision

### 1. An AX review is answered by a fork of the agent under review, never by the agent itself

- The reviewer is a fork of the warm session (Claude `--fork-session`). It
  shares the parent's context up to the fork point, at the parent's model
  (§ measurements, D). It answers, searches, files within the policy (§6),
  reports, and exits.
- The fork is **read-only toward the work.** It does not dispatch, merge,
  message delegates, type into panes or edit files. Its only side effects are
  issue searches, filings and comments within §6, one `AxReviewRecorded`
  event (§5), and its report.
- **Harnesses without fork** (Codex, opencode) get no AX fork. They fall back
  to a fresh agent started from ADR-0032's compiled context plus the agent's
  transcript tail, which costs a cold prefix and sees less. That fallback is
  slice X8 and is explicitly second-class.

### 2. The rubric is fixed, versioned, and lives in the repo

The questionnaire is **fixed per version**, not generated. Generated
questions cannot be compared across runs, and comparison over time is the
point. It lives in the repo (`docs/ax/rubric-architect.md`,
`docs/ax/rubric-builder.md`), so a change is a reviewed diff. Each file
carries `rubric: ax/<n>`. The architect skill references the version and
does not copy the text.

`ax/1`, architect side (the prototype's draft, with the owner's additions):

| Id | Question | Metric it feeds |
| --- | --- | --- |
| Q1 tool_failures | Which flk commands failed or needed a fallback? Command, exact error, count, fallback | `fallbacks` |
| Q2 polling | Where did you poll, sleep or re-read state instead of being told by an event, and which event would have replaced it? | `polls` |
| Q3 reconstruction | Which state did you rebuild by hand (files, registries, gates), and why? | `reconstructions` |
| Q4 sends | Which send path carried each brief and fix round (`delegate start`, `agent send --submit`, the `send-text` + Enter fallback)? Which did not take effect the first time? | `unsubmitted_sends` |
| Q5 owner_waits | Which decisions were slow or blocked on the owner, and could the plan or a brief have pre-decided them? | `owner_waits` |
| Q6 briefs | Per PR in the window: brief, rounds, blocker kinds, size against the budget. Which brief content would have prevented a round? | `rounds_total`, `prs`, `size_overruns` |
| Q7 builders | Where did builders ask back, go off-scope, or ignore an instruction (name it)? | `ignored_instructions` |
| Q8 context | What cost the most context, and which single change would have saved the most turns? | — |

The reply is one JSON object (`{"rubric", "window", "answers", "metrics",
"candidates"}`), then the sentinel line `DONE: ax/<n>`. "Unknown" is a valid
answer; a guess is not. Every answer cites evidence: a command, an exact
error, a log timestamp, a PR or issue number.

`ax/1`, builder side (asked at the builder's DONE, §4): which parts of the
brief were missing, ambiguous or wrong, and what you had to discover; which
instruction conflicted with finishing (the size budget is the known case) and
what you did; commands that failed (exact); tests you could not run, and
why; time spent on CI and guardrail gates; anything you would have asked if
asking were cheap. Metrics: `brief_gaps`, `ignored_instructions`,
`fallbacks`, `gate_failures`.

A version bump is needed for any change to a question's meaning. Metric keys
keep their meaning across versions where they can, so a trend can span a
bump.

### 3. The fork runs in place, labelled, and is reaped when it reports

- **In place, no worktree.** `agent.fork` gains `in_place: true` (CLI
  `--in-place`): the fork starts in the parent's cwd with no new branch, no
  linked worktree and no workspace, as a hidden or background pane. Variant B
  measured the same cache hit as the worktree fork. A read-only reviewer has
  nothing to commit, and a worktree per review would be one more thing for
  ADR-0027 §5 to reap.
- **Read-only by harness flag.** The in-place fork starts with the
  harness's own tool restrictions (Claude `--disallowedTools Edit Write
  NotebookEdit`, plus a hook that refuses `git commit` / `git push` in the
  shared cwd). This is defence in depth, not the only guard: the rubric says
  the same.
- **It is a labelled edge.** The fork is recorded as ADR-0027's
  `AgentDelegated { via: fork, label: "ax:<trigger>" }`, so it shows in
  `delegate.list` and lineage. The digest and the goal ladder ignore `ax:`
  edges, so a running review neither wakes nor parks the architect
  (ADR-0030 rung 5).
- **Reap on report.** The edge's reap policy is `on-exit`. When the fork
  prints its sentinel, or when its deadline passes (default 15 min), the
  server stops it and closes the edge. It owns no worktree, so nothing on
  disk is removed. Its transcript stays where the harness keeps it.
- **Counted against the ceiling.** It is an agent; ADR-0014's ceiling and
  #822's cap apply. A trigger that cannot start a fork under the cap is
  skipped and recorded as skipped, never queued behind builders.
- **At the turn boundary.** A trigger fires when the agent's `turn` is idle
  (ADR-0028 state), never mid-turn, so the fork copies a complete
  transcript and the cached prefix is fresh (measured C: the TTL is the
  profile's, and the next turn may never come).

### 4. Triggers: lifecycle, events, and on demand

| Kind | Trigger | Rubric | Default |
| --- | --- | --- | --- |
| Lifecycle | a landing batch or wave merged (ADR-0028 `pr.merged` for the batch PR, or the last edge of a wave reaped) | architect | on |
| Lifecycle | a release tagged | architect | on |
| Lifecycle | **before compaction**: a compaction is armed (ADR-0030 §6). The fork is started first and its first turn has read the transcript before the compaction proceeds, so it captures what the summary will lose | architect | on |
| Lifecycle | a milestone closed | architect | on |
| Lifecycle | a builder printed `DONE:` (before ADR-0027 §5 hibernates it) | builder | on |
| Event | N fallbacks of one class within a window, counted from ADR-0029 exit codes 8, 9 and 10 on the agent's own send-like calls (default 3 in 60 min) | architect, Q1 and Q4 only | on |
| Event | `dev` red (`pr.ci_completed` failure on the integration branch) | architect, Q1 and Q6 only | off |
| Event | a delegate stalled (ADR-0028 `agent.phase_changed` into a stall, or `delegate wait` exit 7) | architect, Q2 and Q7 only | off |
| On demand | `flk agent ask <agent> "<question>"` / MCP `flock_agent_ask` | none: the question is the pivot | — |

- **Rate limit:** at most one lifecycle review per agent per 30 minutes, and
  one per event class per 2 hours. A suppressed trigger is recorded as
  `suppressed` with its reason, so the trend can see it.
- **On demand** is the owner's "why is X slow?" or "what's blocking?". `flk
  agent ask` forks in place with the question as the pivot, waits on the
  fork's sentinel through a registered wait (ADR-0028 §5), prints the answer,
  and reaps the fork. It files nothing unless the question asks it to, and
  then under §6. The architect is not stopped and learns nothing of it.
- **The first phase needs no server code.** Until slice X5, the architect
  skill fires the lifecycle triggers itself: at the end of the boundary turn it
  calls `flock_agent_fork` on itself with the rubric pivot, then continues.

### 5. Answers are recorded as one durable event, and the full text as a file

- The fork ends by calling `flk ax record <answer.json>` (MCP
  `flock_ax_record`). The server appends one durable event:

  ```text
  AxReviewRecorded {
    run_id, subject: AgentId, role: architect | builder,
    trigger: <kind>, rubric: "ax/1", window: { from, to },
    metrics: { fallbacks, polls, reconstructions, unsubmitted_sends,
               owner_waits, rounds_total, prs, size_overruns,
               ignored_instructions, ... },
    candidates: [{ title, repo, confidence,
                   verdict: filed #n | commented #n | duplicate_of #n | dropped }],
    cost: { cache_read, cache_created, output, wall_ms },
    answers_ref: { path, sha256 },
  }
  ```

  The full answer JSON is stored under `state_dir()/ax/<run_id>.json` and
  served as an MCP resource (ADR-0017). One log, no second store (ADR-0005
  P5). The event carries counts and verdicts, not prose, so the log stays
  small.
- **Trends are a fold.** `flk ax report [--role] [--since] [--rubric]`
  prints metrics per run and per day, normalised per hour of the subject's
  turn time. There is no single "AX score" in `ax/1`: a weighted sum would
  hide which dimension moved, and the weights would be a guess. A score can
  be added once a few weeks of vectors show which metrics move together.
- **Summary out.** The fork's report (≤ 10 lines: the metrics, what it filed
  or commented, the top Q8 answer) goes to the operator's notification log
  (ADR-0016) and, as an `ax.review_recorded` kind, to any channel binding
  that subscribes (ADR-0028 §8, #1005). The architect receives it only as a
  digest line at its next turn boundary (ADR-0027 §3), never as a wake.

### 6. Filing: search first, comment rather than duplicate, dry-run until calibrated

The prototype's 4 duplicates out of 5 decide the order:

1. **Search first, every time.** Per candidate, a search over open **and
   closed** issues in the target repo (`gh issue list --state all --search`,
   3–5 keywords taken from the exact error or command), plus the issues the
   #987 survey already maps. The target repo follows the architect skill §6
   (flock, g-fleet, claude-config, watcher-s1).
2. **Match → comment, and only with new evidence.** A match that is open gets
   a comment only if the fork has evidence the issue lacks (a new command, an
   error, a version, a timestamp); a "+1" is never posted. A closed match
   whose fix has not reached the running version is reported in the summary
   ("fixed in #892, not rolled out here"), not reopened.
3. **New → file only above the bar.** File when the candidate has exact
   evidence (a command and its error, or log lines with timestamps), no
   search match, and a confidence of at least 0.8. Everything else goes to
   the summary as a `watch` item.
4. **Caps.** At most 3 new issues and 5 comments per fork run, at most 10 new
   issues per subject per day, and never two comments on one issue in a day.
   A capped candidate is recorded as `dropped: cap`.
5. **Never filed by a fork:** owner-reserved matters (policy, merge, release,
   deploy, a "your call") and findings about a specific person or PR author.
   Those are questions in the summary.
6. **Dry-run first.** Phase 1 records candidates and verdicts and files
   nothing. The architect (or the owner) reads the summary and files. Auto
   filing is switched on per repo (`[ax] file = "auto"`) once ten dry runs
   show the policy picking what a human would have filed.
7. **The System One gate is optional and starts in shadow.** A `choice`
   question (`duplicate` / `related` / `new`) over the candidate and its top
   five search hits can be asked and logged next to the search verdict. It
   gates nothing until its agreement with the human verdict is measured
   (`s1_rate`).

### 7. Builder briefs: acceptance and size first, rules as a shared layer

From §Context: the template is uniform, PR size predicts rounds best, the
prose budget is ignored, and correctness, claims, weak tests, docs and
platform gates are the recurring blockers. A brief line cannot fix judgement,
but it can turn the recurring blockers into checks the builder runs before
the reviewer does. The revised template, in this order:

```text
# <issue>: <slice title>       budget: <N> production lines · model: <m>

## Outcome            two sentences
## Acceptance         numbered; each one testable, with the exact command
  1. … (behaviour)
  n-1. Each new test fails on origin/dev (show it: run it before the fix).
  n. Size: `git diff --numstat origin/dev...HEAD -- src/` sums to ≤ N.
     Over N → do not open a PR; end with `BLOCKED: split proposal: …`.
## Context            files and functions; decisions (ADRs, owner rulings)
## Known traps        the repo's grown list, e.g. a fix that touches X must
                      test neighbour Y; a write path returns Err, never Ok,
                      when it drops data; grep docs/ for every removed flag
## Claims             the PR body states only what you ran; `Refs #n` unless
                      every acceptance item holds, then `Fixes #n`
## Out of scope
## Repo process       (shared layer, byte-identical across briefs)
## Report             ≤ 15 lines; last line DONE: <url> | BLOCKED: <reason>
```

- **The architect decides the split before dispatch,** not the builder
  mid-flight (skill §1 already says "split anything larger than ~400 diff
  lines before dispatch"). The budget in the brief is then a check on an
  estimate the architect made, and an overrun is a signal to re-plan, not a
  judgement call left to the builder's drive to finish.
- **The size check is an acceptance item with a command,** so "stop" has a
  concrete trigger the builder runs, not a prose instruction. A pre-push gate
  (slice X10) makes it binding where the brief alone is not. #1021's brief
  already uses the `BLOCKED: split proposal:` exit; X9 measures whether it
  holds.
- **Acceptance test first** is the "test fails on origin/dev" item. It targets
  the 52 % weak-test blocker and the tests that cannot fail.
- **Repo process and known traps are the shared layer.** They are identical
  for every builder in a repo and go into ADR-0032's stable layer, so N
  concurrent spawns share one cached prefix and only the issue-specific
  sections differ.
- **Fix rounds go to the same warm builder, on the same model.** Variant D
  shows why: a model switch costs the whole context again. The existing
  fix-brief shape (verdict, `file:line` + required behaviour, "merge, test,
  push, `DONE:`") converged in one round for 55 of 65 PRs and stays.
- **Send through the confirmed path only.** Briefs go out through `delegate
  start` or `agent send` with the ADR-0029 outcome checked. The
  `send-text` + `send-keys Enter` fallback leaves the template once C4b and
  C4d land, as ADR-0028 §9 already plans.

### 8. Builders answer the same kind of rubric from their side

A builder fork at DONE answers the builder rubric (§2) before ADR-0027 §5
hibernates the builder. That is the only place the brief's gaps can be seen
from the builder's side: the architect's fork sees rounds and blockers, the
builder's fork sees what it had to discover and which instruction it chose to
override. Both feed X9's brief comparison. Builder contexts are smaller than
the architect's, so the run costs less (the cache argument is the same).

### 9. How this composes

- **ADR-0027 / #632:** the fork is an edge with `via: fork`, label
  `ax:<trigger>`, reap policy `on-exit`, and no worktree. Its summary is a
  digest line.
- **ADR-0028:** triggers are `events.read` consumers (X5). The on-demand wait
  is a registered wait. `ax.review_recorded` is one more kind for channel
  bindings (#1005).
- **ADR-0029:** exit classes 8, 9 and 10 are what the fallback trigger counts.
  Without C1's registry there is nothing stable to count.
- **ADR-0030 / #573:** an `ax:` edge never parks or wakes the architect's
  goal (rung 5 ignores it). The pre-compaction trigger runs before ADR-0030
  §6's handshake.
- **ADR-0032 / #1042:** compiled layers make the brief's shared sections
  cache-stable, and they are the fallback warm start for harnesses without
  fork.
- **#1041:** the on-demand ask is one of the proactive interactions that
  issue lists: the operator starts it, and flk answers without disturbing
  the agent.

## Consequences

- Friction capture stops depending on the architect's spare context, and it
  survives compaction because the pre-compaction fork runs first.
- A review costs about one extra parent turn to start (≈ 1k created
  tokens), plus the fork's own answer turns, all at cached-read prices. The
  prototype's whole run was 0.56M cached-read, 16k created and 10k output
  tokens.
- AX answers become comparable over time per rubric version, as a metric
  vector per run in the event log.
- Issue volume is bounded by caps and a search, and the first phase files
  nothing on its own.
- Codex and opencode architects are not covered until X8, and then only with
  a cold, compiled-context reviewer.
- `agent.fork` gains an in-place mode. That is a second fork shape to keep
  correct, and it puts a second process on the parent's cwd (read-only, by
  flag and by rubric).

## Alternatives considered

- **The architect answers the rubric itself.** It spends the exact context the
  review exists to protect, and it competes with landing. Rejected.
- **A fresh reviewer agent reading the transcript.** A cold prefix (variant D
  is the cost shape: 17 % hit), and it sees the transcript, not the
  reasoning in context. Kept only as the non-Claude fallback (X8).
- **Generated questions per run.** They cannot be compared across runs, which
  is the point of recording them. Rejected for a versioned fixed rubric.
- **A cheaper model for the fork.** Measured more expensive than the same
  model warm (D). Rejected.
- **Fork into a worktree, as `agent.fork` does today.** Same cache cost, but a
  checkout to create and reap for a reviewer that writes nothing. Rejected
  for in place.
- **File on confidence alone.** The prototype's duplicates had confidence
  0.5–0.7, the same as its new candidate. Rejected for search-first.
- **One AX score from day one.** Weights would be invented. Deferred until
  the metric vectors show structure.
- **Keep the size budget as prose and trust the builder.** Measured: 1 of 4
  held. Rejected for an acceptance item with a command, a plan-time split and
  an optional gate.

## Risks

- **Rubric answers are self-report.** The prototype's stand-in had read the
  log rather than lived it and correctly said "unknown" where it lacked tool
  history. A real architect fork has the tool history, but it can still
  misremember. Evidence citations are mandatory so a reader can check them,
  and filing requires exact evidence.
- **The pre-compaction fork races the compaction.** The fork has to have read
  the transcript before the compaction rewrites the session. X5 orders it
  (fork started and first turn begun, then the compaction proceeds) and
  tests that order.
- **Stop hooks dominate wall time.** In the probe the profile's Stop hooks
  held each turn 2–4 minutes. The fork records and reports before it ends its
  turn, so the answer does not wait on settle.
- **A fork in the parent's cwd** could write despite the rubric. The harness
  flag and the commit/push hook are the guard. X2's acceptance includes a
  refused write.
- **Cache TTL is per account.** On a 5-minute-TTL account a delayed trigger
  pays full creation. Triggers fire at the turn boundary for that reason.

## Owner decisions requested

1. **Phase 1 is dry-run** (candidates recorded, nothing filed by the fork),
   and auto filing is enabled per repo after ten dry runs. Recommended: yes.
   Alternative: auto-file from day one with the caps.
2. **Rubric home:** `docs/ax/` in this repo, versioned, with the skill
   referencing the version. Recommended: yes. Alternative: inside the
   architect skill.
3. **Trigger defaults** as in §4 (lifecycle on, fallback-count on, dev-red and
   stall off). Recommended: yes.
4. **Caps** (3 new issues and 5 comments per run, 10 new per day) and the 0.8
   confidence bar. Recommended: yes, revisit after the dry runs.
5. **Brief template** (§7) for the next wave, with the architect deciding
   splits before dispatch. Recommended: yes. Alternatively keep the current
   template for half the briefs, which is what X9 needs.
6. **Size enforcement:** acceptance item now, pre-push gate X10 only if X9
   shows the item alone does not hold. Recommended: yes.

## Test strategy

- **Fork cost:** `tests/probes/fork_cache.py` re-runs the table above against
  a build (in place and through the verb), and X2's acceptance repeats it for
  `--in-place`.
- **In-place fork:** an integration test over the socket with the fake Claude
  fixture: no worktree, no workspace, the edge labelled `ax:*`, a write
  refused, reaped on sentinel and on deadline.
- **Record and report:** a property test that folds random
  `AxReviewRecorded` sequences across rotations into the same `flk ax
  report`, and a reader check that the previous release skips the new kind
  (ADR-0028 §3a).
- **Filing policy:** a fake `gh` over a fixture issue list, checking that the
  search runs before every candidate, the caps hold, a closed match is
  reported and not reopened, and dry-run files nothing.
- **Brief template:** X9's comparison, not a unit test.

## 10. Slices

Each is sized at about 400 diff lines or less and is independently useful.
They are not filed; the architect files them after the owner accepts.

| # | Slice | Depends on | Acceptance |
| --- | --- | --- | --- |
| X1 | Rubric files `docs/ax/rubric-architect.md` and `rubric-builder.md` (`ax/1`); architect skill: self-fork via `flock_agent_fork` at boundaries, dry-run, summary into HANDOFF; brief template v2 (§7) in the skill | — | A dry run on a real architect session produces the JSON and the summary; the skill names the rubric version |
| X2 | `agent.fork --in-place`: parent cwd, no branch, worktree or workspace; read-only harness flags; edge `via: fork` with `label`; reap policy `on-exit` with a deadline | ADR-0027 slice 1 | Fake Claude: no worktree created, a write refused, the fork reaped on sentinel and on deadline; `fork_cache.py` shows the same hit as the verb |
| X3 | `AxReviewRecorded` kind, `state_dir()/ax/<run>.json` as an MCP resource, `flk ax record`, `flk ax report` | ADR-0028 E3's reader check | Folding is rotation-proof; the last release skips the kind; `ax report` shows per-run metrics |
| X4 | `flk agent ask <agent> "<q>"` / MCP `flock_agent_ask`: fork in place, registered wait on the sentinel, print, reap | X2, ADR-0028 E5 | The answer prints, the parent's transcript is untouched, the fork is gone afterwards; refused for a non-Claude target with `fork_unsupported` |
| X5 | Server-side triggers: `[ax]` config, lifecycle and event triggers from `events.read`, fire at `turn: idle`, rate limits, `suppressed` and `skipped` records, pre-compaction ordering | X2, X3, ADR-0028 E3 / E4a, ADR-0030 G5 | A merged batch fires one review; a second within 30 min is `suppressed`; an armed compaction waits for the fork's first turn |
| X6 | Filing policy in a helper (`flk ax file`, or ADR-0015's filing path): search-first, comment-with-new-evidence, caps, dry-run vs auto per repo, optional System One shadow verdict | X3 | Fake `gh`: the search precedes every candidate, caps hold, dry-run files nothing, a closed match is reported, not reopened |
| X7 | Builder fork at DONE with the builder rubric, before hibernate | X2, ADR-0027 slice 5 | A builder printing `DONE:` yields one builder `AxReviewRecorded` before it hibernates |
| X8 | Non-Claude fallback: a fresh reviewer from ADR-0032's compiled context plus the transcript tail | ADR-0032's compile slice | A Codex architect gets a review, marked `cold: true`, with its cache cost recorded |
| X9 | Brief comparison: tag each brief with its template version; `flk ax report --briefs` joins rounds, blocker kinds, size and builder-rubric answers per version | X1, X3, X7 | After one wave, a table of zero-round share and size overrun per template version |
| X10 | Optional pre-push size gate for builder worktrees: production lines against a budget the brief declares | X9 (only if the acceptance item alone does not hold) | A push over budget is refused with the split instruction |

**X1 ships alone and needs no flk code.** With it the architect gets
fork-based reviews today through the existing `flock_agent_fork` (at the cost
of a worktree per review), dry-run only, plus the new brief template. X2 and
X3 make it cheap and comparable, X5 takes the triggers away from the
architect, and X6 turns on filing.
