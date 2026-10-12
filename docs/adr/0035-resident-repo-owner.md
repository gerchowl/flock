# ADR 0035 — A repo has one resident owner: a hibernated agent that flk wakes on events it has not handled, that triages before it accepts, delegates through edges, and talks to other owners through mail

- Status: Proposed
- Date: 2026-10-12
- Issues: #1172 (the spike this records). Subsumes and orders the spikes
  #1073 (cron agents), #1074 (event triggers and hook/script surface) and
  #1078 (watch surface and cross-repo blocking); §5 answers them. Composes with
  #1160 (harness hooks and question routing), which another spike is writing
  in parallel as its own ADR. This record references that ADR's decisions and
  makes none of its own about hook mechanics or question routing. Answers
  #1167 (architects of architects) for its identity, intake and protocol
  parts (§1a); its dry run stays #1167's.
- Revised: 2026-10-12, round 2. Adds the owner's daily standup and
  transcript-driven improvement (§7a, §7b, owner addition on #1172) and
  two-level coordination inside one repo (§1a, #1167).
- Builds on: ADR-0004 (per-repo `.flk.toml`), ADR-0005 (durable log), ADR-0006
  (addressing), ADR-0014 (agent spawn ceiling, operator boundary), ADR-0018 /
  ADR-0026 (mail intent and custody), ADR-0023 (decision records), ADR-0027
  (delegate edges), ADR-0028 (events, waits, wakes, channels), ADR-0029 (CLI
  contract), ADR-0030 / ADR-0031 (goal supervision and storage), ADR-0032
  (compiled context), ADR-0033 (forked AX reviews).
- Numbering: 0034 is left to the #1160 ADR (`plover-1160`, started first, no
  PR open when this was written). If that ADR takes another number, this one
  is renumbered before acceptance.
- Decision owner: operator.
- Implemented: none. The slices (§9) are not filed. The owner reviews this ADR
  before any slicing.

## Context

### The owner's vision (2026-10-12, #1172, verbatim)

> an architect/owner agent per repo that runs under flk that gets woken on msg
> and or new gh issues/prs/.. sth active that he has not worked on -- unless he
> is still working -- but even then, getting the insights delivered -- rather
> than itself polling and wasting tokens -- so it can plan the work -- it
> itself should hold its repo dear and not destroy it by taking any
> isse/request -- i.e. spike it -- communicate with a requesting architect and
> become part of a software team

### The owner's addition (2026-10-12, #1172, verbatim)

> it could also provide a daily standup -- of what has been achieved, on what
> is planned, what is pending/blockers/experiences/wishes for it to run
> smoother?! -- i.e. looking always into the transcripts of itself/the
> delegates on what tools were expensive, which could e.g. be lifted into just
> scripts and advertised in the claude.md of a repo -- or the general claude
> config?

The owner scoped it on the same comment. The standup's sources are durable
events, edges, `gh` and the goal record, never whole transcripts. It is
delivered through a channel or a digest, never typed into the composer
(#1162). Improvement proposals are filed for a human to approve. They never
change another repo's configuration on their own. They reuse ADR-0033 and the
survey method. Transcripts stay local, and reports quote patterns, not
content.

### What runs today: a hand-built owner

The flock fleet's architect *is* this vision, built by hand around one Claude
session. Its pieces (read-only inventory of
`flock-worktrees/.fleet/` and the `architect` skill on 2026-10-12):

| Piece | What it does | How |
| --- | --- | --- |
| `supervisor.py` | Every 90 s (`POLL`, line 20) it polls `flk agent get` per delegate, `gh pr view` per PR, starts one `watcher-s1 … gh pr checks --watch` per PR head (lines 251–288), reaps on merge (118–173), re-injects a per-delegate `[goal]` up to 3 times (215–239), lists untriaged issues every 30 min (291–301, 342–346), scrapes `ctx N%` off the status line and drives a three-phase `/compact` (179–207), and types the queued lines into the architect's pane as `[fleet] …` with `flk agent send --submit` (77–110) | polling, screen scraping, typing |
| `landbatches.sh` | Lands batches: `gh pr view` / `gh pr checks` every 60 s, one automatic rerun, `update-branch` when behind (lines 286–311) | polling |
| `GOAL.md` | Frozen goal, merge policy ("Merge into `dev` only after reviewer approval and green CI", line 44), "no applies, no handoffs", cap of 3–4 agents, amendments by the owner | hand-written file |
| `STATUS.md` | Mutable state, in flight, landing queue, next action, owner decisions in force | rewritten by the architect |
| `tasks.json`, `triage.json`, `agents.json` | Gated checklist, triage skips, the delegate registry | hand-kept JSON |
| `architect` skill | Plan in waves, write briefs, dispatch delegates under a cap, review with a fresh reviewer, land serially, compact at landing boundaries, file friction (`~/.claude-profiles/priv/skills/architect/SKILL.md` §0–§9) | prompt |

Two failures of that build are on record. A `[fleet]` digest got glued onto
the operator's half-typed message (#1162). And everything dies with the
session and the shell that started the poller (ADR-0027 Context). The
architect is also never off: it is a live session that every `[fleet]` line
turns into a turn on a context of 210k–637k tokens before each compaction
(ADR-0032 Context).

### What flk already provides (verified at `d63ec5d6`)

- **A durable log with cursors.** `EventHub` persists to `event-log.jsonl`
  and rotates at 32 MiB or 100,000 events, keeping four rotated files
  (`src/api/event_hub.rs:77-79`). `events.read` returns `seq > after` with an
  explicit `gap` and a condvar long-poll of up to 540 s
  (`src/api/events_read.rs:51`, `src/api/schema.rs:2108-2110`). `flk events
  read|wait` exist (`src/cli/events.rs:19-21`).
- **A batched PR poller.** It sends one GraphQL request for every `(repo,
  branch)` pair with a worktree (`src/pr_poll.rs:91`, chunked at 100 aliases,
  `:48`) on a fixed 120 s tick (`src/app/mod.rs:485`). It emits
  `pr.ci_completed` once per head SHA and `pr.merged` / `pr.closed`
  (`src/pr_poll.rs:237-320`). It classifies rate limiting
  (`src/github/graphql.rs:80-83`) but does not back off.
- **An issue poller with an author gate.** `issue_guard` runs `gh issue list`
  per configured repo every 60 s (`src/checks/config.rs:42`,
  `src/app/runtime.rs:1411`). It acts only on `flk-trigger` blocks in issue
  bodies written by the configured owner, never on comments
  (`src/checks/issue_guard.rs:1-35`). It emits no "new issue" event.
- **Cron with inert actions.** `[[checks.cron]]` (`src/checks/config.rs:143`,
  `src/checks/runner.rs:275`) emits `CronFired`. Actions are a closed set:
  `Notify`, `Event`, `WriteDigest` (`src/checks/config.rs:273-294`).
- **Delegate edges.** `AgentDelegated` is folded and re-issued on start
  (`src/app/delegation.rs:178`, `:233`). `delegate.list` exists
  (`src/app/api/delegates.rs:19`). The SessionStart hook returns the delegate
  table as `additionalContext` (`src/cli/hook.rs:308-349`).
- **Wakes.** The mail idle wake types one fixed sentence, for Claude only
  (`src/app/idle_wake.rs:35`). The Stop hook's mail nudge is a
  `decision: block` that carries a count, never a body
  (`src/cli/hook.rs:222-235`, `:695`). The phase `waiting_on_event` exists
  but is never produced: the only caller hardcodes `wait_open = false`
  (`src/app/api_helpers.rs:143`). The wait registry is E5 (#1031, open) and
  wake-follows-wait is E6 (#1032, open). The digest is #961 (open).
- **Hibernate, resume, restart.** `agent.hibernate` / `agent.resume`
  (`src/app/hibernation.rs:72`, `:164`). The resume argv per harness is in
  `src/agent_resume.rs:123`. A restart keeps `terminal.agent_id`
  (`src/app/agent_restart.rs:1144`).
- **Spawn ceiling and pause.** Agent-initiated spawns are bounded by depth 2,
  fanout 4 and a host cap of 8 (`src/spawn/mod.rs:207-216`, admission at
  `:258`). `flk fleet pause` halts checks, cron, reap, mail wake and agent
  spawns (`src/app/fleet_pause.rs`, `src/app/api/fleet.rs:17`).
- **Not there.** No goal record (G1 #1034 open), no `.flk.toml` reader
  (ADR-0004 "Implemented: the bridge only"), no channel adapter or questions
  (E8 #1037, E9 #1038–#1040 open), no webhook input, no token accounting
  (`src/costs/mod.rs` counts turns and messages, not tokens), no cross-host
  spawn.

### Measurements taken for this ADR

**Event rate.** Sources: `.fleet/supervisor.log`, which holds 5,098 lines over
about 120 h (2026-10-07 23:12 to 2026-10-12 23:20 local) across 13 supervisor
restarts, and `gh` on gerchowl/flock, read-only. "Wake-worthy" means a line the
architect had to act on: a delegate settling, CI finished, a PR opened, a held
reap. Task bookkeeping, reaps and triage reminders are excluded.

| Measure | Value |
| --- | --- |
| Supervisor cycles logged | 2,941; median gap 146 s; > 85 % changed nothing |
| Wake-worthy supervisor events, 120 h | 848: 397 delegate transitions, 268 CI finished, 137 PR opened and other, 46 goal nudges to delegates |
| … per clock hour | mean 7.0, median 5, peak 52; 18 of 121 hours had none |
| … last 24 h | 150 events; mean 6.0, median 3, peak 21; 11 of 25 hours had none |
| `[fleet]` deliveries into the architect's pane | 148 in 120 h, 72 in the last 24 h |
| GitHub, 7 days | 303 issues opened, 153 closed, 586 issue comments, 303 PRs opened, 160 merged, 0 PR review events (verdicts are comments) |
| GitHub, last 24 h | 70 issues opened, 106 comments, 34 PRs opened, 14 merged, 462 workflow runs |
| Authorship | 315 of 315 issues, 295 of 311 PRs and 497 of 586 comments are by `gerchowl`, the operator's account, whether a human or an agent wrote them |

**How far batching cuts wakes.** Count the time windows that hold at least
one event:

| Source | Events | 5 min windows | 15 min windows | 30 min windows |
| --- | --- | --- | --- | --- |
| Supervisor, last 24 h | 150 | 70 | 38 | 24 |
| Supervisor, 120 h | 848 | 434 | 271 | 177 |
| GitHub, last 24 h | 250 | 76 | 43 | 25 |
| GitHub, 7 days | 1,505 | 616 | 374 | 235 |

The two sources overlap heavily, since a merge appears in both. A busy day
of this repo therefore needs about **24–38 wakes** at a 15–30 min window. That
compares with 150 raw events, and with 72 deliveries into a live session today.
More than 40 % of clock hours hold no event at all.

**Lessons learned once, pasted forever.** `.fleet/briefs/_traps.md` (1,572
bytes, eight rules) says "paste verbatim into every brief and reviewer
prompt". Of the 292 brief files in `.fleet/briefs/` on 2026-10-12:

| Rule | Briefs carrying it | Origin |
| --- | --- | --- |
| `cargo nextest` only, never bare `cargo test` | 146 | shared-process state fakes about 14 failures |
| commit with `nix develop --command git commit` | 156 | #642, hooks need the devShell |
| no "placeholder" and no comment line ending in `;` | 142 | guardrail gates scan comment prose |
| unset `FLOCK_SOCKET_PATH` for isolated probes | 6 | added 2026-10-12 after a #1159 reviewer probe reached the live server |
| the size cap binds production code, not a fix round | 3 | #1161 stalled twice on it |
| one extra Enter for a paste chip | 7 | the same workaround lives in `supervisor.py:88-92` |

Each lesson was learned once and is then paid for in every brief: the same
bytes are created again per builder (ADR-0032 Context: rules collapse from 110
distinct strings to 18 when templated). Each is a fact about the repo or a
product defect, not a judgement call. This is the pattern §7b is for.

**Authorship carries no signal.** Every author is the operator's account, so
"something the owner has not worked on" cannot be computed from GitHub's
author field. §2 decides how instead.

## Decision

### 1. Identity and lifecycle: one owner per repo fleet-wide, homed on one node, living in episodes that hibernate when the cache goes cold

**Recommended.**

- **An owner is a binding, not a session.** The operator binds a repo to an
  owner with `flk owner bind <owner/repo> --checkout <abs path> --harness …
  --model … --profile …` on the node that should host it. The server records
  a durable `OwnerBound { repo, node, agent_id, checkout, harness, model,
  profiles[], policy_ref }` event and folds it like ADR-0027's edges, with
  ADR-0031 §4's re-issue on start and on rotation, so a quiet owner cannot
  rotate out. `OwnerUnbound` ends the binding. Binding is an **operator
  verb**, refused for an attested agent caller (the ADR-0014 boundary): an
  owner holds merge and teardown authority over its delegates (ADR-0027 §5),
  so no agent may appoint itself.
- **One per repo, fleet-wide, with a static home node.** The repo key is the
  GitHub slug (`gerchowl/flock`), not a local path. The binding names its node
  statically, the way ADR-0028 §8 names one node per channel binding: there
  is no lease and no failover. Other nodes route `owner:<repo>` mail to the
  home node over mesh custody (ADR-0026, §4). Moving an owner is an operator
  act (`flk owner move`), which re-binds on the new node and reassigns the
  edges (ADR-0027 §1a).
- **The owner keeps one `AgentId` for life.** Its edges, goal, intake
  decisions and cursors all key on it. A new session is a restart of the same
  agent record. Restart already keeps `terminal.agent_id`
  (`src/app/agent_restart.rs:1144`). Restarting into a *new* session instead
  of resuming is new work (slice O2).
- **Episodes.** The owner lives in episodes. An episode is one harness
  session, from a wake until it hibernates:
  1. **Hibernated** (the resting state): no process, no tokens. Events
     accumulate in its buffer (§2).
  2. **A wake starts or continues an episode.** If the last model call was
     within the provider's cache TTL (`[owner] warm_ttl`, default 55 min,
     inside the 1 h TTL every transcript shows, ADR-0032 Context) and the
     context is below the band, flk **continues** the session: the prefix is
     read from cache at 0.1×. Otherwise flk starts a **fresh session** from
     `flk context compile --role owner` (ADR-0032): layer 1 is the owner role
     text, layer 2 the repo (vision, ADR index, traps), layer 3 the goal,
     standing policy and open decisions, layer 4 the snapshot plus **this
     wake's digest**, layer 5 the next action. The first turn therefore holds
     the reason for the wake and needs no read call.
  3. **Working → parked.** When the owner ends a turn waiting (on delegates,
     a CI run, another owner), it registers a wait (ADR-0028 §5) and stays
     alive, parked, at zero token cost.
  4. **Hibernate when warmth is gone.** After `warm_ttl` with no call, the
     owner is asked once for its end-of-episode status (ADR-0030 §6's
     `GoalStatusSet` handshake, bounded wait) and is then hibernated. Keeping
     the process after the cache has expired buys nothing: the next wake would
     pay a cold prefix either way, and a fresh compiled episode is smaller
     than a resumed long transcript.
  5. **Episode rotation replaces compaction.** At the context band (the
     operator's 50–60 % rule), the owner does not compact. It records its
     status, the episode ends, and the next turn starts fresh from the
     compiled context. A compaction summary paraphrases decisions (ADR-0032
     Alternatives). A compiled context carries them as records.
- **Provider limits and profiles.** `phase: limit_reached` (ADR-0028 §1)
  defers the wake: the events keep batching and the owner's state reads
  `blocked{provider_limit}`. If the binding lists more than one profile (the
  skill's §8 ladder, now operator config), the next episode starts on the
  next profile. A fresh episode makes a profile or harness switch ordinary,
  because no transcript has to move. The binding records which profile each
  episode used, which closes the gap #1010 hit, where no record said which
  account a session ran under.
- **Idle cost is zero by construction.** A hibernated owner has no process.
  flk does every poll (§5). Nothing runs on the model until an event the
  owner has not handled passes the wake classes of §2.

**Alternatives rejected.**

- *One owner per repo per host.* Every host would triage the same issue,
  answer the same cross-repo request and race on labels. The mesh already
  routes mail to one node (ADR-0026), and a per-host owner would need a lock
  to undo that.
- *Mesh-wide leader election with failover.* ADR-0028 owner decision 4 chose
  "one statically named node, no failover" for channels on the same grounds:
  a double-acting owner is worse than a quiet one, and moving is rare.
- *One immortal session, resumed and compacted.* This is today's shape. The
  first call after more than an hour of quiet re-creates the whole prefix at
  2×, which is 200k–600k units for a 100k–300k-token transcript. Compaction
  loses whatever the summary paraphrased. The episode model keeps the cheap
  case (warm continue) and drops the expensive one.
- *A fresh session for every wake.* At 24–38 wakes a day, most of them less
  than an hour apart in busy periods, this pays a cold compiled start (about
  57k units, below) each time, where a warm continue costs a fraction of that.
- *Binding from the repo's own `.flk.toml`.* A checkout path, a profile and a
  node are machine facts (ADR-0004 keeps those out of the committed layer).
  Policy is the committed part, and §3 reads it from there.

**Cost.** Owner memory must be fully external: anything that was not
recorded (goal status, intake decision, ADR-0023 decision, note) is gone at
the next fresh episode. That is the quality bar's "nothing undocumented"
applied to the owner itself, and it is the price of zero idle cost.

### 1a. Architects of architects (#1167): one owner per repo, with segment architects as its delegates, never several owners

#1167 asks for two-level coordination inside one repo. A top architect owns
landing, and segment architects own disjoint scopes. The question for this ADR
is how §1's identity, §3's intake and §4's protocol extend to it.

**Recommended: one owner, with segment architects as delegates that are
themselves architects.**

- **Identity.** The owner stays the only binding for the repo (§1). A
  segment architect is a delegate: an ADR-0027 edge from the owner, plus a
  durable `SegmentAssigned { owner, delegate, segment, scope_paths[],
  sub_goal, integration_branch? }` event. The server refuses a scope that
  overlaps another open segment's paths (`scope_overlap`, exit 13), because
  "file ownership decides who changes what" is the coordination mechanism
  #1167's owner comments rely on. The segment architect keeps its `AgentId`
  and lives in episodes like the owner (§1, slice O2). Its edge survives
  episodes, as every edge does (ADR-0027 §1). The depth is owner → segment →
  builder, which is the depth-2 ceiling ADR-0014 §5 already sets for
  agent-initiated spawns (`max_spawn_depth = 2`, `src/spawn/mod.rs:213`). No
  third level exists.
- **Intake stays with the owner.** Every subject still goes through the
  owner's intake (§3). An accepted subject gains one more state,
  `assigned { segment }`, decided by the owner from the paths it touches. A
  subject that spans two scopes is either split by the owner or held by the
  owner itself, never given to both segments. A segment architect cannot
  accept, decline or spike on its own authority. New work it discovers is
  filed through the O4 verbs, comes back to the owner's buffer as
  `actor: delegate:<segment>` (not `self`, so it is not dropped) and is
  triaged like any other subject.
- **Protocol inside the repo is the edge, not §4.** Upward, segment
  architects report through the owner's buffer: their delegates' events are
  their own, and the owner receives the segment's **wave reports**. A wave
  report is a `GoalStatusSet` on the segment's sub-goal (ADR-0030 §6), so it
  is a durable fact and not a chat message. Questions go up to the owner by
  #1160's routing, and only the owner asks the operator. Downward, the owner
  uses `delegate send`. Segment architects do not message each other. A
  change to an interface between segments is an ADR or contract issue that
  the owner decides and lands first, and the segments merge it down. That is
  #1167's "coordinate through artifacts, not chat", with the owner as the
  decider. `owner.request` (§4) stays for repo-to-repo traffic only.
- **The turn contract #1167 names is solved by parking, not by a new wait
  kind.** A segment architect that ends a turn waiting on its builders
  registers a wait (ADR-0028 §5), so it reads `waiting_on_event`, not
  stalled and not "no sentinel". ADR-0030 rung 5 already parks the owner on a
  delegate whose phase is `waiting_on_event`, because that delegate will
  produce an event.
- **Landing stays with the owner.** Segment architects never merge into the
  integration branch. They hand reviewed PRs, or a reviewed segment branch,
  to the owner's landing (`flk land`, #694; gates per §6). Two options remain
  open: per-segment integration branches (`dev-partA`, merged by the segment
  architect on its own authority and handed to the owner once per wave), or
  segment PRs straight into the owner's queue on `dev`. #1167's dry run
  compares them. Either way the owner is the only agent that writes `dev`.
- **Caps count the subtree.** `max_delegates` (§6) counts the owner's whole
  subtree, with segment sub-caps assigned by the owner, as ADR-0014 §5 counts
  live descendants and as #822 asks for nested agents.

**Rejected: several owners per repo with a landing owner.**

- Two bindings per repo breaks §1's invariant that one agent speaks for a
  repo. `owner:<repo>` (§4) would no longer resolve to one recipient.
- No rule says which owner triages an issue that touches two scopes. Two
  intake records for one subject is exactly the "no one decides" failure the
  owner predicted on #1167 for a peer team.
- Peers coordinate by chat, which burns the scarcest context (#1167, owner
  comment: "in the field runs the architect's context always failed first").
- The landing owner would decide merge order, shared files and interface
  contracts. That makes it the top owner in all but name, with a weaker
  identity.
- Separate services that need real independence get separate repos, and §4
  already covers those.

**Cost.** Every segment adds one long-lived architect with its own episodes,
roughly another owner's wake budget, so §8's per-owner figures scale with the
number of segments. The owner becomes the bottleneck for intake and landing
in a large repo. That is deliberate, and #1167's dry run measures whether it
holds. A segment cannot take an outside request directly; the request waits
for the owner's triage.

### 2. Wake vs insight: every input is a durable event; a per-owner buffer classifies it; a hibernated owner wakes with the digest as its context; a busy one gets it at the next boundary

**Recommended.**

- **One input path.** Everything an owner reacts to is a durable event in the
  one log: mail addressed to it, `pr.*`, the new `issue.*` / `release.*`
  kinds (§5), `agent.phase_changed` and `agent.needs_human` for its delegates
  (E3 #1027), `goal.*`, owner requests (§4), `cron.fired` and `watch.fired`
  (§5). No input is typed and none bypasses the log (ADR-0005 P5).
- **The owner is a recipient with a buffer.** The server keeps one
  coalescing buffer per owner. It is ADR-0028 §3's per-recipient buffer and
  #961's digest buffer generalised: the latest state per subject for
  state-like kinds, and every edge-like fact kept as its own line. A filter
  selects what reaches the buffer: the owner's repo, its delegates, its
  registered waits, and mail and requests addressed to it.
- **"Already handled" is a fact the owner records, not a guess.**
  - *Its own echoes are dropped.* Every GitHub write the owner makes goes
    through a flk verb (`flk owner comment|label|file|close`, slice O4) that
    records the created object's node id in a durable `OwnerActed { subject,
    gh_node_id, action }` event. The poller tags an event `actor: self` when
    it matches one, and the buffer drops it. Author cannot do this job
    (Measurements: everything is `gerchowl`). A raw `gh` write made by the
    owner outside flk carries no record and comes back as an outside event.
    The role text therefore forbids raw `gh` writes, and #1160's tool-use
    hook can deny them in the owner's pane.
  - *Handled subjects are filtered.* An intake decision (§3), a review
    verdict on a PR head SHA, or a request answer (§4) marks the subject
    handled up to that fact's `seq`. A later event on the same subject
    re-opens it only when it changes something the decision was based on: a
    new head SHA, a reopen, new text from someone other than the owner, a
    changed label set.
  - *Delivery is at least once.* Each digest records the highest `seq` it
    covers, in a durable `OwnerDigestDelivered { through_seq }` event. A
    digest that was delivered but never acted on (the episode crashed) is
    re-delivered from the same cursor. Consumers dedupe on `seq` (ADR-0028
    §3).
- **Wake classes.** Configurable per repo in `.flk.toml [owner.wake]` (§3
  explains where policy lives). Defaults:

  | Class | Events | Hibernated owner | Busy owner |
  | --- | --- | --- | --- |
  | **now** | operator or channel mail; `needs_reply` / `blocking` mail and owner requests addressed to it (ADR-0018); an answer to its question; its delegate `needs_human` / `blocked` / `limit_reached` (and, through #1160, a delegate's question); `pr.ci_completed` failure on the default branch; a registered wait opening (ADR-0028 §5, so wake follows the wait) | wake at once | next boundary, flagged urgent |
  | **batch** | a new issue or PR not authored by the owner; delegate `finished` / `gone`; `pr.ci_completed` and `pr.merged` on its delegates' PRs; new comments on open subjects; `cron.fired` routed to it | wake when the window closes: `batch_window` 15 min while it has a delegate in flight, `idle_batch_window` 2 h when it has none; or at once when the batch reaches `[digest] max_lines` (12, ADR-0028 §6) | next boundary |
  | **digest** | `fyi` mail; label or milestone changes by others; phases of agents that are not its delegates; releases of other repos it does not watch | never wakes, rides along in the next digest | next boundary |
  | **drop** | `actor: self` echoes; facts on subjects it has closed out | dropped | dropped |

  The two windows are deliberate. While delegates are in flight, a 15 min
  window gives the measured 38 wakes on a busy day and keeps reviews moving.
  With nothing in flight, everything left is intake, which is not urgent, and
  a 2 h window bounds a quiet day to 12 wakes.
- **Delivery, per state.** No path types a body into a composer (#1162,
  ADR-0027 §3).

  | Owner state | Path |
  | --- | --- |
  | hibernated | an episode starts (§1); the digest is part of layer 4 of the compiled context, which is the first user message for a fresh session, or one guarded fixed sentence ("digest ready, call `flock_owner_digest`") for a warm continue |
  | alive, idle, parked | the guarded idle wake (#850 lease, draft stash; ADR-0028 §5) with the fixed sentence |
  | alive, mid-turn | at the turn boundary through the Stop hook's nudge, which already carries mail counts (`src/cli/hook.rs:222-235`). *now* items may also arrive mid-turn as hook output (`additionalContext`) if #1160's hook ADR adopts tool-use hooks for injection; this ADR depends on that decision and does not make it |

  The body is always pulled (`flock_owner_digest`, or `events.read` from the
  owner's cursor). It is never pushed into the composer.
- **The goal ladder stays the stop authority.** An owner is an agent with a
  goal (ADR-0030): its standing goal is the repo's milestone plan and quality
  bar. Its stops go through ADR-0030 §2. Rung 4 or 5 parks it, and the rule
  "hibernate when warmth is gone" in §1 is what happens to a parked owner
  after `warm_ttl`. The owner adds no new rung.

**Alternatives rejected.**

- *Let the owner poll* (`flk events wait` loops, `gh` in the agent). This is
  exactly what the vision forbids, and what ADR-0028 §5 already rules out.
- *Wake on every event.* That is 150 wakes on a measured busy day, against
  24–38 for the same content.
- *Separate batching for each source.* The buffer exists once, from #961 and
  ADR-0028 §3; a second one would be a second event system.
- *Identify echoes by author or by a marker in the text.* Author carries no
  signal (Measurements), and a marker can be copied by anyone and is lost when
  someone edits the text. The recorded node id is exact. A marker stays only
  as a display hint.
- *Deliver the digest as a typed block.* This is the #1162 failure.

**Cost.** Every GitHub write the owner makes must go through flk verbs, which
is a new verb surface (O4). Echo detection does not cover writes the owner
makes with raw `gh` (they come back as outside events, at worst one wasted
wake each). Events reach the owner up to one window late; the *now* class
exists for the cases where that matters.

### 3. Intake ("hold the repo dear"): a state machine with recorded decisions, auto-accept only for decided designs, spike for everything new, and the operator as the only override

**Recommended.**

- **Every incoming subject gets an intake record.** Subjects are an issue, a
  PR from outside the owner's delegates, and an owner request (§4). The
  states:

  ```text
  new ─▶ triaging ─▶ accepted ─▶ planned ─▶ in_progress ─▶ done
             │           ▲                      (edge)      (merged / released)
             ├─▶ spike ──┤
             ├─▶ clarify ─▶ (answer) triaging | (no answer by deadline) deferred
             ├─▶ declined   (reason, terminal unless the operator re-opens)
             ├─▶ duplicate  (of #n, terminal)
             └─▶ deferred   (milestone or "later", re-triaged by schedule)
  ```

  Each transition is a durable `IntakeDecided { subject, from, to, reason,
  refs, by: owner | operator, policy_version }` event, folded into a live
  record and re-issued on rotation (ADR-0031 §4). That record, not GitHub, is
  the source of truth.
- **Rules (the policy).**
  - **Auto-accept** applies only to an operator-authored issue that implements
    a decided design. It must name an Accepted ADR's slice or a decided issue,
    as every slice issue filed today does ("Slice E7 of #1001, specified in
    ADR-0028 §2"). The second case is an operator bug report with a
    reproduction. "Operator-authored" is decided by flk, not by GitHub: the
    issue was filed through an operator verb (`flk issue drop --file-it`,
    ADR-0015) or a channel binding with the operator's scope, or the operator
    confirmed it. A GitHub author of `gerchowl` proves nothing (Measurements).
  - **Spike first** covers new behaviour without an accepted design. It
    includes any change to a public surface (CLI or MCP verbs, the wire
    protocol, persisted event kinds, config keys), any security boundary
    (ADR-0014 class), any request from another owner that widens scope, and
    anything the owner cannot place under an ADR. A spike is a delegate whose
    deliverable is an ADR or a decided issue, never product code. That is the
    rule the fleet already follows ("spike before new features", GOAL and
    STATUS owner decisions).
  - **Clarify**: the owner cannot state the outcome in two sentences or name
    an acceptance test. The question goes to the author as a comment for an
    issue, as a `clarify` response for a request (§4), or to the operator as
    an ADR-0023 decision record.
  - **Decline**, always with a reason: outside the repo's vision (layer 2 of
    the compiled context); contradicts an Accepted ADR without proposing to
    supersede it; a feature request from an external contributor (routed to
    Discussions per `CONTRIBUTING.md` and AGENTS.md "External contributor
    guardrail"); or work that would breach the quality bar (a hotfix, a
    loosened gate).
  - **Duplicate** requires a search first. That is ADR-0033 §6's
    search-first rule: comment on the existing issue, never open a second one.
- **Untrusted text stays data.** An issue or comment body from anyone but the
  operator is quoted in the digest as data, size-capped, with control bytes
  refused, as ADR-0014 §4 decides for spawn prompts. It never becomes a brief
  verbatim: the owner writes every brief itself (ADR-0033 §7 template). An
  external author's issue is never auto-accepted, never triggers a spawn
  directly, and runs no code from its body. `issue_guard` enforces the same
  author gate for its trigger blocks (`src/checks/issue_guard.rs:26-35`).
- **Merges of outside PRs** are never the owner's to make alone. An outside
  PR gets a review and an intake verdict, and the merge is an operator gate
  (§6).
- **Override.** Only the operator overrides: `flk owner decide <subject>
  <state> --reason …` (an operator verb), or an answer through a channel
  binding with the `reply` scope (ADR-0028 §8). An override is an
  `IntakeDecided` with `by: operator`. The owner never re-opens an
  operator decision itself; it may raise an ADR-0023 decision asking for one.
- **Projection to GitHub.** After each decision, flk applies one label
  (`intake:<state>`) and, for `declined`, `clarify`, `duplicate` and `spike`,
  one comment with the reason and a `<!-- flk:intake … -->` marker. These are
  O4 writes, so their echoes are dropped. The labels are a view: if one is
  edited by hand on GitHub, the next fold restores it and reports the drift in
  the digest. It is not an override.
- **Where policy lives.** The policy is a committed `[owner]` section in the
  repo's `.flk.toml` (ADR-0004): wake classes, windows, intake rules, merge
  grant (§6), labels. Per ADR-0004 decisions 4–5, flk reads it from the
  default branch's committed blob, never from a worktree or a PR branch, so a
  PR cannot change the policy that judges it. A policy change is a reviewed
  PR like any other doc. The `policy_version` (blob id) is recorded on every
  decision.
- **Dry run first.** For each repo, intake starts with `mode = "shadow"`:
  decisions are recorded but nothing is posted. After ten shadow decisions
  agree with the operator's own triage, the operator switches the repo to
  `live`. This copies ADR-0033 owner decision 1.

**Alternatives rejected.**

- *GitHub labels as the source of truth.* The shared account cannot attest
  who set a label, two owners or a human can race on it, and every read costs
  rate limit. The log is attested and cheap. Labels are a projection.
- *Auto-accept by label (`agent-ready`, #1074's example).* Anyone with triage
  rights can set a label, and on this account an agent and the human look the
  same. A label is an input to triage, not a decision.
- *Accept everything into a backlog and let planning sort it.* That is what
  the vision calls "taking any issue": it hides the decision and lets
  undecided designs reach dispatch.
- *Hold the policy in host config.* It is a repo fact that should be reviewed
  with the repo and travel with it (ADR-0004).

**Cost.** ADR-0004's `.flk.toml` reader has no code yet, so slice O0 builds
the part this needs. Shadow mode delays the first live triage by at least ten
decisions. Every spike costs a delegate round before any work starts. That
cost is intended.

### 4. Inter-owner protocol: an `owner:<repo>` address, typed request and response blocks over mail, the target owner files in its own tracker, and the requester parks on a watch

**Recommended.**

- **Addressing.** A new message target `{"type": "repo_owner", "repo":
  "<owner/repo>"}` sits beside ADR-0006's `pane` and `repo_pane`, written
  `owner:<owner/repo>` on the CLI. The server resolves it through the binding
  fold to the owner's `AgentId` on its home node. A remote home goes through
  mesh custody (ADR-0026), which keeps its idempotency, TTL and pause
  semantics. If no owner is bound, the send is refused with `no_owner` (exit
  4, ADR-0029) and a hint to file an issue in that repo instead.
- **Messages.** Requests and responses are ordinary mail (ADR-0008) carrying
  a typed block, the same way ADR-0028 §8a adds `options`:

  ```text
  owner.request  { request_id, from_repo, to_repo, kind: change | question | unblock,
                   summary, links[], acceptance?, needed_by?, chain: [request_id…] }
  owner.response { request_id, verdict: accepted { issue } | declined { reason }
                   | clarify { question } | blocked_on { ref } | done { evidence } }
  ```

  A request is `needs_reply` (it wakes, §2). A response is a `msg.reply` with
  `in_reply_to = request_id` (ADR-0023 §2's pattern). Request state is a fold
  of these as durable events, so both owners and the operator can list it
  (`flk owner requests [--repo]`).
- **The target owns its tracker.** The request goes through the target's
  intake (§3) like any other subject. If accepted, the **target owner files
  the issue in its own repo** and answers `accepted { issue }`. The requester
  then registers a watch on that issue at the fix level it needs (§5:
  `closed_completed`, `fix_merged`, `released`) and parks. When the watch
  fires, the requester is woken with the evidence, and the target sends `done
  { evidence }` for the record. If the target repo has **no owner**, the
  requester may file the issue itself (as #1078's example does) and park on
  it the same way. The issue then meets that repo's human or a later owner's
  intake.
- **Trust and authorisation.**
  - The sender is attested, never asserted. The pane ancestry walk (ADR-0014,
    `msg.send`) resolves the sending agent, and the binding fold maps it to
    `owner:<repo>`. Across hosts, the mesh's origin node and `allow_from`
    (ADR-0026) add the node check. Only a bound owner may send
    `owner.request` or `owner.response` blocks. Anything else that carries one
    is refused with `not_an_owner` (exit 13).
  - An owner's authority ends at its own repo. A request asks; it never
    starts an agent, writes, merges or labels in the target. The target's
    answer can only report decisions about the target.
  - Questions to the human: each owner is the root of its own delegation tree
    and may ask the operator about its own repo, by #1160's routing. An owner
    never relays another owner's question to the operator. A question about
    repo B is B's owner's to ask.
- **Loops and storms.**
  - *Chains.* Every request carries the `chain` of requests that caused it.
    The server refuses a request whose chain already contains the target repo
    (`request_cycle`, exit 13), or whose chain is longer than `[owner]
    max_request_depth` (default 3).
  - *Blocked-on cycles.* Watches form a graph between repos. When a new
    `blocked_on` or watch closes a cycle (A waits on B, B on A), the server
    refuses to park the second wait, files an ADR-0023 decision for the
    operator naming the cycle, and wakes both owners with it.
  - *Rate.* At most `[owner] max_open_requests_per_pair` open requests (default
    5) from one owner to another. A duplicate request (same pair, same
    `links`) folds into the open one rather than opening a second.

**Alternatives rejected.**

- *The requester files the issue in the target repo* (the default in #1078's
  example). That writes into another owner's tracker without its intake, and
  it skips the decline and clarify paths. It is kept only as the fallback for
  a repo with no owner.
- *A separate request store or RPC.* Mail already has custody, cross-host
  routing, idempotency and pause. ADR-0005 P5 rules out a second store.
- *Owners messaging each other's pane by name.* Names and panes change across
  episodes and moves. The repo is the stable address.
- *Trust by repo membership or GitHub permissions.* On this fleet every repo
  is the same account, so it attests nothing. Attestation comes from the
  binding.

**Cost.** A new target type (ADR-0006 grows) and two typed blocks. A
cross-repo request takes at least two triage steps, one per owner, before any
work starts. That is the intended "hold the repo dear" cost.

### 5. One trigger layer: every source becomes a durable event, rules route events (to an owner by default), watches are registered waits with more sources, and only flk polls GitHub

This answers #1073, #1074 and #1078 and closes them as spikes. Their
remaining questions are listed in §10.

**Recommended.**

- **Sources produce events. They never act.**
  - **GitHub: one repo poller per home node.** It extends the batched GraphQL
    poller (`src/pr_poll.rs`) from "PRs of local worktree branches" to every
    repo that has a binding or a registered watch on this node. Its one query
    per tick, with aliases per repo and the existing 100-alias chunking,
    covers:
    - issues and PRs updated since the cursor (`updatedAt`), with comments
      and labels;
    - the default branch's check suites;
    - releases.

    It emits durable `issue.opened|closed{reason}|reopened|labelled|commented`,
    `pr.opened`, `branch.ci_completed`, `release.published`, alongside today's
    `pr.*`. The query asks GraphQL's `rateLimit { cost remaining resetAt }` and
    paces itself: below a reserve (`[github] reserve_points`, default 20 % of
    the hour) it stretches its interval and reports `degraded` health, rather
    than holding today's fixed cadence (`src/app/mod.rs:485`) into the limit
    this fleet hit on 2026-10-11 (#1074). Only the home node of a binding polls
    its repo, so mesh nodes never double-poll. `issue_guard` keeps its owner
    trigger semantics and reads from this poller instead of running its own
    `gh issue list` (`src/app/runtime.rs:1617`). The sidebar badges of #1077
    read the same cached state.
  - **Time.** `[[checks.cron]]` stays the one scheduler
    (`src/checks/cron.rs`). It emits `cron.fired`.
  - **Watches.** A watch is ADR-0028 E5's registered wait with more predicate
    sources, not a separate object (#1078's question 1): `issue:<repo>#<n>`
    and `pr:<repo>#<n>` in any repo the poller can read, at a fix level
    `closed_completed` (closed as completed, not as not-planned) <
    `fix_merged` (a PR that closes it merged) < `released` (that merge is in a
    published release) < `deployed` (a named rollout issue is closed), plus
    composites `any_of` / `all_of` with a mandatory deadline. A watch on
    another repo adds that subject to the poller's query for as long as the
    watch is open. When it opens, it emits `watch.fired { wait_id, level,
    evidence }` and wakes its registrant (E6).
  - **Scripts.** `[[checks.script]]` may print JSON lines that become durable
    `source.<name>.*` events (operator-configured programs only, as today).
- **Rules route; they do not run work.** A rule is `when <event matcher> [if
  <predicate over fields>] do <action>`, declared in host config by the
  operator, or in the repo's `.flk.toml [[owner.rules]]` for actions on that
  repo's own owner. The closed action set (`ActionSpec`,
  `src/checks/config.rs:273`) gains:
  - **`RouteToOwner { repo, class }`**: put the event in that owner's buffer
    under a wake class (§2). This is the default for every event about an
    owned repo: a nightly backlog triage is a `cron.fired` routed to the
    owner, not a new architect.
  - **`WakeAgent { target }`** for a non-owner agent: the guarded fixed
    sentence and a digest line, with ADR-0029 outcomes. It never hard-sends;
    #1044's hard-send stays a goal-supervisor decision.
  - **`ForkReview { target, rubric }`**: hands the event to ADR-0033's
    trigger engine (AX6a), which keeps its own rate limits.
  - **`SpawnAgent`**, operator-authored rules only, for repos **without** an
    owner (for example a nightly report job). It goes through `agent.spawn`
    with its ceiling (ADR-0014 §5–§7) and a mandatory deadline, and is
    idempotent per `(rule, subject)` so one issue never starts two agents. In
    an owned repo the owner decides what to start, so `SpawnAgent` is refused
    there.

  Existing `Notify`, `Event` and `WriteDigest` stay. Every firing emits
  `trigger.fired { rule, subject, action, outcome }`. `--dry-run` per rule
  records what it would have done.
- **Is a scheduled or triggered run a goal or a delegate?** (#1073 and #1074,
  owner addition.) For an owned repo it is **neither a new agent nor a bare
  spawn**. It is a wake of the resident owner, whose standing goal (ADR-0030)
  absorbs the event. "Start an agent for this issue" is a **delegate**
  (ADR-0027), started by the owner after intake, with the edge, reap and hold
  rules that come with it. #1078's "resume dependent work" is the owner,
  woken by `watch.fired`, re-dispatching. Automatic re-dispatch would be an
  auto-start, which ADR-0027 §6 deliberately keeps out.
- **Mapping of today's verbs and scripts** (#1073 and #1074 deliverable
  addition): §7.

**Alternatives rejected.**

- *Rules that spawn architects per event* (#1073's `SpawnAgent` architect,
  #1074's "labelled issue starts an architect"). That gives several
  architects per repo with no shared intake, which is exactly what the
  resident owner replaces.
- *Webhooks now.* They need an inbound endpoint reachable from GitHub (a
  public listener or relay) and secret verification. The poller at a 120 s
  base interval covers the latency this workflow needs: one batch window is
  15 min. Kept open (§10).
- *A `RunScript` action fed with event JSON.* Arbitrary hook code is the
  extension-surface question (#503). It would be the first action that runs
  operator code per event. Deferred there, not added here.
- *A watch registry separate from the wait registry.* It would duplicate
  `state.waiting`, the wake path and ADR-0030's parking. #1078 already
  recommended against it.
- *Each agent watching GitHub itself* (`gh pr checks --watch` under
  watcher-s1, the supervisor's 382 watcher starts in 120 h). That is the
  token, process and rate-limit cost the vision names.

**Cost.** The poller's query grows per watched repo. GraphQL cost is counted
per query, so this costs points rather than requests, and the pacing above
bounds it. Process watches (watcher-s1's semantics in the server) are not
decided here (§10). Until they are, an owner waits on a long local command
through the harness's own background mechanism plus `flk events wait`, which
E5 attributes (ADR-0028 §4).

### 6. Safety and cost limits: caps on two levels, a wake budget enforced by flk, provider limits respected, one kill switch, and human gates enforced at the verb and the hook

**Recommended.**

- **Caps.**
  - `[owner] max_delegates` per owner (default 4, today's cap in STATUS.md
    and the skill §0b.4) counts live edges plus their reported subagents. It
    is enforced at `delegate start` for an attested owner caller, with an
    `at_owner_capacity` refusal that carries current, limit and retry
    (ADR-0014 §8).
  - The host wall stays `[fleet] max_concurrent_agents` (8,
    `src/spawn/mod.rs:212`) and now counts owners too.
  - Heavy-build slots are #822's.
- **Budgets.**
  - **Wakes.** `[owner] max_wakes_per_day` (default 48, above the measured
    busy-day 38). flk counts wakes deterministically. Past the budget, only
    *now*-class operator and channel mail wakes the owner, everything else
    keeps batching, and the operator gets a notification (ADR-0016).
  - **Episode length.** `[owner] max_episode_turns` (default 60). Reaching it
    ends the episode with a status handshake and starts a fresh one at the
    next wake. It never loops.
  - **Tokens.** Slice O13 records per-episode usage from the harness
    transcript into `OwnerEpisodeEnded { turns, input, output, cache_read,
    cache_created }`. A token budget per day (`[owner] max_units_per_day`)
    becomes enforceable after that slice, not before. The wake budget is the
    limit until then.
- **Provider limits.** `limit_reached` defers wakes (§1). A profile ladder is
  operator configuration in the binding, never an owner's choice. An owner
  may not log in, change profiles or touch credentials (skill §0b, "never
  improvise credential workarounds").
- **Kill switch.** `flk fleet pause` (exists) also stops owner wakes and
  episode starts. Events keep being recorded and batched, so nothing is lost
  across a pause. `flk owner pause|resume <repo>|--all` pauses owners alone.
  `flk owner stop <repo>` ends the current episode with a status handshake,
  hibernates the owner and disables wakes until `resume`. These are operator
  verbs.
- **Human gates.** From the repo's `.flk.toml [owner.gates]`, defaults
  matching today's GOAL.md:

  | Action | Default |
  | --- | --- |
  | merge a delegate PR into the integration branch | `after_review_and_green` (GOAL.md line 44: reviewer approval and green CI) |
  | merge an outside PR | `ask` |
  | release (`just release`, tags) | `ask` |
  | deploy, host applies, server handoff, stop or restart | `never` (GOAL.md Rollout and Policy; owner decision "no applies") |
  | close an external author's issue as declined | `ask` until shadow mode ends, then `allowed` with a reason |
  | push to `main` or the integration branch directly, `--admin`, `--force` | `never` |

  Enforcement comes at two points, because a prompt alone is not
  enforcement:
  - (a) the flk verbs that perform gated actions (`flk land` from #694, and
    `flk owner release`) check the gate and, for `ask`, raise an ADR-0023
    decision and park on its answer;
  - (b) #1160's tool-use hook surface denies the raw equivalents (`gh pr
    merge`, `gh release`, `just release`, `flk server …`) in an owner's or
    delegate's pane with a message naming the flk verb.

  GitHub's own ruleset, where only the release App moves `main` (AGENTS.md),
  is the outer wall.

**Alternatives rejected.**

- *A budget in tokens only.* No token data exists in flk yet
  (`src/costs/mod.rs`). A budget nobody can measure is not a limit. The wake
  budget is enforceable now.
- *Gates in the prompt.* The role text states them, but the field has shown
  agents working around rules under pressure (skill §0b's credential
  example). The verb and hook layers make them hold.
- *No per-owner cap, host cap only.* One busy owner would starve every other
  repo on the host. That is ADR-0014 §5's argument against a flat counter.

**Cost.** The hook enforcement depends on #1160's slices, and on the
harnesses #1160 covers. A harness without a deny hook falls back to the
verb gate plus GitHub's ruleset.

### 7. Migration: `.fleet/` and the architect skill retire piece by piece into the owner, behind the checkpoints of ADR-0030 §8, after a shadow run

**Recommended.** Every responsibility maps to a decided home. The flock repo
is the first owned repo, in shadow mode, before anything is switched off.

| Today | Becomes | Slice | Retires when |
| --- | --- | --- | --- |
| `supervisor.py` status loop, `[fleet]` lines | owner buffer and digest (§2) on E3 events | O5, #961, E3 #1027, E6 #1032 | O5 live |
| `supervisor.py` CI watcher (watcher-s1 around `gh pr checks --watch`) | `pr.ci_completed` (shipped, #1020) into the owner's buffer | O5 | O5 live |
| `supervisor.py` reap on merge, issue hold | ADR-0027 reap (#968), hold = edge stays open until the issue's intake record is `done` | #968, O6 | #968 live |
| `supervisor.py` `goal_check` per delegate | delegate goals (ADR-0030 §5, G1/G2) | #1034, #1035 | G2 live |
| `supervisor.py` ctx scrape and three-phase `/compact` | episode rotation at the band (§1), status handshake (ADR-0030 §6) | O2, G5 #884 | O2 live |
| `supervisor.py` 30 min untriaged-issue reminder | intake records (§3) from `issue.opened` | O3, O6 | O6 live (shadow) |
| `landbatches.sh`, `batch.sh` (`sleep 60` over `gh pr checks`) | `flk land` (#694) behind the merge gate (§6), driven by `pr.*` events | #694, O11 | #694 live |
| `GOAL.md` | goal record (ADR-0030) plus `.flk.toml [owner]` policy and gates | G1, O0, O6 | O6 live |
| `STATUS.md`, `HANDOFF.md`, `CONTINUE.md` | compiled layers 3–5 (ADR-0032 §1), episode status | X3, X4, X6, O2 | X10 (ADR-0032) |
| `tasks.json`, `triage.json` | intake records plus goal subtasks | O6, G1 | O6 live |
| `agents.json` | `delegate.list` (shipped, #960) | — | already |
| `spawn-claude.sh` dialog scraping | `delegate start` (exists) and #1116 | — | per ADR-0030 §8 |
| skill §0 preflight and arm | `flk owner bind` (O1) and the owner role text | O1, O12 | O12 |
| skill §3 "observe through events" and background waits | wake classes; the owner never waits in a loop | O5 | O5 live |
| skill §6 friction filing | ADR-0033 AX reviews and filing policy, plus lift proposals (§7b) | AX1–AX7, O16, O17 | per ADR-0033 |
| `.fleet/briefs/_traps.md` pasted into every brief | the repo traps file compiled into layer 2 (ADR-0032 owner decision 3), kept current by lift proposals (§7b) | ADR-0032 X1, O17 | X1 live and the rules merged into the traps file |
| STATUS.md "Next action" and hand-written progress reports to the operator | the daily standup (§7a) | O15 | O15 live |
| skill §7 HANDOFF and compaction | episode rotation | O2 | O2 live |
| skill §8 harness takeover | profile ladder in the binding, fresh episodes | O2 | O2 live |

- **The skill becomes the owner role text** (layer 1). Per ADR-0032 owner
  decision 1 it stays in the skills repo, read by path and pinned by hash. It
  loses everything flk now does (preflight scripts, supervisor, HANDOFF,
  compaction mechanics) and keeps judgement: waves, briefs, review, the
  quality bar, intake rules and the protocol of §4.
- **Shadow run.** O12 binds `gerchowl/flock` with intake in `shadow` mode
  while the hand-built supervisor keeps running. For at least seven days the
  owner's recorded decisions and digests are compared against what the
  architect actually did: agreement on triage, wakes per day, missed events.
  The supervisor stops only after the operator reviews that comparison. Then
  the pieces retire in the order of the table.
- **Rollout.** As always: release, then a g-fleet "Roll out flock vX" issue.
  No owner applies anything to a host.

**Alternatives rejected.**

- *A big-bang switch.* The supervisor is the only thing that runs the fleet
  today. Removing it before the owner has shown equal recall in shadow would
  stall the repo it builds.
- *Keep `.fleet/` as the owner's state store.* ADR-0027, ADR-0030 and ADR-0032
  each already rejected hand-kept files as the source of truth.

### 7a. The daily standup is a fold flk compiles, not a turn the owner writes

**Recommended.**

- **What it says** (the owner's six headings), and where each comes from.
  Nothing re-reads a transcript:

  | Heading | Source |
  | --- | --- |
  | achieved | `pr.merged`, `issue.closed{completed}`, `release.published` in the window (O3, #1020); reaped edges (`DelegateReaped`, ADR-0027 §5); intake records that reached `done` (§3) |
  | planned | the goal record's open subtasks and wave structure (ADR-0030 §1); intake records in `accepted`, `planned` and `assigned` (§3, §1a); queued start intents (ADR-0027 §6) |
  | pending | PRs awaiting review or landing (`delegate.list` joined with the poller's PR state, ADR-0027 §2); open ADR-0023 decisions and gate requests for the operator (§6); open `owner.request`s both ways (§4); open watches (§5) |
  | blockers | delegates or owner in `needs_human`, `blocked` or `limit_reached` (ADR-0028 §1); red default branch (`branch.ci_completed`); expired wait deadlines; blocked-on cycles (§4); wake budget exhausted (§6) |
  | experiences | AX review records in the window, with their metrics and the issues they filed or commented on (`AxReviewRecorded`, ADR-0033 §5); friction issues filed by the owner or its delegates (O4 `OwnerActed` with action `file`) |
  | wishes | open lift proposals awaiting approval (§7b), and the top Q8 answer ("which single change would have saved the most turns", ADR-0033 §2) of the last AX review |

- **Compiled by flk, with no model in the loop.** `flk owner standup <repo>
  [--since <time>]` folds the sources above into a bounded markdown and JSON
  report: twelve lines per heading, overflow as `+k more: <command>`
  (ADR-0028 §6). This is the same pattern as `WriteDigest`, which already
  folds the log into a digest "with no LLM in the loop"
  (`src/checks/config.rs:284-293`). The prose parts (experiences, wishes) are
  quoted from records that a model already produced for its own reason (an
  AX review, a lift proposal), never written for the standup. The owner is
  not woken to produce it.
- **Scheduled through §5.** A `[[checks.cron]]` entry with a new closed
  action `Standup { repo, deliver: [notification, channel:<binding>, file] }`
  (slice O15). The default is weekdays 08:00 local, set in the repo's
  `.flk.toml [owner.standup]`.
- **Delivered, never typed.** The standup is filed as an operator
  notification (ADR-0016), posted to a channel binding that subscribes to the
  `owner.standup` kind (ADR-0028 §8, E8 #1037), and written as a file like
  `WriteDigest`. It enters the owner's own buffer as a *digest*-class line
  (§2), so the owner sees it at its next wake. It is never typed into any
  composer (#1162).
- **Answering it.** A reply on the channel post is inbound mail to the owner
  (ADR-0028 §8, `needs_reply`, *now* class), so "do X first today" from a
  phone reaches the owner as an operator message.

**Alternatives rejected.**

- *The owner writes the standup each morning.* That is one cold episode a
  day (about 57k units, §8) to restate facts flk already holds, with the
  risk that the prose drifts from the records. ADR-0032 §1 settles this: a
  snapshot is derived, not authored.
- *Build it from transcripts.* The owner excluded this explicitly. Records
  are smaller, exact and auditable.
- *Type it into the architect's pane each morning.* That is the #1162
  failure.

**Cost.** Zero model tokens. A standup is only as good as its records: a
delegate's work that never produced an event (an unattested start, ADR-0027
§8) is missing from it. The standup lists such gaps (`unattributed: n`)
rather than hiding them.

### 7b. Transcript-driven improvement: a local miner finds expensive patterns, ADR-0033's review engine turns them into lift proposals, and a human approves every lift

**Recommended.**

- **Two stages, so that a model only sees what a counter already found.**
  1. **Mine, with no model.** `flk ax mine --subject <owner> [--include
     delegates] --window <dur>` reads the local transcripts of the owner and
     of every agent on its edges in the window (Claude Code, Codex, opencode
     session logs). It is a port of the toolchain survey's read-only,
     aggregate-and-redact method (`scripts/research/toolchain_survey.py:1-24`:
     "Raw transcript text never leaves the host: only counters, short
     redacted examples"). It emits pattern fingerprints with counts and cost:
     - repeated multi-step tool sequences: n-grams of normalised commands
       (paths, numbers and ids templated) seen in at least N sessions;
     - large reads: tool results over a byte bound, by source;
     - polling loops: repeated identical reads or `sleep`/`until` shapes,
       which is ADR-0033 Q2's question, counted rather than recalled;
     - retries: the same command template run again after a non-zero exit;
     - re-derived facts: the same read or command template whose output hash
       repeats across sessions (the same fact computed twice);
     - repeated brief text: identical paragraphs across briefs, the
       `_traps.md` case in Context.

     Each fingerprint carries its count, the sessions it spans and the
     tokens attributed to it, from the harness's own `usage` fields where it
     records them (ADR-0032's `cache_usage.py` method). The output is one
     JSON file under `state_dir()/ax/`, served as an MCP resource (ADR-0017),
     like ADR-0033 §5's answers.
  2. **Propose, through ADR-0033.** When the miner finds a fingerprint above
     a threshold (default: seen in at least 3 sessions and 5 times, or more
     than 50k tokens attributed in the window), it fires an ADR-0033 trigger
     of a new kind, `lift`, with a versioned rubric `docs/ax/rubric-lifts.md`
     (`ax-lifts/1`). It runs through ADR-0033's trigger engine, rate limits,
     `AxReviewRecorded` event and filing policy (AX6a, AX4a, AX7), not
     through a parallel mechanism. The reviewer's state is the mined table,
     not one agent's warm context, so it uses ADR-0033's compiled-reviewer
     path (AX9) rather than a fork: the patterns span many sessions, and no
     single warm context holds them.
- **What a lift proposal is.** One candidate per fingerprint, with a target
  and the evidence (count, sessions, tokens, the command template, never
  output text):

  | Target | Example from this fleet | How it lands |
  | --- | --- | --- |
  | a `just` recipe or script in the repo | a multi-step check sequence repeated by builders | an issue in the repo, through the owner's intake (§3); if approved, a delegate builds it |
  | a line in the repo's `AGENTS.md` / `CLAUDE.md`, or the repo traps file of ADR-0032 owner decision 3 (`docs/agents/traps.md`, compiled into layer 2) | the six pasted rules in Context: nextest, `nix develop` commits, the comment-prose gates, `FLOCK_SOCKET_PATH`, the size cap | as above; once merged, layer 2 carries them and briefs stop repeating them (ADR-0032 §5) |
  | a product fix | the paste-chip Enter (`supervisor.py:88-92`) is a symptom of #1162 / #950, not a rule to teach | a comment with the new evidence on the existing issue (ADR-0033 §6 search-first) |
  | a rule or skill in the general Claude config (claude-config) | a harness-level habit seen across repos | an issue in `gerchowl/claude-config`, never a change to it |
  | a flk feature | a state agents rebuild by hand | an issue in `gerchowl/flock`, through that repo's owner if it has one (§4) |

- **Humans approve, and the owner changes only its own repo.** Every lift is
  a proposal. In phase 1 it is recorded and nothing is filed (ADR-0033 owner
  decision 1). Afterwards it is filed under ADR-0033 §6's caps and
  search-first rule, as an issue labelled `lift` or as an ADR-0023 decision
  for the operator. An approved lift for the owner's own repo enters intake
  as an operator decision (§3) and is built by a delegate. For another repo,
  the owner sends an `owner.request` to that repo's owner (§4), or files an
  issue where no owner exists. It never edits another repo, the general
  Claude config or a host's config itself. Host config stays the operator's
  (GOAL.md Rollout: no applies).
- **Privacy.**
  - Transcripts are read on the host that holds them and never leave it.
  - The miner's output contains counts, hashes and templated command shapes
    only. Templating removes paths under `$HOME`, hostnames and ids, with
    the same redaction the survey uses. A filed issue passes the repo's
    fixture-host gates like any other text (AGENTS.md, "Fixture hosts must
    be declared fictions").
  - Output text from tools is never quoted, only its size and hash.
  - The miner reads transcripts of agents on this owner's edges only, not
    every session on the host. Delegates running under another Claude
    profile are read only if the operator listed that profile in the
    binding (§1).

**Alternatives rejected.**

- *A model reads the transcripts directly.* That costs a model call over
  hundreds of thousands of tokens per pass, it recalls instead of counting
  (ADR-0033's prototype found four of five candidates were duplicates when
  recalled), and it puts raw transcript text into another context.
- *A separate improvement engine beside ADR-0033.* The owner asked for
  reuse. ADR-0033 already has the rubric versioning, records, report fold,
  rate limits and filing policy that this needs.
- *Auto-apply lifts in the owner's own repo.* A lift changes what every
  later agent reads (layer 2) or runs (a recipe). That is a change like any
  other, so it goes through intake and review.
- *Edit claude-config or other repos directly.* The owner excluded it, and
  it would break §4's rule that an owner's authority ends at its own repo.

**Cost.** The miner is new code that must keep up with three transcript
formats. The survey script already parses all three
(`scripts/research/toolchain_survey.py`, `claude`, `codex`, `opencode`
readers), so the port starts from working parsers. Each lift review is one
compiled-reviewer run (§8). A lift that is proposed and declined costs a
review and an issue comment; the 0.8 confidence bar and caps from ADR-0033 §6
bound that.

### 8. Token cost, explicitly

Units are input-token equivalents at the provider ratios ADR-0032 used: cache
read × 0.1, cache creation × 2, uncached input × 1. Dollar figures use
ADR-0032's Opus spot check, $0.245 for 56.5k units, so about $4.3 per million
units. These are estimates built from measured parts. O13 measures the real
ones.

**Per wake (overhead to the first action, excluding the work itself):**

| Case | Units | Basis |
| --- | --- | --- |
| Hibernated, fresh compiled episode (cold) | ≈ 57k | ADR-0032: compiled architect resume, cold, 58.1k Haiku / 56.5k Opus, one turn |
| Hibernated, fresh episode with layer 1 warm (another owner or episode used it within the TTL) | ≈ 42k | ADR-0032: warm 42.3k / 42.2k |
| Alive, warm continue, prefix P tokens | ≈ 0.1·P + 2·(digest tokens) | prefix read from cache. For P = 60k and a 1.5k-token digest: about 9k |
| Today: one `[fleet]` delivery into a 100k–300k-token live context | ≈ 10k–30k for the first call, then 4.5 calls median before the first action at a 55k–64k prefix after a resume | ADR-0032 Context |

**Per day:**

| Scenario | Wakes | Estimate |
| --- | --- | --- |
| Idle (no events) | 0 | **0 tokens**: no process, flk polls |
| Quiet (intake only, under 20 events, 2 h window) | 2–12 | 2–12 cold episodes ≈ 0.1–0.7M units (≈ $0.5–3) |
| Busy, as measured (15 min window while delegates are in flight) | ≈ 38 | if two thirds land inside the warm TTL: 25 × 9k + 13 × 57k ≈ 1.0M units (≈ $4.4) |
| Today's shape on the same day | 72 deliveries into a live 100k–300k-token context | 72 × 10k–30k ≈ 0.7–2.2M units for first calls alone (≈ $3–9), plus self-compactions and resumes, and the session never idles to zero |

**Additions from §1a, §7a and §7b:**

| Item | Model tokens | Basis |
| --- | --- | --- |
| Daily standup (§7a) | **0** | a fold over records, as `WriteDigest` is; one cron fire a day |
| Transcript miner (§7b, stage 1) | **0** | local CPU and disk only. The survey script walked 21 days on four hosts this way (ADR-0028 Context, #987) |
| Lift review (§7b, stage 2) | ≈ 58k–77k units per review, at most `[ax] lift_reviews_per_day` (default 1) per owner, so ≤ ≈ 77k units a day (≈ $0.33) | ADR-0033 §1: a compiled reviewer measured at 58k–77k units, independent of the subject's context size |
| Each segment architect (§1a) | up to one more owner's figure on a busy day | its wakes are a partition of the owner's events plus its own builders. Bounded by its own wake budget (§6) |

A lift pays for itself when it removes more than its review cost. The
worked example in Context: the six pasted rules are created anew in every
builder's first message. Moving them into the cached layer 2 (ADR-0032 §3)
turns that creation into a cache read. At about 1.5 KB (the size of
`_traps.md`, roughly 400 tokens) created at 2 units a token (800 units) against
read at 0.1 (40 units), that saves about 760 units per builder, so one
≈ 77k-unit review pays back after about 100 builder starts. That is weeks at this fleet's measured rate of 303
PRs a week. The larger saving is the rounds the rules prevent, such as
#1161's two stalls, which this ADR does not try to price.

What these figures support:

- The idle claim is structural, not estimated: no process exists.
- The busy-day saving is modest and uncertain. It depends on the warm share
  and on how much context a working episode builds up. The robust gains are
  the zero cost of idle and quiet hours (more than 40 % of clock hours had no
  event), the removal of rehydration turns (ADR-0032: 1 turn against 5–14),
  and the deletion of the per-PR watchers and 90 s polling (2,941 cycles, of
  which more than 85 % changed nothing).
- GitHub cost falls too. One GraphQL query per 120 s per home node (720 a
  day, a few points each) replaces per-PR `gh pr view` and `gh pr checks
  --watch --interval 30` processes (382 watcher starts in 120 h) and
  `landbatches.sh`'s 60 s loop.

### 9. Slices

Sizes are estimated production lines (docs and tests excluded), each at most
about 400 for the first PR. "Skew" follows ADR-0028 §3a: new methods are
gated by #868 (exit 78 against an older server). New persisted kinds rely on
#1058's reader skip, which shipped in v2.0.0. Mesh `VERSION` stays 5: owner
mail rides the existing mail frames with additive fields, and a peer without
the fields treats the typed block as plain text.

| # | Slice | Size | Depends on | Replaces / extends | Skew | Acceptance |
| --- | --- | --- | --- | --- | --- | --- |
| O0 | Minimal `.flk.toml` reader per ADR-0004 decisions 4–5: default-branch committed blob from the common dir, `[owner]` section only, `policy_version` = blob id | ~250 | — | ADR-0004 (first code) | client-free, server-internal | A policy edited in a worktree or on a PR branch is not read; a merged change moves `policy_version`; a missing file yields defaults and a `missing` note |
| O1 | Owner binding: `flk owner bind|unbind|list|show` (operator verbs; agent callers refused), `OwnerBound` / `OwnerUnbound` kinds, fold, re-issue on start and rotation, MCP `flock_owner_show` (read) | ~300 | — | new; extends ADR-0027 §1a (edges under a stable owner) | new methods, gated; new kinds | Bind, restart the server, force rotations past `MAX_ROTATED`: the binding survives; an attested agent calling `bind` gets exit 13 |
| O2 | Episodes: wake = warm continue or fresh compiled session on the same `AgentId`; hibernate after `warm_ttl` with the status handshake; rotation at the band instead of compaction; profile ladder; `limit_reached` deferral; per-episode profile recorded | ~380 | O1, ADR-0032 X1/X3/X4 (#1079 …), G1 #1034, #1070 | ADR-0027 §1a takeover (made ordinary); #1010 profile record | server-internal plus a restart variant | Fake harness: two wakes 10 min apart continue one session; a wake after `warm_ttl` starts a fresh session whose first message holds the digest; `AgentId` and edges unchanged across episodes |
| O3 | Repo poller: the PR poller's query extended to issues, comments, labels, default-branch checks and releases for bound and watched repos; `rateLimit` pacing with a reserve; durable `issue.*`, `pr.opened`, `branch.ci_completed`, `release.published`; home-node only; `issue_guard` fed from it | ~400 | O1 | #1020 (extends), #1074 sources, #1077 data source; issue_guard's own `gh issue list` | new kinds | Fake GraphQL: one request per tick for N repos; a new issue emits one `issue.opened`, a restart re-emits nothing; under the reserve the interval stretches and health reads `degraded` |
| O4 | Owner GitHub writes and echo suppression: `flk owner comment|label|file|close` record `OwnerActed { gh_node_id }`; the poller tags `actor: self` | ~250 | O3 | — | new methods, gated | An owner's comment comes back from the poller as `actor: self` and is dropped from the buffer; the same text posted by raw `gh` is not |
| O5 | Owner buffer and wake classes: per-owner filter over the generalised #961 buffer, class table from `.flk.toml`, two windows, `max_lines` flush, `OwnerDigestDelivered` cursor, `flock_owner_digest`; hibernated owner woken through O2 | ~400 | O0, O2, O3, #961, E3 #1027, E6 #1032 | #961 (generalises) | new kinds and verb | A recorded 24 h event fixture yields wakes within ±10 % of the windowed count in §8; a *now* event wakes a hibernated owner within one injection; a crash before acting re-delivers the same `through_seq` |
| O6 | Intake state machine: `IntakeDecided` kinds, fold, auto-accept and spike rules from policy, `flk owner intake list|decide` (operator override), label and comment projection through O4, shadow mode | ~400 | O0, O4, O5 | `.fleet` triage reminder, `triage.json`; #1074's "labelled issue starts an agent" (replaced) | new kinds and verbs | Fixture issues: a slice issue of an Accepted ADR filed by an operator verb is auto-accepted; an external feature request is declined with the Discussions reason; shadow mode posts nothing; a label edited by hand is restored and reported |
| O7 | `owner:<repo>` target: ADR-0006 extension, resolution through O1, mesh routing to the home node, `no_owner` refusal | ~250 | O1 | ADR-0006 (extends) | additive target; old server: exit 78 | A send to `owner:x/y` on a non-home node arrives at the home node's owner; an unbound repo exits 4 with the hint |
| O8 | Owner requests: `owner.request` / `owner.response` blocks, request fold, `chain` depth and cycle refusals, per-pair cap, `flk owner request|respond|requests` | ~400 | O7, O6 | new | additive message fields; mesh VERSION unchanged | A→B accepted yields an issue filed by B; a request whose chain contains the target exits 13 `request_cycle`; a sixth open request to one pair is refused |
| O9 | Watch sources on the wait registry: `issue:` / `pr:` in any repo at fix levels, composites, deadline; the poller follows watched subjects; `watch.fired`; blocked-on cycle detection raising an ADR-0023 decision | ~400 | E5 #1031, E6 #1032, O3 | #1078 (core) | new predicate kinds on `wait.park`, gated | A parks on `issue:x/y#3 released`; closing as not-planned does not fire it; a release containing the fix does, waking A with the evidence; A↔B blocked-on is refused with a decision filed |
| O10 | Trigger rules: `when/if/do` over events, cron as a source, actions `RouteToOwner`, `WakeAgent`, `ForkReview`, `SpawnAgent` (unowned repos, operator rules only, idempotent per `(rule, subject)`), `trigger.fired`, `--dry-run` | ~400 | O5, ADR-0014 spawn | #1073 (actions), #1074 (rules), `ActionSpec` (extends) | config and new kinds | A nightly cron routed to an owner wakes it once with a `cron.fired` line; `SpawnAgent` in an owned repo is refused; the same subject never spawns twice |
| O11 | Safety: `max_delegates` at `delegate start` for an owner caller, `max_wakes_per_day`, `max_episode_turns`, `flk owner pause|resume|stop`, fleet pause covering owners, gate checks in `flk land` (#694) and `flk owner release` raising ADR-0023 decisions | ~350 | O1, O5, #694, #1160 slices (hook deny) | #822 (per-owner part) | new verbs, gated | A fifth `delegate start` exits with `at_owner_capacity`; past the wake budget only operator mail wakes; `fleet pause` stops a wake and keeps the events; a merge under `ask` parks on a decision |
| O12 | Shadow run on `gerchowl/flock` and migration: owner role text (from the architect skill), the `.fleet` mapping of §7 in docs, `flk owner shadow-report` comparing owner decisions with the operator's triage | ~150 | O1–O6 | `.fleet/` (retires per §7), architect skill | — | Seven days of shadow decisions and wake counts are reported, and the operator's go is recorded before the supervisor stops |
| O13 | Episode usage: per-episode tokens from the harness transcript into `OwnerEpisodeEnded`; `[owner] max_units_per_day` enforced | ~300 | O2 | `src/costs` (adds tokens) | new kind | A fake transcript with known usage yields the exact units; past the budget behaves as the wake budget does |
| O14 | Segment architects (§1a): `SegmentAssigned` kind and fold, disjoint `scope_paths` (`scope_overlap` refusal), the `assigned { segment }` intake state with path-based routing, depth limit owner → segment → builder at `delegate start`, subtree cap with segment sub-caps, segment wave reports as `GoalStatusSet` lines in the owner's buffer | ~350 | O5, O6, O11, G1 #1034, #1160 routing; **after #1167's dry run** decides integration branches vs direct queue | #1167 (flk primitives part) | new kinds and verb, gated | Two segments with overlapping paths: the second is refused; an issue touching segment A's paths is routed to A; a segment architect parked on its builders reads `waiting_on_event` and parks the owner; a third-level `delegate start` is refused |
| O15 | Daily standup (§7a): `flk owner standup [--since]` fold over records, bounded markdown and JSON, `Standup` cron action delivering to the notification log, a channel binding and a file; `owner.standup` kind; `unattributed: n` gap line | ~350 | O3, O5, O6; ADR-0033 AX4b for experiences; E8 #1037 for channel delivery | `WriteDigest` (extends); STATUS.md progress reports | new action and kind | A fixture day of records yields all six headings with the right items and no model call; nothing is typed into any pane; a channel reply reaches the owner as `needs_reply` mail |
| O16 | Transcript miner (§7b stage 1): `flk ax mine` ported from `scripts/research/toolchain_survey.py` (Claude, Codex, opencode readers), edge-scoped, six pattern kinds, templating and redaction, token attribution from `usage`, JSON under `state_dir()/ax/` as an MCP resource | ~400 | ADR-0027 edges (shipped), AX4a | the survey script (productised); `_traps.md` detection | client-side read; new MCP resource | A fixture transcript set yields the expected fingerprints and counts; no output text and no `$HOME` path or hostname appears in the JSON; sessions outside the owner's edges are not read |
| O17 | Lift reviews (§7b stage 2): trigger kind `lift` with thresholds, `docs/ax/rubric-lifts.md` (`ax-lifts/1`), compiled-reviewer path, target routing (own repo → intake, owned repo → `owner.request`, otherwise an issue), dry run first, `[ax] lift_reviews_per_day` | ~300 | O16, AX6a, AX7, AX9, O6, O8 | — | config, existing AX kinds | Mined fixture over the threshold fires one review, which records candidates and files nothing in dry run; a lift for claude-config becomes an issue candidate there and never a change; a second review the same day is `suppressed` |

**Order.** O0, O1 → O3 → O4 → (E3, E5, E6, #961 from ADR-0028 and ADR-0027) →
O2 (after G1 and ADR-0032 X1/X3/X4) → O5 → O6 → O12 (shadow starts) → O7 →
O8 → O9 → O10 → O11 (after #694 and #1160's hook slices) → O13. O15
(standup) follows O6, because it needs intake and goal records to say
anything. O16 (miner) has no dependency on the owner and can start at any
time. O17 follows O16 and ADR-0033's AX6a, AX7 and AX9. O14 (segments) waits
for #1167's dry run and for O11. O12 can start
as soon as O6 runs in shadow mode. Shadow needs neither cross-repo traffic
(O7–O9) nor live gates (O11).

**What the three spikes become.** #1073: O10 plus `cron.fired` routing. Its
"singleton architect per repo" question is answered by the binding (O1).
#1074: O3, O4, O6 and O10. Its `RunScript` question goes to #503. #1078: O9
and O8. Its process-watch question stays open (§10). #1167: its identity,
intake and protocol questions are answered by §1a and become O14. Its dry run
(hierarchy against artifact-coordinated peers, integration branches against
a direct queue) stays #1167's and gates O14.

## Consequences

- A repo has exactly one agent that speaks for it, with an attested identity
  that survives sessions, profiles and harnesses.
- An owner with nothing to do costs nothing. A busy owner is woken in batches
  sized by measurement, with the reason for the wake in its first turn.
- No agent polls GitHub or flk. One poller per home node feeds the owners,
  the sidebar (#1077) and the watches.
- No request becomes work without a recorded intake decision, and every
  decision can be listed, overridden by the operator and audited.
- Cross-repo work is mail between owners with a typed protocol, loop
  refusals and parked watches, not a human relaying messages.
- The hand-built `.fleet/` layer and most of the architect skill's mechanics
  retire. The skill keeps the judgement.
- New durable kinds: `OwnerBound`, `OwnerUnbound`, `OwnerActed`,
  `OwnerDigestDelivered`, `IntakeDecided`, `OwnerEpisodeEnded`, `issue.*`,
  `pr.opened`, `branch.ci_completed`, `release.published`, `watch.fired`,
  `trigger.fired`, `SegmentAssigned`, `owner.standup`, and the owner request
  fold events. All are additive.

- The operator gets one standup a day from flk, at zero model cost, instead of
  reconstructing progress from STATUS.md.
- Lessons learned once can become repo facts that every later agent reads
  from cache, instead of paragraphs pasted into every brief. A human approves
  each one.
- A large repo can run segment architects under one owner, without a second
  identity, intake or landing authority.

## Risks

- **Echo leaks.** A write the owner makes outside the O4 verbs comes back as
  an outside event. Bounded to one wasted wake per write. #1160's hook can
  deny raw `gh` writes in owner panes.
- **Fresh-episode amnesia.** Anything not recorded is lost at rotation. The
  status handshake is mandatory before every hibernate and rotation, and the
  shadow run measures what was missed.
- **Poller latency.** 120 s plus a 15 min window means an outside issue is
  seen up to about 17 min late, and up to 2 h late when nothing is in flight.
  The *now* class covers what must not wait. Webhooks are the later answer if
  that proves too slow (§10).
- **Over-strict intake.** Spike-first slows small changes. The auto-accept
  rule covers the common case of slice issues, and shadow mode calibrates the
  rest before anything is posted.
- **Harness dependence.** Warm continue, fresh-session restarts and
  system-prompt placement are harness behaviour (ADR-0032 Risks). O2's probe
  re-measures them per harness version.

- **Miner false positives.** A frequent sequence is not always waste (`just
  check` before every commit is meant to repeat). The 0.8 bar, dry-run phase
  and human approval of every lift bound this. Declined lifts are recorded so
  the same fingerprint is not proposed again within `[ax] lift_cooldown`
  (default 30 days).
- **Transcript format drift.** Harness transcript formats are not contracts.
  O16 carries fixture transcripts per harness version, and an unknown shape
  is counted as `unparsed`, never guessed.
- **Owner bottleneck with segments.** One owner triages and lands for every
  segment. #1167's dry run measures it before O14 is filed.

## Test strategy

- **Fold tests** for every new kind across random rotations and restarts
  (ADR-0031's method), including a parked owner that emits nothing for longer
  than the retention.
- **A replay fixture of 24 h** built from `.fleet/supervisor.log` and the
  `gh` event timestamps used in §8, fed through O3's fake GraphQL and O5's
  buffer. It asserts the wake count per window and that no wake-worthy event
  is dropped.
- **Intake table tests**, one case per rule in §3, first match wins, plus
  the shadow-mode no-write check against a fake `gh`.
- **Two-node mesh test** for O7 and O8: request, accept, issue filed by the
  target, watch fires, `done`. Plus the cycle refusal.
- **Isolated flk probe** (`tests/probes/`, sandboxed `XDG_CONFIG_HOME`,
  `FLOCK_*` unset) with a throwaway Claude owner: hibernate, wake by a fake
  `issue.opened`, and assert the first turn holds the digest and that no
  `.fleet` file or pane read happens.

## 10. Open questions for the owner

1. **Identity (§1).** One owner per repo fleet-wide, on a statically named
   home node with no failover (recommended). Or one per host.
2. **Episodes (§1).** Continue while the cache is warm, otherwise start a
   fresh compiled session, and rotate instead of compacting (recommended).
   Or keep one long session that compacts.
3. **Windows (§2).** 15 min while delegates are in flight and 2 h when none
   are (recommended, from the measured 38 versus 150 wakes a day). Or one
   fixed window.
4. **Auto-accept scope (§3).** Only operator-filed slice issues of Accepted
   ADRs and operator bug reports with a reproduction (recommended). Or wider,
   for example every operator-filed issue.
5. **Merge grant (§6).** The owner may merge delegate PRs after a fresh
   review and green CI, as GOAL.md grants today (recommended). Or `ask` for
   every merge.
6. **Who files accepted cross-repo requests (§4).** The target owner, in its
   own tracker (recommended). Or the requester.
7. **Webhooks (§5).** Out for now, polling only (recommended). Or a relay
   (Tailscale or a public endpoint) in this ADR's scope.
8. **`RunScript` (§5).** Deferred to #503 (recommended). Or a restricted
   action here.
9. **The three spikes.** Close #1073, #1074 and #1078 as answered by this
   ADR, with their open remainders (process watches, `RunScript`, webhooks)
   kept as follow-up issues (recommended).
10. **Process watches (§5).** Keep using the harness's background mechanism
    with E5 attribution, or bring watcher-s1's semantics into the server.
    Not decided here: it needs its own measurement of handoff survival
    (#1078 question 2).
11. **Shadow length (§7).** At least seven days and ten intake decisions
    (recommended), or another bar.
12. **Architects of architects (§1a, #1167).** One owner per repo, with
    segment architects as depth-2 delegates on disjoint path scopes, intake
    and landing kept by the owner (recommended). Or several owners per repo
    with a landing owner (rejected in §1a).
13. **Segment landing (§1a).** Per-segment integration branches merged once
    per wave, or segment PRs straight into the owner's queue on `dev`. The
    recommendation is to let #1167's dry run decide and to file O14 only
    after it.
14. **Standup (§7a).** Compiled by flk with no model call, weekdays 08:00
    local, delivered to the notification log and a channel (recommended). Or
    written by the owner in a daily episode (about 57k units a day).
15. **Lift approval (§7b).** Each lift is an issue labelled `lift` in the
    target repo, or an ADR-0023 decision when it is the owner's own repo
    (recommended). Or one weekly batch for the operator to approve.
16. **Miner scope (§7b).** Read only the transcripts of agents on the
    owner's edges, under the profiles listed in its binding (recommended).
    Or every session on the host, which is broader and crosses accounts.
17. **Lift review budget (§7b).** At most one review a day per owner
    (recommended), or a different cap.
