# ADR 0033 — Agent-experience reviews are answered by a fork of the warm agent, against a versioned rubric, recorded as events, and filed only after a search; builder briefs put acceptance and size first

- Status: Accepted
- Date: 2026-10-11 (proposed); accepted 2026-10-12
- Issues: #1043 (the spike this records). Composes with ADR-0027 (delegate
  edges, #632), ADR-0028 (events, registered waits, channels, #1005),
  ADR-0029 (exit-code registry), ADR-0030 (goal supervision, #573),
  ADR-0032 (compiled context, #1042, Proposed on its own PR), ADR-0014
  (spawn ceiling), ADR-0015 / ADR-0023 (filing), #987 (toolchain survey),
  #1041 (proactive interactions).
- Decision owner: operator.
- Scope of acceptance: the owner accepted all six owner decisions as
  recommended on 2026-10-12 (see "Owner decisions"). The acceptance claims
  the decisions, not code.
- Implemented: none. The measurement runbook is `tests/probes/fork_cache.py`.
  The slices (§10) are not filed yet; the architect files them now that the
  owner has accepted.

## Context

### What the architect does today, by hand

The architect skill makes friction reporting "half the job" (architect skill §6): record
tooling friction with exact commands and errors in HANDOFF, then file in
batches after searching. In practice that happens when the architect has
spare context, which it rarely does. The flock fleet's own state directory
shows the result. Its 800-plus-line `STATUS.md` mentions friction on six lines,
and the rest of it is scattered through status lines ("send
accepted", "my wait loop matched 'DONE: http…' inside a recap line", "3 of 4
slice builders overran 400 lines"). Filing is a side task, competing with
landing for the scarcest resource in the system, which is the architect's
context (architect skill §0). Every compaction then loses whatever was not written
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
  The cache is per model, so a fork onto any other model, cheaper ones
  included, pays the same miss. D measured the miss onto a dearer model.
  That a cheaper cold fork costs more than the same model warm is inferred
  from the miss and list prices, not measured.
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
| After the search | 4 of 5 match existing issues: #892 (shipped, not yet rolled out), #639 / #911 (shipped), #663 / #819 (open, covered by ADR-0028 E1b, E3 and E7), claude-config#49 (closed). The fifth (builders ignore the size budget) is new but is exactly §7 of this ADR |

**At real architect sizes.** The probe ran at 84k and 136k tokens; ADR-0032
records real architect contexts of 210k–637k before compaction. Every call a
fork makes re-reads the whole prefix, so the cost scales linearly with the
context and with the number of calls. In ADR-0032's units (input-token
equivalents: cache read × 0.1, 1h creation × 2):

| Context | First turn | Whole run, 4 calls as in the prototype | Whole run, 2 calls (rubric inline in the pivot, searches batched in one shell call) |
| --- | --- | --- | --- |
| 136k (measured) | 16k units | 89k units + 10k output | ≈ 40k units |
| 400k | ≈ 43k units | ≈ 200k units + 10k output | ≈ 95k units |
| 637k | ≈ 66k units | ≈ 290k units + 10k output | ≈ 140k units |

Everything except the first row's first two cells is extrapolated from the
measured per-call shape (≈ 1–7k created per call, the rest read), not
measured. The two-call column assumes ≈ 6.5k created in total. Two consequences:
the fork's rubric goes inline in the pivot and its searches run in one call
(§3), and above a context size the compiled reviewer is cheaper (§1).

So the cost is small and the answers were grounded, but **filing without a
search would have produced 4 duplicates out of 5**, and a confidence of
0.5–0.7 did not separate them from the new one. That sets the filing policy
(§6).

The same probe also hit friction worth recording. A guarded `agent send
--submit` of a one-paragraph prompt returned `unconfirmed /
owned_composer_not_visible` with the text left in the composer. That is
the situation ADR-0029's new `typed_not_submitted` reason is meant to name,
reported under an older reason. And `flk agent send <target> <text> --submit`
on 1.2.0 typed `--submit` into the composer as text: the flag is recognised
before the target or right after it, never after the text.

### Brief styles against review rounds

Data: every brief in the fleet's `briefs/` directory (256 files at the time of the analysis), mapped
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
- **PR size is the clearest signal we have.** Small PRs do best; above 250
  lines the buckets are close (n = 29 and 52), so this is not a gradient:

  | PR diff (add + del) | n | zero-round |
  | --- | --- | --- |
  | < 60 | 5 | 5 of 5 |
  | 60–250 | 29 | 76 % |
  | 250–600 | 29 | 24 % |
  | > 600 | 52 | 31 % |

  Median diff by rounds: 227 lines (0 rounds), 615 (1), 1,263 (2), 1,790 (3+).
- **The prose size budget does not hold.** Five slice briefs carried "at
  most ~400 diff lines; if it grows past that, stop and report the split"
  (#1017, #1018, #1019, #1020, #1028). Two held: #1017's builder split
  before opening its PR (#1048, 375 lines, the rest announced as a
  follow-up), and #1018's PR (#1060) came in at 84 lines. Three opened one PR
  over the budget with a split offered in the body: #1050 (514), #1056 (558
  at open, 775 after fixes; the reviewer ruled "don't split") and #1057 (592;
  split only after a sequencing blocker). The owner's count of three overruns
  is confirmed, with one correction: the list on the issue names #1048, which
  is #1017's split, and the third overrun is #1057. Builders almost
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
§10 slice AX10 makes the comparison measurable from here on.

## Decision

### 1. An AX review is answered by a fork of the agent under review, never by the agent itself

- The reviewer is a fork of the warm session (Claude `--fork-session`). It
  shares the parent's context up to the fork point, at the parent's model
  (measured variant D). It answers, searches, files within the policy (§6),
  reports, and exits.
- The fork is **read-only toward the work.** It does not dispatch, merge,
  message delegates, type into panes or edit files. Its only side effects are
  issue searches, filings and comments within §6, one `AxReviewRecorded`
  event (§5), and its report.
- **Harnesses without fork** (Codex, opencode) get no AX fork. They fall back
  to ADR-0032's compiled reviewer (its slice X7: layers 1–2 from cache, the
  compiled state as the task) plus the agent's transcript tail. That is
  slice AX9.
