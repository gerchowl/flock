# ADR-0049: Definable agent types and workflows: a small executable runtime, and one authority model

- Status: Proposed
- Date: 2026-11-03
- Issues: none filed yet. The owner asked for this ADR on 2026-11-03 ("go for
  all") after the GUI vision reviews. Related:
  [#694](https://github.com/gerchowl/flock/issues/694) (`flk land`),
  [#1184](https://github.com/gerchowl/flock/issues/1184) (ADR-0034 Q6,
  `architect_may_grant`), [#1799](https://github.com/gerchowl/flock/issues/1799)
  (operator-edited permission policy),
  [#1600](https://github.com/gerchowl/flock/issues/1600) /
  [#1603](https://github.com/gerchowl/flock/issues/1603) (work projection and
  records), [#2718](https://github.com/gerchowl/flock/issues/2718) (durable
  work), [#573](https://github.com/gerchowl/flock/issues/573) /
  [#965](https://github.com/gerchowl/flock/issues/965) (goal-s1 migration) and
  [#822](https://github.com/gerchowl/flock/issues/822) (caps).
- Decision owner: repository owner. W1–W8 below are recommendations awaiting
  the owner's decision.
- Builds on: ADR-0004 (repo `.flk.toml`), ADR-0021 (model registry and tiers),
  ADR-0027 (delegation edges), ADR-0028 (observation and registered waits),
  ADR-0030/0031 (goals), ADR-0032 (compiled context layers), ADR-0033 (forked
  reviews and the brief template), ADR-0034 (question routing), ADR-0035
  (resident owner, binding authority, shadow migration), ADR-0036 (queue, forks,
  permission presets, S6e), ADR-0041/0042 (typed scoped configuration),
  ADR-0044 (lifecycle) and ADR-0045 (durable work and supervision loops).
- Depends on: ADR-0048 (proposed, GUI object model), written in parallel; this
  ADR uses its Run and Workflow objects by title only and claims nothing about
  its final text. Also depends on ADR-0045 slices 1–12.
- Amends (on acceptance): ADR-0034 §6 (who answers permission prompts),
  ADR-0036 §10b S6 and the S6e "Authorization and proposals" table (who may
  call `agent.decide`). See W5.
- Implemented: none. The baseline describes `origin/dev` at `c70c23c383ec`.

## Context

The owner runs a repository today with three things that are not flk: the
architect skill (a 170-line playbook loaded as role text), goal-s1 (an external
stop and park shim) and private `.fleet/` shell scripts that wait for green CI,
build landing batches and drive them to merge. ADR-0035 §7 already maps most of
`.fleet/` onto a resident owner, and ADR-0045 supplies the durable wake,
deadline and escalation machinery that the scripts lack. What neither ADR
defines is a way to **declare a process** (implement, review, fix, land,
release) so that flk, rather than a shell loop or a model's memory, holds its
structure across compaction, restart and host handoff.

Two reviews of the GUI vision (2026-11-02) converged on the same point. The
first proposed declarative `.flk/workflows/*.toml`, agent types made of "role
text + model tier + permission preset + question policy + caps", and the rule
"skill = judgement, workflow = structure flk enforces, config = knobs". The
second warned that "a diagram alone cannot supply" a runtime: each operation
needs "a server owner, authority check, durable transition and recovery rule",
and runs must be pinned "to definition and rule hashes". It also found a
policy conflict that must be fixed before the GUI exposes delegated approvals:

- ADR-0034 §6 lets the **architect** answer a delegate's in-worktree tool
  permission (`allow once` / `deny`), and lets it answer widening permissions
  whose class the operator lists in `[questions] architect_may_grant`.
- ADR-0036 §10b (S6) says `agent.decide` accepts a decision "only from an
  authenticated, `operate`-scoped token principal"; "an agent message, or a
  structured harness's MCP lease never authorizes a decision". The S6e
  "Authorization and proposals" table repeats it: `agent.decide` needs
  "`operate` or the existing specific operator authority, unchanged", and
  "MCP is not an operator credential".

So the same prompt is architect-answerable on a PTY Claude delegate (through
`hook.decide`, ADR-0034 §1) and operator-only on a structured one. And
`architect_may_grant` is a config key, while ADR-0035 §6.2 says every value
that grants authority is binding state that no agent-editable file may carry.

## Inspected baseline

| Fact | Evidence |
| --- | --- |
| `agent.decide` authorises only a verified token: `Principal::decision_subject` returns `NotAuthorized` for `Operator`, `Agent` and `Lease`. | `src/structured/permissions.rs` (`Principal`) |
| ADR-0034 Q6 (#1184: PermissionRequest routing and `architect_may_grant`) is open; no source file names `architect_may_grant`. The conflict is in accepted text, not yet in code. | `gh issue view 1184`; `grep -rn architect_may_grant src` is empty |
| `flk land` (#694) is open. Landing is done by `.fleet/greenqueue.sh` / `chaingq.sh` (poll `gh pr view` every 120 s, up to 150 times; refuse a moved head; judge each check name by its best outcome because superseded runs leave `CANCELLED` entries), `batch.sh` (build `land/<name>` from `origin/dev`, one squash commit per member, stacked members as three-way deltas, defer conflicts, open a batch PR with rebase auto-merge, later close members with `landed-in-batch`) and `landbatches.sh` (serial driver over a `plan` file; one automatic rerun of failed jobs, owner decision 2026-10-11; rebase when `BEHIND`; a `RED` file stops everything). | the operator's private `.fleet/` scripts |
| Those scripts encode recovery by file flags (`.pr`, `.closed`, `.rerun`, `RED`), resume by re-reading them, and clean landing worktrees on exit because failed builds once filled a disk. None is attested, durable in flk, or visible to it. | same |
| The architect skill mixes three kinds of content: judgement (quality bar, briefs, diagnosis), structure (§4 "land serially", spawn freeze, 3 fix rounds, reviewer per round) and knobs (cap 4, `CARGO_BUILD_JOBS`, `--silence 10m`). | architect skill §§0b, 2, 4 |
| `delegate start` is composed client-side (worktree, start, brief submit, readiness, rollback). | ADR-0027 §6 |
| A review is a comment ending `VERDICT:`; ADR-0035 §6.4 makes `ReviewRecorded` at a head SHA the only valid fact and rejects parsing comments. | ADR-0035 §6.4 |

## Decisions at a glance

| | Recommendation | Main rejected alternative |
| --- | --- | --- |
| **W1** agent type | A versioned `.flk/agents/<name>.toml`: role text reference (ADR-0032 layer 1, hash-pinned), harness and model constraints within the operator's registry (ADR-0021 §§1–2), **requested** capabilities, a permission-preset ceiling, a question policy (narrowing only, ADR-0034 §2) and caps. Grants are separate operator binding state; a type never grants. | Types that carry their own grants (a repo file widening authority). |
| **W2** workflow definition | Versioned declarative `.flk/workflows/<id>.toml`, schema `flk.workflow/1`, validated statically, read from the default branch and **adopted by hash** by the operator; runs pin definition, type, role and rule hashes. GUI: viewer and launcher, then a form editor that round-trips the same file, then a visual builder. | A general scripting language or a GUI-native graph store. |
| **W3** vocabulary | Nine closed operations: assign, spawn, await, collect-evidence, request-review, request-decision, land, release, file-issue. Each has one server owner, an authority check, ADR-0045 durable transitions, a deterministic effect identity and a reconcile-before-retry rule. | Arbitrary `run = "<shell>"` steps. |
| **W4** runs | Durable `RunId` records in the ADR-0045 acknowledged log and checkpoints, a typed event ledger, a step DAG with attempts, waits, evidence and pending decisions, explicit `uncertain` states, and shadow mode (ADR-0035 §7) before any script retires. | A separate workflow database; runs held only in memory. |
| **W5** authority | Deciding a permission is operator authority. An architect exercises it only through an operator-issued, scoped, finite, revocable, audited **decide delegation** for a closed set of permission classes, checked by one resolver on both `hook.decide` and `agent.decide`. Forks, children and workflow steps never inherit it; capabilities intersect down the tree. | Keep both rules (two answers to one prompt), or make every permission operator-only (strands unattended trees). |
| **W6** separation | Skill = judgement text, workflow = structure flk enforces, config = knobs (ADR-0042 registry). One home per fact; a lint per boundary. | Letting skills keep structural steps "for now". |
| **W7** risks | Pre-mortem below; the five named risks each get a mechanism and a test. | — |
| **W8** phasing | P0 none; P1 read-only run view of existing loops; P2 definitions, adoption and shadow runs; P3 live enforcement and the form editor; visual builder after P3. | Builder first. |

## W1. Agent types

An agent type is a **request**, never a grant.

```toml
# .flk/agents/implementer.toml
schema  = "flk.agent-type/1"
name    = "implementer"
version = 4

[role]                    # ADR-0032 §1 layer 1
skill = "builder"         # a released role pack or a skill read by path
pin   = "sha256:…"        # refused at adoption if the bytes differ

[harness]                 # constraints, not a choice made for the agent
allow  = ["codex", "claude"]
prefer = "codex"

[model]                   # ADR-0021 §1: names from the operator's registry
allow = ["gpt-6.1-sol", "claude-sonnet-5-5"]

[requests]                # what the type needs; grants live elsewhere
capabilities      = ["worktree.write", "branch.push", "pr.open"]
permission_preset = "worktree-edit"   # ADR-0036 §10b, a ceiling by rank
questions         = "route"           # ADR-0034 §2: route | deny only

[caps]
max_instances = 4         # per run; the host cap (#822) still applies
max_wall_ms   = 14400000
max_turns     = 400
```

- **Role.** Layer 1 of ADR-0032 §1, pinned by hash as ADR-0032 already does
  for the architect skill. Layer 3 (the task) comes from the brief the
  workflow step supplies, compiled per ADR-0032 §5 and laid out per the
  ADR-0033 §7 template. A type has no other prompt text.
- **Harness and model.** ADR-0021 §1: the operator declares models once; a
  type may only name a subset. ADR-0021 §2: an agent chooses within what it was
  given, never the tier. A type naming an undeclared model fails adoption.
- **Capabilities** are a closed set: `worktree.write`, `branch.push`,
  `pr.open`, `spawn` (with a depth not above ADR-0014 §5's
  `max_spawn_depth = 2`), `fork`, `loop.manage` (ADR-0045 "Subscription
  authority"), `review.record` (ADR-0035 §6.4), `network`, `check.run`
  (ADR-0045 D11). `merge`, `release`, `config.set`, `grant`, `decide` and any
  lifecycle-widening verb are **not requestable**: those are workflow
  operations the server performs under binding authority (W3, W5), never
  agent capabilities.
- **Question policy.** `route` or `deny` only. `ask_human` and
  `ask_operator` widen the line to the human, which ADR-0034 §2 reserves to an
  operator verb.
- **Grants** are binding state, recorded by the operator verb
  `flk grant type <repo> <type>@<hash> --capabilities … --preset …` as a
  durable `TypeGrantSet { repo, type, def_hash, capabilities, preset, caps,
  by }`. As in ADR-0035 §6.2, `set` is refused for an attested agent caller
  (exit 13), is loud (notification and standup) and is never reloaded from a
  file an agent could edit.
- **Effective set** at spawn: `requested ∩ granted ∩ ceiling(parent) ∩
  ceiling(run)` (W5). If a requested capability is missing, the spawn
  **refuses** with `capability_ceiling` naming the missing items. It never
  runs degraded, because a builder that silently lacks `branch.push` fails an
  hour later and looks like a model error.

**Rejected.** *Grants in the type file*: the repo is writable by the agents it
would empower (ADR-0035 §6.2). *Clamp silently to the effective set*: hides
the cause of later failure. *Types as free-form skill files*: no validation,
no hash, no ceiling.

## W2. Workflow definitions

### Format and example

```toml
# .flk/workflows/land-issue.toml
schema  = "flk.workflow/1"
id      = "land-issue"
version = 7

[inputs]
issue = { type = "issue" }
brief = { type = "brief" }          # a repo path, read at launch, hashed

[[step]]
id         = "implement"
op         = "spawn"
agent_type = "implementer"
brief      = "inputs.brief"
worktree   = { base = "dev", branch = "{kind}/{inputs.issue}-{slug}" }
blocked_while = ["landing_in_flight", "base_red"]   # skill §4.5 spawn freeze

[[step]]
id    = "built"
op    = "await"
needs = ["implement"]
until = "delegate:implement phase in [finished, needs_human, gone]"
then  = { require_receipt = true }  # ADR-0045 typed receipt, head == PR head

[[step]]
id    = "evidence"
op    = "collect-evidence"
needs = ["built"]
kinds = ["receipt", "pr_head", "ci_run"]

[[group]]                           # the only loop: bounded rounds
id     = "review"
needs  = ["evidence"]
rounds = { max = 3, on_exhausted = "decide-split" }   # skill §4.2
  [[group.step]]
  id         = "review"
  op         = "request-review"
  agent_type = "reviewer"

  [[group.step]]
  id   = "fix"
  op   = "assign"
  to   = "implement"                # the same warm delegate (skill §4.3)
  when = "review.verdict == blockers"

[[step]]
id    = "decide-split"
op    = "request-decision"
class = "scope"
options = ["split", "escalate", "abandon"]

[[step]]
id    = "land"
op    = "land"
needs = ["review"]
when  = "review.verdict in [approve, nits]"
mode  = "serial"                    # or "batch"
ci    = { rerun_failed = 1 }        # owner decision 2026-10-11
```

### Rules

1. **Closed grammar.** `op` is one of W3's nine. `when` and `until` are typed
   predicates over step outputs and the ADR-0028 §5 / ADR-0045 source grammar.
   There are no shell strings, no general expressions and no templates other
   than substitution of typed inputs into the few fields that accept them
   (branch names, titles). A predicate kind ADR-0045 does not support yet is
   refused at validation, never "watched" (ADR-0045 "Boundaries").
2. **Structure is a DAG** of steps and groups (`needs`). Cycles and dangling
   `needs` refuse. The only repetition is a group's bounded `rounds` and a
   step's bounded `attempts`; fan-out (`for_each` over a typed input list) has
   a declared `max_parallel` and also respects the host cap (#822).
3. **Static validation** before adoption and again at launch: schema, DAG,
   that every `agent_type` is adopted, that every requested capability is
   grantable, that each step's predicates are readable under the launcher's
   grants (ADR-0045 "Subscription authority"), that each external-effect step
   has a reconcile kind (W3), and the bounds in W4. Unknown fields refuse:
   definitions are authored, not exchanged, so they fail closed. A server
   older than the schema refuses with `unsupported_schema`, never by
   ignoring a field.
4. **Source and adoption.** Definitions are read from the default branch's
   blob, not from the worktree a run happens to start in, for ADR-0004 §4's
   reason: a branch must not edit the policy that governs it. A definition
   does nothing until the operator adopts it: `flk workflow adopt <repo>
   <path>@<blob>` records `WorkflowAdopted { repo, path, blob, def_hash,
   schema, by }`, the same pattern as ADR-0035 §6.2's `owner policy adopt`.
   A merged edit is a proposal until adopted. Node-scoped definitions
   (operator config) follow the same adoption rule, because ADR-0035 §6.2
   rejected host config as an authority source for the same reason.
5. **Pinning.** A run records `def_hash`, every agent type's hash, every role
   text pin (ADR-0032 §1 layer 1), the layer 2 commit (ADR index and traps
   file, ADR-0032 §1), the grant revision and the ADR-0042 resolved policy
   revision. A run never changes definition in flight; a new version applies
   to new runs only. No in-place migration of a running run: cancel and
   relaunch, with already landed effects recognised by reconcile (W3).

### GUI order

1. **Viewer and launcher** (P1/P2): list adopted definitions, show the
   pinned run graph and ledger, launch with typed inputs, cancel.
2. **Form editor** (P3): a schema-driven form that edits the same TOML with
   a format-preserving writer (comments and ordering survive a round trip).
   Saving produces a branch and a diff to review and merge, then an adoption
   prompt; the editor never writes adopted state directly. The round-trip
   test (W8) is a gate for shipping it.
3. **Visual builder** (after P3): edits the same file through the same
   validator. It ships only after the restart and failure recovery tests of
   W8 pass end to end, so the picture never promises more than the runtime.

**Rejected.** *A scripting language (Starlark, Lua, JS)*: every escape hatch is
a hidden script with its own credentials and restart behaviour, exactly the
failure ADR-0045 D6 rejected. *GUI-native graph storage*: two sources of
truth; diffs and review vanish. *YAML*: the repo already standardises on TOML
(ADR-0004, ADR-0041); no reason to add a second format.

## W3. The executable vocabulary

### Common effect protocol

Every operation that changes state outside its own run record follows one
protocol, built on ADR-0045's acknowledged write (slice 1) and its
"exactly-once is not offered" rule (D3; ADR-0044 Q3):

1. **Intend.** Commit `run.effect_intended { effect_id, run, step, attempt,
   kind, subject, preconditions }` before any call. `effect_id =
   H(run_id, step_id, attempt, subject_revision)`: deterministic, so a replay
   computes the same identity.
2. **Act** through the operation's server owner, passing `effect_id` where
   the target can carry it and the guard preconditions where the target can
   enforce them (for example merge only at an expected head).
3. **Observe.** Commit `run.effect_observed { effect_id, outcome: applied |
   refused | uncertain, evidence }`.
4. **Reconcile, then maybe retry.** A crash, timeout or lost response between
   1 and 3 leaves the effect `uncertain`. Recovery queries the subject by the
   effect's identity. *Proven applied* records `applied`. *Proven absent* and
   *guarded* (re-execution cannot double-apply) may retry under the **same**
   `effect_id`. Anything else becomes a `request-decision` to the operator.
   No retry ever mints a new identity while an earlier attempt is unresolved.

### Operations

| Op | Server owner | Authority check | Durable transitions | Identity and retry | Recovery after crash, restart or handoff | External-effect safety |
| --- | --- | --- | --- | --- | --- | --- |
| **assign** | work service (ADR-0045 D1/D2) and the target's queue (ADR-0036 §3) | run principal holds supervisor authority over the WorkId; target is a live agent the run spawned or the launcher's attested delegate (ADR-0027 §1) | `work.updated` (assignment generation +1, expected revision), queue `message` item | queue ItemId = `effect_id`; same ID and body observes prior result, different body refuses (ADR-0045 "Durable subscription and delivery protocol", step 4) | refold; a committed item is not re-sent to stdin; an unhandled one stays owed and escalates at its deadline (ADR-0045 D4) | internal only |
| **spawn** | server-side start composition (moves ADR-0027 §6's client-side `delegate start` into the server; a prerequisite slice) | effective capabilities (W1/W5), host cap (#822), depth (ADR-0014 §5), `blocked_while` gates | intended → spawn record (ADR-0036 "Uncertain spawn records") → edge recorded (ADR-0027 §1) → step `in_flight` | spawn request ID = `effect_id`; agent name derived from run and step | look up the spawn record by `effect_id`; an uncertain spawn reconciles per that amendment; never a second agent for one ID | the worktree is a recorded cleanup obligation, released by ADR-0027 §5 reap on merge or by an explicit run cleanup |
| **await** | ADR-0045 loop service (LoopId); the run dispatcher is the wake recipient | source-read grants at registration and every firing (ADR-0045 "Subscription authority") | loop registered with a source cut; occurrence → step `ready`; healthy waits per ADR-0045 D4 | the occurrence key (same protocol, step 2) | the loop survives; the run refolds and consumes owed occurrences once | none; deadlines and the D8 chain apply |
| **collect-evidence** | `work.evidence.attach` and the D11 check runner | assigned agent or run on its behalf; `check.run` for checks | evidence committed with typed identity (receipt/revision/hash, CI run/attempt, check invocation) | dedupe by evidence identity | re-verify; a claim (`DONE:` line, link) stays a claim until verified (ADR-0045 "Record and authority model") | reads only; checks sandboxed |
| **request-review** | review service (ADR-0035 §6.4) plus spawn | reviewer type grant; independence: not the builder and not anyone who steered it this round | spawn reviewer with edge label `review:<pr>` → `ReviewRecorded { head_sha, verdict }` → step done | one fresh reviewer per round; `effect_id` per round | reviewer gone without a record → failed attempt, next attempt is a new reviewer; a moved head invalidates the verdict | a `VERDICT:` line is a claim; only `ReviewRecorded` at the current head counts |
| **request-decision** | S6e question service | answer needs operator `answer`/`operate` or a decide delegation for that class (W5) | `QuestionId` committed; answer custody per S6e; step resumes on commit | `QuestionId` derived from `effect_id` | the question persists; ADR-0045 D8 escalation and reminders apply; no default answer on timeout (S6e Q3) | none |
| **land** | the landing worker (`flk land`, #694) holding ADR-0035 §6.3 layer one and `flk/land-gate` | binding merge policy (ADR-0035 §6.6); `ReviewRecorded` approve at the current head; green CI at that head by run/attempt (ADR-0045 PR/CI); base not red | one in-flight landing per (repo, base): intended → merge or batch PR → observed merged | merge guarded by expected head (`--match-head-commit`); batch branch and PR named from `effect_id` | query PR: merged at head → applied; open, same head → absent, guarded retry allowed; head moved → refuse, back to review; batch PR found by branch → reuse, never a second | no duplicate merge (guard); rerun, update-branch and member close are their own effects |
| **release** | release dispatcher (the repo's release workflow) | a non-delegable operator `request-decision` immediately before, bound to the exact commit (ADR-0035 §6.6 release `ask`) | decision → intended → dispatch → observed run → tag observed | dispatch input carries `effect_id`; release version fixed at decision time | find the workflow run by `effect_id`, the tag by version; uncertain → operator decision; **never auto-retried** | the rollout to hosts is out of scope; a follow-up `file-issue` records it |
| **file-issue** | GitHub write adapter (ADR-0035 §6.3 agent write identity and its O4 verbs; ADR-0045 D10's credential is read-only) | `issue.write` grant for the target repo; search first (ADR-0033 §6) | intended → created → observed number | hidden body marker with `effect_id` | search by marker: found → applied; absent → retry same ID | marker search prevents duplicate issues |

Notes on the table:

- **Landing makes the scripts first-class.** `.fleet/` behaviours map to typed
  facts: a moved head is a guard failure; "best outcome per check name" is
  ADR-0045's run/attempt identity; `RED` is a run in `blocked` with a base-red
  observation that `blocked_while = ["base_red"]` reads; the single automatic
  rerun is the definition's `ci.rerun_failed = 1`, recorded as an attempt;
  `BEHIND` is an update-branch effect; deferred stacked members are typed
  `deferred { reason: conflict | parent_deferred }` outputs; worktree cleanup
  is an obligation, not a shell `trap`.
- **No LLM in the dispatcher.** The run dispatcher is a deterministic reducer
  over committed facts. Judgement happens only inside agents (spawn, assign,
  request-review) or humans (request-decision). This is what "structure flk
  enforces" means.
- **Not in the vocabulary.** Deploys, host applies and server lifecycle stay
  `never` (ADR-0035 §6.6). Notifications are ADR-0045 D8's, not a step.
  Adding an operation needs an ADR amendment with all seven columns filled.

**Rejected.** *A `run` step with a shell command*: recreates the unsupervised
processes, ambient credentials and restart failures of ADR-0045 D6's rejected
option. *Retry by re-calling*: duplicates merges, issues and releases on a lost
response.

## W4. Runs

- **Record.** `RunId` is server-minted. A run holds its pins (W2), launcher
  principal, run ceiling (W5), mode (`live` or `shadow`), state, a step
  instance table, linked WorkIds (a run that creates work creates ADR-0045
  work nodes, it does not shadow them) and pending decisions. Run states:
  `admitted`, `running`, `waiting`, `blocked`, `uncertain`, `succeeded`,
  `failed`, `cancelled`. Step instance states: `pending`, `ready`,
  `in_flight`, `waiting`, `succeeded`, `failed`, `uncertain`, `skipped`,
  `cancelled`; each carries attempts, and each attempt its effects.
- **Storage.** The same acknowledged log, record-carrying checkpoints and
  rotation fence as ADR-0045 D1. No second store. Events: `run.started`,
  `run.step_changed`, `run.effect_intended`, `run.effect_observed`,
  `run.decision_requested`, `run.decision_resolved`, `run.paused`,
  `run.cancelled`, `run.finished` and record-carrying `run.checkpoint`. Each
  carries actor, operation ID, run revision and `def_hash`. Readers skip and
  preserve unknown kinds (ADR-0028 §3a).
- **Bounds** (proposed, to be measured like ADR-0045's): 32 active runs per
  authority, 64 steps per definition, `max_parallel` at most 16, 8 attempts per
  step instance, 4 KiB per step instance record. Admission refuses before a
  bound is crossed; nothing is truncated. Whether runs share ADR-0045's 8 MiB
  checkpoint budget or reserve their own is owner question 3.
- **Dispatch.** Driven by committed changes and deadlines, never by a sleep
  loop (ADR-0045 "Observation sources and coverage"). On restart the server
  refolds runs before serving status, reconciles every `uncertain` effect
  (W3 step 4), and only then admits new steps.
- **Handoff.** Runs belong to the ADR-0045 D5 authority and epoch. Same-host
  and cooperative cross-host transfer carry them like loops; uncertain effects
  transfer as uncertain and reconcile on the target before the run resumes.
- **Uncertainty is visible.** The run view shows the dependency graph beside
  the chronological ledger, each attempt, wait (with source health and expiry),
  evidence item, pending decision and every `uncertain` effect with what is
  known and which reconcile query ran. An unknown is never rendered as
  success or failure.
- **Shadow runs** (ADR-0035 §7). In `shadow`, reads, waits and evidence are
  real; every effect is replaced by `RunWouldAct { effect_id, kind, subject,
  args }`, and `spawn` does not start an agent. The run instead **binds to
  the actual effects** of the script or architect on the same subject (the
  real PR merged, the real batch PR opened) and records divergence: would-act
  versus did-act, timing, and events the script missed. The server refuses
  effect verbs for shadow runs (`run_in_shadow`, exit 13), as ADR-0035 §7 does
  for owners.
- **Cutover**, per workflow: at least ten shadow runs over at least seven days
  (ADR-0035 §7's bar), no unexplained effect divergence, and an operator
  review; then the operator disables the script and switches the workflow to
  `live`. ADR-0045 D7's rule holds: never two uncoordinated drivers. A live
  run that observes an effect it did not intend on a subject it holds (a
  merge by a script still running) records `foreign_effect` and blocks.

**Rejected.** *Runs as a separate database*: ADR-0045 D1 already rejected a
second store. *Runs as agent goals*: a goal is one agent's; a run outlives and
spans agents. *Skip shadow*: the scripts are the only thing landing code today
(ADR-0035 §7's argument against a big-bang switch).

## W5. One authority model

### Decision

**Deciding a permission prompt is operator authority. An architect may
exercise part of it only as an explicit, operator-issued decide delegation.**

```text
DecideDelegation {
  id, revision, grantor: operator principal,
  delegate: Agent(AgentId, generation) | ParentArchitect | Owner(repo),
  scope:   { repo, subtree: attested ADR-0027 edges of the delegate, depth <= 2 },
  classes: [worktree_tool | plan_approval | form_elicitation
            | widen_directory | widen_network | widen_sandbox],
  decisions: [allow_once, deny],          # allow_always is never delegable
  expires_at,                             # finite; default 8 h, at most 7 d
  by, created_at }
```

- **One resolver.** `decide_authority(principal, request) → Operator |
  Delegated(id, revision) | Refused(reason)` is called by `hook.decide`
  (PTY, ADR-0034 §1) and `agent.decide` (structured, ADR-0036 §10b) alike.
  The structured `Principal` gains a delegated variant: the architect's
  verified agent identity (ADR-0036 §1a) **plus** a live matching delegation,
  rechecked at the mutation fence. An MCP lease's scopes alone still
  authorise nothing (S6e).
- **Binding state, operator verbs.** `flk authority delegate|revoke|show`;
  durable `DelegationSet` and `DelegationRevoked` events; refused for an
  attested agent caller (exit 13); loud like ADR-0035 §6.2 (notification,
  channel, standup). Revocation takes effect at the next decision; a
  decision already committed stands.
- **Audit.** Every permission close records `principal`, `delegation_id` and
  `delegation_revision` beside the existing S6 audit fields, so "who allowed
  this" always names a human grant.
- **Never delegable**: trust dialogs and login (ADR-0034 §6 rows 5–6),
  `allow_always` session grants (S6), preset escalation (ADR-0036 §10b),
  question-policy widening (ADR-0034 §2), model-tier changes (ADR-0021 §2),
  resume (ADR-0036 §1a), and the merge, release and protection gates
  (ADR-0035 §6.6). A `request-decision` step declares its class; `release`,
  `deploy` and `outside_merge` are non-delegable classes.
- **Defaults keep ADR-0034's accepted behaviour, explicitly.** The node-scoped
  operator policy `authority.delegation.defaults` (ADR-0042 registry, `G/N`
  only, so a repo file cannot set it) ships with one row: `ParentArchitect`
  for `worktree_tool`, `plan_approval` and `form_elicitation`, `allow_once`
  and `deny`. That is ADR-0034 §6 rows 1–3. The widening classes start empty,
  as `architect_may_grant` did. The row is visible in `flk authority show`
  and the GUI, and the operator can remove it.
- **`architect_may_grant` is withdrawn** before it ships (#1184 is open). Its
  meaning moves into delegation classes; #1184 implements against this
  resolver instead of a config key.

### Inheritance and ceilings

- A delegation is bound to `(AgentId, generation)` or resolved through an
  attested role at decision time. It is **never inherited**: not by a fork
  (ADR-0036 §9 creates a new AgentId; lineage grants nothing, as S6e says for
  transcripts), not by a spawned child, a reviewer or a run.
- Capabilities only shrink down the tree: `effective(child) = requested(type)
  ∩ granted(type) ∩ effective(parent) ∩ ceiling(run)`. Permission preset rank
  is at most the parent's (ADR-0036 §10b rank). Question policy only narrows
  (ADR-0034 §2). Read-only forks pass a tool subset of the parent (ADR-0036 §9).
- **Run ceiling.** The operator grants a workflow per repo:
  `WorkflowGrant { repo, def_hash, capabilities, effects: [land, file-issue,
  …] }`. A run launched by the operator gets that ceiling. A run launched by an
  architect or owner gets `WorkflowGrant ∩ effective(launcher)`, and needs a
  `workflow.launch` grant for that definition. Runs never hold decide
  authority; a permission prompt inside a run-spawned agent goes to the same
  resolver as any other, along the spawned agent's edge: the launching
  architect's edge, or, for an operator launch, `ask_operator` as a
  server-started root (ADR-0034 §2).
- **Resident owner.** An owner's limits are already binding state (ADR-0035
  §6.2); its delegations are just more of it, and a shadow owner holds none.

### Why this and not the alternatives

- *Keep both texts*: one prompt with two answers depending on runtime
  (`pty` or `structured`); GUI approval cards would have to explain which
  rule applies.
- *Operator-only for every permission*: consistent but strands every
  unattended tree on routine in-worktree prompts, which ADR-0034 was written
  to stop; the resident owner (ADR-0035) could not run.
- *Architect authority from the edge alone*: makes authority a by-product of
  who started whom, which ADR-0014 and ADR-0035 §6.2 reject; nobody could see
  or revoke it.

## W6. Separation: skill, workflow, config

| Kind | Holds | Never holds | Home | Enforced by |
| --- | --- | --- | --- | --- |
| **Skill** (judgement) | how to plan waves, write a brief, diagnose a finding, judge a review, the quality bar | step order, gates, round caps, timers, commands that perform a step | role text, ADR-0032 layer 1, hash-pinned | review; a skill lint flags `flk land`, `gh pr merge`, `sleep`-loops and similar step commands once the matching workflow is live |
| **Workflow** (structure) | which steps, in which order, behind which gates, with which evidence, bounded rounds and attempts, who decides | durations, host caps, credentials, prose instructions beyond brief references | `.flk/workflows/`, `.flk/agents/`, adopted by hash | the W2 validator: refuses timing literals for keys the ADR-0042 registry owns, and free text outside `description` |
| **Config** (knobs) | timeouts, poll intervals, host cap, build jobs, notification bindings, delegation defaults | process structure, judgement | ADR-0042 registry and resolver | ADR-0042 §3 validation; `G/N` scoping for anything authority-bearing |

Test for a new fact: if it varies by host or environment it is config; if it
varies by process it is workflow; if it needs judgement it is skill. A fact
lives in exactly one row.

### Migration (extends ADR-0035 §7's table)

| Today | Becomes |
| --- | --- |
| skill §1 waves, §2 brief rules, quality bar, §4.3 diagnosis | stay in the skill (judgement) |
| skill §2 dispatch command, §3 background waits | `spawn` and `await` steps |
| skill §4.1–4.2 fresh reviewer, re-review per round, cap 3 | `request-review` group with `rounds.max = 3` |
| skill §4.4–4.6 land serially, spawn freeze, merge with `--match-head-commit` | `land` (serial or batch), `blocked_while`, the land guard |
| skill §4.7 cleanup after merge | ADR-0027 §5 reap plus the spawn cleanup obligation |
| skill §0b.4 cap, `CARGO_BUILD_JOBS`, `--silence 10m` | config keys |
| skill §0a GOAL/TASKS/HANDOFF files | ADR-0035 §7 rows (goal record, compiled layers) |
| `greenqueue.sh`, `chaingq.sh` | `await` on typed CI at the exact head, then `land` admission |
| `batch.sh`, `landbatches.sh` | `land { mode = "batch" }` with typed deferrals and `ci.rerun_failed = 1` |
| goal-s1 arm and park | ADR-0030 §2 stop ladder over ADR-0045 loops (ADR-0045 D7 goal-s1 paragraph); a run's waits are registered waits, so rung 4 parks |

Each row retires only after its workflow passes shadow cutover (W4).

## W7. Consequences, risks and pre-mortem

**Consequences.** One place shows how a repo's work moves, and it survives
compaction and restart. The architect's context loses mechanics it carries
today. The owner gains a second artifact to review (definitions) and one more
operator verb family (adopt, grant, delegate). `flk land` (#694) and
server-side start composition become prerequisites rather than options.

**Pre-mortem.** Assume it shipped, and a year later the owner turned it off.
Why?

| Failure | Prevention and observable outcome |
| --- | --- |
| **Unexecutable diagrams**: the builder drew graphs the runtime could not run | GUI editors write only the validated file; the visual builder ships after P3's recovery tests; unsupported predicates refuse at validation. |
| **Hidden scripts**: someone kept a shell loop "just for landing" | No shell op exists; `foreign_effect` blocks a live run on a subject another driver touched; ADR-0045 D7's no-two-drivers cutover. |
| **Duplicate external effects**: a lost response produced two merges or two releases | effect intent before call, deterministic `effect_id`, guarded merge, marker-based reconcile, release never auto-retried. |
| **Authority creep**: a repo edit or a fork quietly widened access | grants and delegations are binding state; types only request; ceilings intersect; delegations never inherit; every change is loud. |
| **Definition sprawl**: forty near-identical workflows | adoption by hash per repo; validator refuses timing literals and prose; the bound of 64 steps per definition; the GUI lists adopted definitions only. Starting with two workflows (land-issue, release) is part of P2. |
| A restart mid-landing left the run claiming "landed" | `uncertain` until reconcile proves it; status served only after refold. |
| A wait went silent while CI hung | ADR-0045 D4 healthy-wait limits and D8 escalation apply to run waits unchanged. |
| A run kept spawning while dev was red | `blocked_while = ["base_red"]` reads the typed base observation, not a file flag. |
| The definition changed mid-run | runs are pinned; a new version needs a new run. |
| The architect approved a sandbox widening it should not have | widening classes need an explicit delegation; the audit names it; revocation is one verb. |

Residual risks: the same-UID cooperative identity gap of ADR-0035 §6.2 (closed
only by O19a's `flk-agent` user) applies to every agent-attested check here.
External reconciliation depends on the provider's API answering truthfully and
within quota (ADR-0045 D10). Shadow comparison cannot see effects the scripts
made outside GitHub.

## W8. Phasing and acceptance

- **P0: none.** Depends on ADR-0048 (proposed, GUI object model) phase 1 and on
  ADR-0045 slices 1–12 (acknowledged writes through the CI adapter).
- **P1: read-only run view of existing loops.** Project ADR-0045 loops,
  delegate edges and queued start intents (ADR-0027 §6), and armed goals as
  implicit runs, with every unknown shown as unknown. No definitions, no
  writes.
- **P2: definitions, adoption and shadow runs.** Schema, validator, adopt,
  type and workflow grants, the W5 resolver and delegation verbs (needed
  before #1184 ships), run records and the ledger, shadow mode for
  `land-issue` and `release`. GUI viewer and launcher.
- **P3: enforcement and the form editor.** Live mode per workflow after
  cutover; server-side spawn composition; `land` live (#694); the form editor.
- **Later: visual builder**, after P3's recovery tests pass.

Each slice is at most 400 production lines, with fail-first tests through the
public socket, CLI, MCP or wire ingress, isolated servers and the public fake
model, as ADR-0045 requires.

| Decision | Acceptance (fails before the change) |
| --- | --- |
| W1 | A type requesting `merge` or `ask_human` fails adoption. A type requesting `branch.push` without a grant refuses spawn with `capability_ceiling` listing it. An attested agent calling `flk grant type` gets exit 13. |
| W2 | A cyclic `needs`, an unknown op, a timing literal for a registry key, an unknown field or an unsupported predicate each refuse validation. A definition edited on a feature branch is not what a run on that branch loads. An unadopted blob cannot launch. A run's pins survive restart and a later adoption. The form editor round-trips a commented file byte-identically when nothing changed. |
| W3 | Inject a crash after `effect_intended` and before the call, and after the call before `effect_observed`, for land, file-issue and release against a fake GitHub: exactly one merge, one issue and one dispatch, with release raising an operator decision rather than retrying. A moved head refuses landing. A rerun failure at the same SHA is observed. |
| W4 | Restart between every pair of ledger events: the refolded run has the same step states, and uncertain effects reconcile before any new step is admitted. A shadow run issues no effect verb (`run_in_shadow`) and reports divergence against a scripted fake driver. A live run observing a foreign merge blocks with `foreign_effect`. |
| W5 | The same in-worktree prompt, on a PTY and a structured fake delegate, gets the same answer path. With the default row removed, the architect's decide refuses on both. A widening prompt is architect-answerable only with a matching live delegation, refuses after expiry or revocation, and the audit names the delegation. A fork of the architect cannot decide. A run-spawned child cannot exceed its launcher's preset rank. |
| W6 | The validator refuses a workflow carrying a registry-owned duration. The skill lint flags a landing command in a role text whose workflow is live. |
| W7 | Every pre-mortem row above maps to at least one W1–W6 test. |

## Owner questions

1. **Default delegation row (W5).** Ship `ParentArchitect` for `worktree_tool`,
   `plan_approval` and `form_elicitation` on by default, preserving ADR-0034
   §6 (recommended), or off, making every structured and PTY prompt
   operator-only until the operator grants it?
2. **Delegation expiry (W5).** Default 8 hours, maximum 7 days
   (recommended), or no expiry with revocation only?
3. **Checkpoint budget (W4).** Runs share ADR-0045's 8 MiB aggregate budget
   (recommended, one store and one bound), or reserve a separate 4 MiB?
4. **Launch authority (W5).** May a resident owner or architect launch adopted
   workflows under a `workflow.launch` grant (recommended), or only the
   operator in P2?
5. **First workflows (W8).** `land-issue` and `release` in shadow first
   (recommended), or `land-issue` alone?

## Boundaries

- Deploys, host applies and server lifecycle remain outside the vocabulary
  (ADR-0035 §6.6). Rollout after a release stays an operator task, recorded by
  `file-issue`.
- Cross-host runs and remote spawn follow ADR-0045 D5 and #1380; nothing here
  adds a mesh command.
- Workflow-level cost budgets and provider-limit handling reuse ADR-0035 §6.5;
  this ADR adds no budget mechanism.
- The GUI objects (Run, Workflow) are defined by ADR-0048; this ADR defines
  their server semantics only.