- **Above a context size the compiled reviewer is used instead.** A fork's
  cost grows with the parent's context (≈ 95k units for a two-call run at
  400k, ≈ 140k at 637k); ADR-0032 measured a compiled architect resume at 58k–77k units,
  independent of how long the architect has run. The compiled reviewer sees
  the state, not the lived friction, so the fork stays the default, and
  `[ax] fork_max_context` (default 400k tokens) switches a trigger to the
  compiled reviewer above it. The pre-compaction trigger is exempt: that
  review exists precisely to capture what only the context holds.

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
- **Two calls, not four.** The rubric text goes inline in the pivot rather
  than as a file to read, and the candidate searches run in one shell call.
  Each call re-reads the whole prefix, so this halves the cost at any size.
- **Read-only by a command allowlist, not by a tool flag alone.** Claude's
  `--disallowedTools Edit Write NotebookEdit` removes the edit tools, but the
  fork needs a shell for `gh`, and a shell can write. So the in-place fork
  also gets a PreToolUse hook that allows only the commands its job needs
  (`gh issue list|view|create|comment`, `flk ax record`, read-only `git`,
  `cat`, `grep`, `rg`) and refuses the rest. The answer goes to `flk ax
  record -` on stdin, so the fork never needs to write a file. The rubric
  states the same rule; the hook is what enforces it.
- **It is a labelled edge.** The fork is recorded as ADR-0027's
  `AgentDelegated { via: fork, label: "ax:<trigger>" }`, so it shows in
  `delegate.list` and lineage. The digest and the goal ladder ignore `ax:`
  edges, so a running review neither wakes nor parks the architect
  (ADR-0030 rung 5).
- **A server-started fork still gets an edge.** ADR-0027 §1 records an edge
  only for an attested agent caller, and a trigger (AX6a) is the server, not
  an agent. For a fork the server starts on its own trigger, the edge's
  architect is the forked subject itself, with `label: "ax:<trigger>"`. The
  server records it; it is never a parameter, so ADR-0027's spoofing argument
  still holds.
- **Reap on report.** The edge's reap policy is `on-exit`, a new value
  that extends ADR-0027 §5's `merged | never`. When the fork
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
| Event | a delegate stalled (ADR-0029 exit 7 from the architect's `delegate wait` on it; ADR-0028 has no stall phase) | architect, Q2 and Q7 only | off |
| On demand | `flk agent ask <agent> "<question>"` / MCP `flock_agent_ask` | none: the question is the pivot | — |

- **Rate limit:** at most one lifecycle review per agent per 30 minutes, and
  one per event class per 2 hours. A suppressed trigger is recorded as
  `suppressed` with its reason, so the trend can see it.
- **On demand** is the owner's "why is X slow?" or "what's blocking?". `flk
  agent ask` forks in place with the question as the pivot, waits on the
  fork's sentinel through a registered wait (ADR-0028 §5), prints the answer,
  and reaps the fork. It files nothing unless the question asks it to, and
  then under §6. The architect is not stopped and learns nothing of it.
- **The first phase needs no server code.** Until slice AX6a, the architect
  skill fires the lifecycle triggers itself: at the end of the boundary turn it
  calls `flock_agent_fork` on itself with the rubric pivot, then continues.

### 5. Answers are recorded as one durable event, and the full text as a file

- The fork ends by piping its answer JSON to `flk ax record -` (MCP
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
   #987 survey already maps. The target repo follows architect skill §6
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

### 7. Builder briefs: acceptance and size first

From §Context: the template is uniform, PR size predicts rounds best, the
prose budget is ignored, and correctness, claims, weak tests, docs and
platform gates are the recurring blockers. A brief line cannot fix judgement,
but it can turn the recurring blockers into checks the builder runs before
the reviewer does.

**Placement is ADR-0032's.** ADR-0032 §5 compiles a brief down to its task
layer (layer 3) and moves what every builder shares out of the brief: the
shared rules to layer 1, the repo's known traps to layer 2 (its owner
question 3: an authored traps file). This ADR does not decide placement. It
changes only what the task section asks for, in this order:

```text
# <issue>: <slice title>       budget: <N> production lines · model: <m>

## Outcome            two sentences
## Acceptance         numbered; each one testable, with the exact command
  1. … (behaviour)
  n-1. Each new test fails on origin/dev (show it: run it before the fix).
  n. Size: `git diff --numstat origin/dev...HEAD -- src/` sums to ≤ N.
     Over N → do not open a PR; end with `BLOCKED: split proposal: …`.
## Context            files and functions; decisions (ADRs, owner rulings)
## Claims             the PR body states only what you ran; `Refs #n` unless
                      every acceptance item holds, then `Fixes #n`
## Out of scope
## Report             ≤ 15 lines; last line DONE: <url> | BLOCKED: <reason>
```

- **The architect decides the split before dispatch,** not the builder
  mid-flight (architect skill §1 already says "split anything larger than ~400 diff
  lines before dispatch"). The budget in the brief is then a check on an
  estimate the architect made, and an overrun is a signal to re-plan, not a
  judgement call left to the builder's drive to finish.
- **The size check is an acceptance item with a command,** so "stop" has a
  concrete trigger the builder runs, not a prose instruction. A pre-push gate
  (slice AX11) makes it binding where the brief alone is not. #1021's brief
  already uses the `BLOCKED: split proposal:` exit; AX10 measures whether it
  holds.
- **Acceptance test first** is the "test fails on origin/dev" item. It targets
  the 52 % weak-test blocker and the tests that cannot fail.
- **Known traps the analysis found** go into ADR-0032's traps file (layer
  2), not into each brief: a fix that touches one path must test its
  neighbour; a write path that drops data returns `Err`, never `Ok`; every
  removed key or flag is grepped out of `docs/`; `Fixes` only when every
  acceptance item holds; `just check` on Linux before a PR that touches
  platform code.
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
override. Both feed AX10's brief comparison. Builder contexts are smaller than
the architect's, so the run costs less (the cache argument is the same).

### 9. How this composes

- **ADR-0027 / #632:** the fork is an edge with `via: fork`, label
  `ax:<trigger>`, reap policy `on-exit`, and no worktree. Its summary is a
  digest line.
- **ADR-0028:** triggers are `events.read` consumers (AX6a, AX6b). The on-demand wait
  is a registered wait. `ax.review_recorded` is one more kind for channel
  bindings (#1005).
- **ADR-0029:** exit classes 8, 9 and 10 are what the fallback trigger counts.
  Without C1's registry there is nothing stable to count.
- **ADR-0030 / #573:** an `ax:` edge never parks or wakes the architect's
  goal (rung 5 ignores it). The pre-compaction trigger runs before ADR-0030
  §6's handshake.
- **#1044 heartbeat:** the hard-send is for agents with an armed goal, and
  skips `ax:` forks: a fork has no goal and is bounded by its own deadline
  (§3). The two never write into the same pane, because an AX trigger types
  nothing into the architect; it starts a separate process from the
  transcript. They still meet on an idle architect, so two rules apply. A
  fork's activity never counts as the architect's liveness, so a running
  review does not postpone the heartbeat. And an AX trigger that fires at
  the turn boundary holds a per-subject snapshot lease until the fork's first
  API call (measured: about 2 s), during which a due heartbeat waits; the
  heartbeat then sends as usual. Neither suppresses the other. When the
  heartbeat and the pre-compaction trigger are both due, ADR-0028 §5's order
  stands (compaction before a goal continuation), and the fork starts before
  either, as §4 requires.
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
  tokens (89k units at 136k context); at a real 400k context a two-call run
  is about 95k units.
- AX answers become comparable over time per rubric version, as a metric
  vector per run in the event log.
- Issue volume is bounded by caps and a search, and the first phase files
  nothing on its own.
- Codex and opencode architects are not covered until AX9, and then only with
  a compiled-context reviewer that sees state, not turns.
- `agent.fork` gains an in-place mode. That is a second fork shape to keep
  correct, and it puts a second process on the parent's cwd (read-only, by
  flag and by rubric).

## Alternatives considered

- **The architect answers the rubric itself.** It spends the exact context the
  review exists to protect, and it competes with landing. Rejected.
- **A fresh reviewer agent reading the transcript.** A cold prefix (variant D
  is the cost shape: 17 % hit) the size of the transcript, so at 400k it costs
  several times the fork, and it sees the transcript, not the reasoning in
  context. Rejected.
- **ADR-0032's compiled reviewer (its X7) for every review.** Layers 1–2 come
  from cache and the cost does not grow with the architect's context, so it
  is cheaper than a two-call fork above roughly 250k–350k tokens of
  architect context. But it starts from
  derived state, and the friction this review is for (a fallback that worked,
  a poll that should have been an event) lives in the turns, not in the
  state. Kept as the non-Claude fallback and as the choice above `[ax]
  fork_max_context` (§1), not as the default.
- **Generated questions per run.** They cannot be compared across runs, which
  is the point of recording them. Rejected for a versioned fixed rubric.
- **A cheaper model for the fork.** The cache is per model, so it starts
  cold (D measured the miss); that it costs more than the same model warm is
  inferred from the miss and list prices. Rejected.
- **Fork into a worktree, as `agent.fork` does today.** Same cache cost, but a
  checkout to create and reap for a reviewer that writes nothing. Rejected
  for in place.
- **File on confidence alone.** The prototype's duplicates had confidence
  0.5–0.7, the same as its new candidate. Rejected for search-first.
- **One AX score from day one.** Weights would be invented. Deferred until
  the metric vectors show structure.
- **Keep the size budget as prose and trust the builder.** Measured: 2 of 5
  held, and one of those two was an 84-line change that never came near it. Rejected for an acceptance item with a command, a plan-time split and
  an optional gate.

## Risks

- **Rubric answers are self-report.** The prototype's stand-in had read the
  log rather than lived it and correctly said "unknown" where it lacked tool
  history. A real architect fork has the tool history, but it can still
  misremember. Evidence citations are mandatory so a reader can check them,
  and filing requires exact evidence.
- **The pre-compaction fork races the compaction.** The fork has to have read
  the transcript before the compaction rewrites the session. AX6c orders it
  (fork started and first turn begun, then the compaction proceeds) and
  tests that order.
- **Stop hooks dominate wall time.** In the probe the profile's Stop hooks
  held each turn 2–4 minutes. The fork records and reports before it ends its
  turn, so the answer does not wait on settle.
- **A fork in the parent's cwd** could write despite the rubric. The harness
  flag and the command-allowlist hook are the guard. AX3's acceptance includes a
  refused write.
- **Cache TTL is per account.** On a 5-minute-TTL account a delayed trigger
  pays full creation. Triggers fire at the turn boundary for that reason.

## Owner decisions

Decided 2026-10-12: the owner accepted all six as recommended. Each item
keeps the recommendation and the alternative it was weighed against.

1. **Phase 1 is dry-run** (candidates recorded, nothing filed by the fork),
   and auto filing is enabled per repo after ten dry runs. Recommended: yes.
   Alternative: auto-file from day one with the caps.
   **Decided: as recommended.**
2. **Rubric home:** `docs/ax/` in this repo, versioned, with the skill
   referencing the version. Recommended: yes. Alternative: inside the
   architect skill. **Decided: as recommended.**
3. **Trigger defaults** as in §4 (lifecycle on, fallback-count on, dev-red and
   stall off). Recommended: yes. **Decided: as recommended.**
4. **Caps** (3 new issues and 5 comments per run, 10 new per day) and the 0.8
   confidence bar. Recommended: yes, revisit after the dry runs.
   **Decided: as recommended.**
5. **Brief template** (§7) for the next wave, with the architect deciding
   splits before dispatch. Recommended: v2 for half of the next wave's
   briefs and the current template for the other half, so AX10 has a
   control arm; every brief moves to v2 after that wave. Alternative: v2 for
   every brief at once, which leaves no comparison.
   **Decided: as recommended.**
6. **Size enforcement:** acceptance item now, pre-push gate AX11 only if AX10
   shows the item alone does not hold. Recommended: yes.
   **Decided: as recommended.**

## Test strategy

- **Fork cost:** `tests/probes/fork_cache.py` re-runs the table above against
  a build (in place and through the verb), and AX3's acceptance repeats it for
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
- **Brief template:** AX10's comparison, not a unit test.

## 10. Slices

Each is sized below, in estimated production lines (docs and tests
excluded), and the two that came out over ~400 are split. They are not
filed yet; the owner accepted on 2026-10-12, and the architect files them.

| # | Slice | Size (est.) | Depends on | Acceptance |
| --- | --- | --- | --- | --- |
| AX1 | Rubric files `docs/ax/rubric-architect.md` and `rubric-builder.md` (`ax/1`); architect skill: self-fork via `flock_agent_fork` at boundaries with the rubric inline, dry-run, summary into HANDOFF, reap the fork's worktree by hand; the §7 task-section changes in the skill | 0 code (~250 docs) | — | A dry run on a real architect session produces the JSON and the summary; the skill names the rubric version |
| AX2 | `ax:` edges: `on-exit` reap policy (extends ADR-0027 §5); the digest and ADR-0030's rung 5 ignore edges labelled `ax:`; server-started forks record an edge attributed to the subject | ~200 | ADR-0027 slices 1 and 3 | An `ax:` fork neither adds a digest line nor parks the architect; it closes when the fork exits |
| AX3 | `agent.fork --in-place`: parent cwd, no branch, worktree or workspace; `--disallowedTools` plus the command-allowlist hook; deadline | ~350 | AX2 | Fake Claude: no worktree created, a shell `touch` in the cwd refused, reaped on sentinel and on deadline; `fork_cache.py` shows the same hit as the verb |
| AX4a | `AxReviewRecorded` kind, `flk ax record -` (stdin), `state_dir()/ax/<run>.json` served as an MCP resource | ~300 | ADR-0028 E3's reader check | A record round-trips; the last release skips the kind; the resource resolves by `run_id` |
| AX4b | `flk ax report [--role] [--since] [--rubric]`: rotation-proof fold, per-run and per-day metrics, normalised per turn hour | ~250 | AX4a | Property test: random records across rotations fold to the same report |
| AX5 | `flk agent ask <agent> "<q>"` / MCP `flock_agent_ask`: fork in place, registered wait on the sentinel, print, reap | ~250 | AX3, ADR-0028 E5 | The answer prints, the parent's transcript is untouched, the fork is gone afterwards; refused for a non-Claude target with `fork_unsupported` |
| AX6a | Trigger engine: `[ax]` config, lifecycle triggers (batch merged, release, milestone, builder `DONE:`), firing at `turn: idle`, rate limits, `suppressed` / `skipped` records, `fork_max_context` cutover | ~350 | AX3, AX4a, ADR-0028 E3 / E4a / E7 | A merged batch fires one review; a second within 30 min is `suppressed`; a context over the limit routes to the compiled reviewer (or `skipped` before AX9) |
| AX6b | Event triggers: fallback count from ADR-0029 exit classes 8–10 per window, `dev` red from `pr.ci_completed`, stalled delegate from `delegate wait` exit 7 | ~250 | AX6a, ADR-0029 C1, ADR-0028 E7 | Three exit-8 sends within the window fire one Q1/Q4 review; defaults off stay off |
| AX6c | Pre-compaction ordering with ADR-0030's handshake, and the per-subject snapshot lease shared with #1044's heartbeat | ~200 | AX6a, ADR-0030 G5, #1044 | An armed compaction waits for the fork's first call; a due heartbeat waits out the lease and then sends; fork activity does not reset liveness |
| AX7 | Filing policy in a helper (`flk ax file`, or ADR-0015's filing path): search-first, comment-with-new-evidence, caps, dry-run vs auto per repo, optional System One shadow verdict | ~350 | AX4a | Fake `gh`: the search precedes every candidate, caps hold, dry-run files nothing, a closed match is reported, not reopened |
| AX8 | Builder fork at DONE with the builder rubric, before hibernate | ~150 | AX3, AX4a, ADR-0027 slice 5 | A builder printing `DONE:` yields one builder `AxReviewRecorded` before it hibernates |
| AX9 | Compiled-reviewer path: AX triggers use ADR-0032's reviewer role for non-Claude subjects and above `fork_max_context`, plus the transcript tail | ~150 | ADR-0032 X7, AX6a | A Codex architect gets a review, marked `compiled: true`, with its cost recorded |
| AX10 | Brief comparison: tag each brief with its template version; `flk ax report --briefs` joins rounds, blocker kinds, size and builder-rubric answers per version | ~250 | AX1, AX4b, AX8 | After one wave, a table of zero-round share and size overrun per template version |
| AX11 | Optional pre-push size gate for builder worktrees: production lines against a budget the brief declares | ~100 | AX10 (only if the acceptance item alone does not hold) | A push over budget is refused with the split instruction |

The prefix is `AX` so these do not collide with ADR-0032's own `X` slices.

**AX1 ships alone and needs no flk code.** It is deliberately rough: the
self-fork goes through today's `flock_agent_fork`, so each review creates a
worktree the architect must reap (`flk worktree kill`) and appears in the
digest as an ordinary delegate. Dry-run only, plus the new brief template.
AX2 and AX3 make it clean and cheap, AX4a/b make it comparable, AX6a–c take the
triggers away from the architect, and AX7 turns on filing.
