# ADR-0049: Definable agent types and workflows: a small executable runtime, and one authority model

- Status: Accepted (2026-11-03, owner)
- Date: 2026-11-03
- Issues: [#2909](https://github.com/gerchowl/flock/issues/2909) (release
  adapter prerequisite, W3). The owner asked for this ADR on 2026-11-03 ("go for
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
- Decision owner: repository owner. The owner accepted W1–W8 on 2026-11-03.
  The owner questions below remain open: the owner asked for more detail
  before deciding them.
- Builds on: ADR-0004 (repo `.flk.toml`), ADR-0021 (model registry and tiers),
  ADR-0027 (delegation edges), ADR-0028 (observation and registered waits),
  ADR-0030/0031 (goals), ADR-0032 (compiled context layers), ADR-0033 (forked
  reviews and the brief template), ADR-0034 (question routing), ADR-0035
  (resident owner, binding authority, shadow migration), ADR-0036 (queue, forks,
  permission presets, S6e), ADR-0041/0042 (typed scoped configuration),
  ADR-0044 (lifecycle) and ADR-0045 (durable work and supervision loops).
- Depends on: ADR-0045 slices 1–12. Related: ADR-0048 (proposed, GUI object
  model). Ownership is one-way: **this ADR owns workflow definitions (their
  identity, W2 "Workflow definition identity"), agent types and the run
  runtime; ADR-0048 owns only their GUI projection** (the Workflow and Run
  objects it renders) and cites this ADR, never the reverse.
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
  authenticated, `operate`-scoped token principal"; "a local UID alone, pane
  input, an agent message or a structured harness's MCP lease never
  authorizes a decision". The S6e
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
| **W5** authority | Deciding a permission is operator authority. An architect exercises it only through an operator-issued, scoped, finite, revocable, audited **decide delegation** for a closed set of permission classes, checked by one resolver on both `hook.decide` and `agent.decide` (decided by the owner). Defaults are a template adopted once into binding state. Forks, children and workflow steps never inherit it; capabilities intersect down the tree; workflow effects have their own grant; lost authority reroutes pending prompts to the operator. | Keep both rules (two answers to one prompt), or make every permission operator-only (strands unattended trees). |
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
- **Structured agents only.** Every harness a type allows is started as a
  structured session (ADR-0036). No type, step or run starts a PTY or
  terminal agent, and no run view, count, capacity figure or GUI surface
  counts or advertises PTY agents (owner direction 2026-11-02, the same rule
  as ADR-0048's structured-only fleet). Structured agents replace them; the
  PTY path survives only as legacy compatibility outside workflows.
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
needs = ["review"]
when  = "review.outcome == exhausted"   # the only trigger; see rule 2a
class = "scope"
options = ["split", "escalate", "abandon"]

[[step]]
id    = "land"
op    = "land"
needs = ["review"]
when  = "review.outcome == passed"   # last verdict approve or nits
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
   Step outcomes have their own predicate: `step.<id>.outcome == <state>` or
   `step.<id>.outcome in [<state>, …]` over `succeeded | failed | skipped`,
   and `<group>.outcome` over `passed | exhausted` (rule 2a). A step whose
   `when` reads `step.<id>.outcome` for a `failed` value is that step's
   **failure handler**; it must list `<id>` in `needs`, and the validator
   refuses one that does not. When a step instance fails and its handler is
   admitted, the failed instance is recorded `handled` (W4 "Success").
2. **Structure is a DAG** of steps and groups (`needs`). Cycles and dangling
   `needs` refuse. The only repetition is a group's bounded `rounds` and a
   step's bounded `attempts`; fan-out (`for_each` over a typed input list) has
   a declared `max_parallel` and also respects the host cap (#822).
2a. **Group outcome and exhaustion.** A group finishes with a typed
   `outcome`: `passed` (a round ended without its repeat condition, here a
   verdict other than `blockers`) or `exhausted` (`rounds.max` reached with the
   repeat condition still true). Exhaustion is decided at the last round's
   deciding step (here `review`): when that round is round `max` and its
   verdict would repeat, the steps after it in that round (here `fix`) are
   `skipped`, so a group never ends on an unreviewed fix and the exhaustion
   step sees the last reviewed head. Steps that `needs` the group read
   `outcome` and the last round's outputs. A step named by `on_exhausted`
   must declare exactly `needs = [<group>]` and `when = "<group>.outcome ==
   exhausted"`, and no other dependency; the validator refuses any other
   shape, so the exhaustion handler cannot run early or on a passed group.
   On `passed` it is `skipped`; on `exhausted` every step whose `when`
   requires a passing verdict (here `land`) is `skipped`. A run that ends
   through the exhaustion step never reaches `succeeded` (W4 "Success needs
   approved Work"); the
   decision is its recorded output, and acting on `split` is a new launch,
   not an automatic one.
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
   schema, closure, by }`, the same pattern as ADR-0035 §6.2's `owner policy
   adopt`. A merged edit is a proposal until adopted.
   **Adoption pins the complete transitive content, never names.** At
   adoption the server resolves every name the definition reaches and records
   the result as `closure`: each `agent_type` name to the content hash of the
   adopted type, each type's role pack or skill to the hash of its bytes
   (ADR-0032 §1 layer 1), and any other file the definition or its types
   reference to its blob hash. `def_hash` covers the definition and its
   closure. A launch uses the closure, never a name lookup, so re-adopting
   or editing `implementer` later changes no adopted workflow; picking up a
   new type version means adopting the workflow again, which yields a new
   `def_hash`. A name that does not resolve to adopted content refuses
   adoption. Node-scoped definitions
   (operator config) follow the same adoption rule, because ADR-0035 §6.2
   rejected host config as an authority source for the same reason.
5. **Pinning.** A run records `def_hash` and its adopted closure (every agent
   type's hash and every role text pin, ADR-0032 §1 layer 1), the layer 2 commit (ADR index and traps
   file, ADR-0032 §1), the grant revision and the ADR-0042 resolved policy
   revision. Pinned grant and policy revisions are an upper bound, not a
   licence: W5 "Pinned revisions and current revocation" intersects them with
   current state at every check. A run never changes definition in flight; a new version applies
   to new runs only. No in-place migration of a running run: cancel and
   relaunch, with already landed effects recognised by reconcile (W3).

### Workflow definition identity

This is the identity ADR-0048's Workflow facet projects; it is defined
here and only here.

- **`WorkflowDefinitionId { project, workflow_id, def_hash }`.** `project`
  is the ADR-0042 project id of the repository; `workflow_id` is the file's
  `id`; `def_hash` is the adoption hash over the definition and its
  `closure` (rule 4). One `(project, workflow_id)` names a lineage of
  versions; each adopted version is a distinct `WorkflowDefinitionId`.
- Adoption refuses a second path in one project declaring an `id` already
  adopted from another path, so `workflow_id` is unique per project.
- A run references exactly one `WorkflowDefinitionId` for its whole life
  (rule 5). Grants (`WorkflowGrant`, `workflow.launch`, W5) are keyed by it,
  not by name.

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

1. **Intend.** Commit `run.effect_intended { effect_id, effect_key,
   payload_hash, run, step_instance, attempt, kind, subject, preconditions }`
   before any call.
   - The **effect key** is an immutable structured tuple, written once in
     this record and never recomputed from mutable state: `(run_id,
     step_instance_id, attempt, effect_kind, sub_effect)`.
     `step_instance_id = (step_id, fan_out_index | none, group_round |
     none)` is minted when the step instance is created (W4), so two
     fan-out instances or two review rounds of one step never share a key.
     `sub_effect` names the effect within the operation, from a closed list
     per op: `assign` has `work_assign` and `queue_item`; `land` has `merge`,
     `batch_branch`, `batch_pr`, `ci_rerun:<n>`, `update_branch:<n>` and
     `member_close:<pr>`; `release` has `dispatch`; every other op has
     `main`.
   - `effect_id = H(effect_key)`.
   - `payload_hash = H(canonical arguments)`: target, body, expected
     revisions and the subject revision observed at intent. The subject
     revision belongs to the payload, not the key, because effects such as
     update-branch move it.
   - Replay and recovery read the persisted key; they never derive it again.
     The same `effect_id` with a different `payload_hash` refuses
     (`effect_payload_mismatch`) and blocks the step. Changed inputs make a
     new attempt, hence a new key, and only after every effect of the earlier
     attempt is resolved (rule 4).
2. **Act** through the operation's server owner, passing `effect_id` where
   the target can carry it and the guard preconditions where the target can
   enforce them (for example merge only at an expected head).
3. **Observe.** Commit `run.effect_observed { effect_id, outcome: applied |
   refused | uncertain, evidence }`.
4. **Reconcile, then maybe retry.** A crash, timeout or lost response between
   1 and 3 leaves the effect `uncertain`. Recovery queries the subject by the
   effect's identity.
   - *Proven applied* records `applied`.
   - A retry under the **same** `effect_id` is allowed only if the effect is
     **guarded**: the target enforces a precondition or a uniqueness key so
     that the earlier request, should it still complete late, and the retry
     cannot both apply (a merge pinned to an expected head, a queue item ID,
     a compare-and-set on a record revision).
   - An empty query result is **not** proof of absence. A request still in
     flight can complete after the query, and a searchable marker is not a
     uniqueness constraint. Without a guard the effect stays `uncertain`.
   - Every remaining case becomes a `request-decision` to the operator, which
     is ADR-0045 D3's "actions outside flk still need idempotency keys or
     review" and ADR-0044 Q3's "ambiguous writes become held with a typed
     reason". This `request-decision` (class `effect_uncertain`) is the step
     form of W4's `run.resolve_effect`: answering it is a call of that verb,
     with the same resolutions, authority and audit, and either form closes
     the other. Only that decision may close the effect without proof.
   - No retry ever mints a new identity while an earlier attempt is
     unresolved.

### Operations

| Op | Server owner | Authority check | Durable transitions | Identity and retry | Recovery after crash, restart or handoff | External-effect safety |
| --- | --- | --- | --- | --- | --- | --- |
| **assign** | work service (ADR-0045 D1/D2) and the target's queue (ADR-0036 §3) | the run principal exercises the launcher's assignment authority over the WorkId (the launcher stays the supervisor and the approver, W4); target is a live agent the run spawned or the launcher's attested delegate (ADR-0027 §1) | two ordered sub-effects that are not one transaction: `work_assign` (`work.updated`, assignment generation +1 at an expected revision, the record carrying `assignment_effect = effect_id`), then `queue_item` (queue `message` item bound to that generation) | `work_assign` is a compare-and-set on the expected revision; `queue_item`'s ItemId = its `effect_id`, same ID and body observes the prior result, different body refuses (ADR-0045 "Durable subscription and delivery protocol", step 4) | crash between the two commits: read the WorkId. Current generation with `assignment_effect` equal to this `work_assign` ID → applied (an already-applied assignment revision is recognised, never re-applied); still at the expected revision → absent and guarded, retry the same ID; moved on → search the WorkId's assignment history (the retained `work.updated` events for that WorkId): this ID at an earlier generation → applied and since superseded; absent from a complete history → refused, the dispatcher re-plans; history rotated out so neither is provable → `uncertain`, operator decision. The `queue_item` lookup by ItemId always runs next: a current generation admits that same ID if missing, never a replacement; for a superseded generation an existing item stays and is fenced at delivery (ADR-0045 step 6), and a missing one is recorded `refused`, never admitted. A committed item is not re-sent to stdin; an unhandled one stays owed and escalates at its deadline (ADR-0045 D4) | internal only |
| **spawn** | server-side start composition (moves ADR-0027 §6's client-side `delegate start` into the server; a prerequisite slice) | effective capabilities (W1/W5), host cap (#822), depth (ADR-0014 §5), `blocked_while` gates | intended → spawn record (ADR-0036 "Uncertain spawn records") → edge recorded (ADR-0027 §1) → step `in_flight` | spawn request ID = `effect_id`; agent name derived from run and step | look up the spawn record by `effect_id`; an uncertain spawn reconciles per that amendment; never a second agent for one ID | the worktree is a recorded cleanup obligation, released by ADR-0027 §5 reap on merge or by an explicit run cleanup |
| **await** | ADR-0045 loop service (LoopId); the run dispatcher is the wake recipient | source-read grants at registration and every firing (ADR-0045 "Subscription authority") | loop registered with a source cut; occurrence → step `ready`; healthy waits per ADR-0045 D4 | the occurrence key (same protocol, step 2) | the loop survives; the run refolds and consumes owed occurrences once | none; deadlines and the D8 chain apply |
| **collect-evidence** | `work.evidence.attach` and the D11 check runner | assigned agent or run on its behalf; `check.run` for checks | evidence committed with typed identity (receipt/revision/hash, CI run/attempt, check invocation) | dedupe by evidence identity | re-verify; a claim (`DONE:` line, link) stays a claim until verified (ADR-0045 "Record and authority model") | reads only; checks sandboxed |
| **request-review** | review service (ADR-0035 §6.4) plus spawn | reviewer type grant; independence: not the builder and not anyone who steered it this round | spawn reviewer with edge label `review:<pr>` → `ReviewRecorded { head_sha, verdict }` → step done | one fresh reviewer per round; `effect_id` per round | reviewer gone without a record → failed attempt, next attempt is a new reviewer; a moved head invalidates the verdict | a `VERDICT:` line is a claim; only `ReviewRecorded` at the current head counts |
| **request-decision** | S6e question service | a delegable class needs operator `answer`/`operate` or a decide delegation for that class (W5); a non-delegable class needs `operate` only (W5 "Never delegable", `decision_requires_operate`) | `QuestionId` committed; answer custody per S6e; step resumes on commit | `QuestionId` derived from `effect_id` | the question persists; ADR-0045 D8 escalation and reminders apply; no default answer on timeout (S6e Q3) | none |
| **land** | the landing worker (`flk land`, #694) holding ADR-0035 §6.3 layer one and `flk/land-gate` | binding merge policy (ADR-0035 §6.6); `ReviewRecorded` approve at the current head; green CI at that head by run/attempt (ADR-0045 PR/CI); base not red | one in-flight landing per (repo, base), a slot a `release` can also hold (below): intended → merge or batch PR → observed merged | one key per sub-effect (`merge`, `batch_branch`, `batch_pr`, `ci_rerun:<n>`, `update_branch:<n>`, `member_close:<pr>`); merge guarded by expected head (`--match-head-commit`); batch branch and PR named from the `batch_branch` `effect_id` | `merge`: merged at head → applied; open at the same head → guarded retry; head moved → refuse, back to review. `batch_pr` found by branch → reuse, never a second. `ci_rerun:<n>`: the CI attempt number at the head proves it; no proof → uncertain. `update_branch:<n>` is guarded by its expected head. `member_close:<pr>` is idempotent on an already closed PR | no duplicate merge (guard); each rerun, update-branch and member close is its own effect with its own key, so they never collide on shared inputs |
| **release** | release dispatcher, through a repository adapter contract that does not exist yet (see "Release adapter prerequisite" below) | a non-delegable operator `request-decision` immediately before, bound to the exact commit (ADR-0035 §6.6 release `ask`), decided only under `operate` (W5 "Never delegable") | acquire the (repo, `dev`) landing slot → decision → intended → dispatch → observed run → release PR landed with parent equal to the approved commit → tag observed → release the slot | dispatch inputs carry the approved commit and `effect_id`; release version fixed at decision time | find the workflow run by `effect_id`, the tag by version; uncertain → operator decision; **never auto-retried** | live only after the adapter prerequisite lands; until then shadow only. The rollout to hosts is out of scope; a follow-up `file-issue` records it |
| **file-issue** | GitHub write adapter (ADR-0035 §6.3 agent write identity and its O4 verbs; ADR-0045 D10's credential is read-only) | `issue.write` grant for the target repo; search first (ADR-0033 §6) | intended → created → observed number | hidden body marker with `effect_id`, used for detection only; GitHub offers no uniqueness constraint on it | search by marker: one found → applied; several → applied, extras reported in the decision; none found → stays `uncertain` (a late create can still land) and becomes an operator `request-decision`, never an automatic retry | creation is unguarded, so duplicates are prevented only by not retrying unproven creates; the marker makes any duplicate detectable |

Notes on the table:

- **Landing makes the scripts first-class.** `.fleet/` behaviours map to typed
  facts: a moved head is a guard failure; "best outcome per check name" is
  ADR-0045's run/attempt identity; `RED` is a run in `blocked` with a base-red
  observation that `blocked_while = ["base_red"]` reads; the single automatic
  rerun is the definition's `ci.rerun_failed = 1`, recorded as an attempt;
  `BEHIND` is an update-branch effect; deferred stacked members are typed
  `deferred { reason: conflict | parent_deferred }` outputs; worktree cleanup
  is an obligation, not a shell `trap`.
- **Release adapter prerequisite
  ([#2909](https://github.com/gerchowl/flock/issues/2909)).** Today
  `.github/workflows/promote.yml` takes one `workflow_dispatch` input,
  `version`, checks out the moving `dev` branch, commits `release: vX` on a
  `release/vX` branch and queues `gh pr merge --squash --auto` into `dev`;
  `release-follow-up.yml` then fast-forwards `main` to whatever squash
  commit landed. So a dispatch is not bound to the commit the operator
  approved, carries no `effect_id` a reconcile query could find, and has no
  duplicate handling beyond the `promote` concurrency group (which
  serialises, not dedupes) and the "tag already exists" check. Binding the
  commit at dispatch is not enough: while the release PR waits on checks, a
  `land` can merge Y onto `dev`, the squash lands on top of Y, and Y ships
  unapproved. The `release` op cannot run live until all of these hold:
  1. **Commit-bound dispatch:** a required full-SHA `commit` input X; the
     workflow checks out exactly X and refuses unless `dev` still points at
     it.
  2. **Release lands only at `dev == X`:** the release PR merges only while
     its base is X (a merge guarded on the base, or a fast-forward of `dev`
     to the release commit), or `release-follow-up.yml` refuses to move
     `main` unless the landed release commit's parent is X and its tree is
     the prepared one. Either form refuses a release that would include
     anything after X. **On refusal** the `dispatch` effect is observed
     `refused`, the step fails and the run ends `failed` (terminal), and the
     slot is released. With the merge guard, nothing lands on `dev`: the
     release PR and `release/vX` stay open and the run's record names them.
     With the follow-up refusal, `dev` carries an untagged `release: vX`
     commit and `main` has not moved. In both cases vX is not tagged, so it
     is cut again by a new run with a new operator decision bound to the
     then-current `dev` head; the operator first closes a leftover release
     PR, and the adapter must accept re-cutting vX on a `dev` that already
     carries an untagged `release: vX` commit (#2909).
  3. **The `release` op holds the (repo, `dev`) landing slot** from before
     the operator decision is raised until the tag is observed (or the effect is resolved
     by the operator), so no flk `land` is admitted onto `dev` meanwhile;
     waiting `land` steps stay `waiting` with the slot holder named. A
     held slot has an ADR-0045 D4 deadline and D8 escalation, never a
     silent timeout. Item 3 stops flk's own landings; item 2 is still
     required for merges made outside flk.
  4. **`effect_id` correlation:** a required `effect_id` input, surfaced in
     the run name, so reconcile finds the run by `effect_id`.
  5. **Duplicate-dispatch handling:** a second dispatch with the same
     `effect_id`, or for a version whose release branch or tag already
     exists, exits without effect and reports the earlier run. Today
     `promote.yml` instead reuses an existing release PR (`gh pr view`
     finds it) and force-pushes `release/vX`, so a second dispatch silently
     replaces the first one's commit under the same PR. The adapter must
     drop that path: an existing `release/vX` branch or open release PR,
     whatever its `effect_id`, refuses the dispatch instead of being
     overwritten or reused.
  "Never auto-retry" remains, but on its own it cannot prevent releasing a
  different commit than the one approved; items 1–3 together do.
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
  principal, run principal, run ceiling (W5), mode (`live` or `shadow`), state, a step
  instance table, linked WorkIds (a run that creates work creates ADR-0045
  work nodes, it does not shadow them) and pending decisions. Run states:
  `admitted`, `running`, `waiting`, `blocked`, `uncertain`,
  `awaiting_approval`, `succeeded`, `failed`, `cancelled`. Step instance
  states: `pending`, `ready`, `in_flight`, `waiting`, `succeeded`, `failed`,
  `handled` (failed, and its failure handler, W2 rule 1, was admitted),
  `uncertain`, `skipped`, `cancelled`; each instance has a server-minted
  `step_instance_id` (W3 rule 1) and carries attempts, and each attempt its
  effects.
- **Two principals.** The **launcher principal** is whoever launched the
  run (the operator, an architect or a resident owner). The **run
  principal** is the server-side identity `Run(RunId)` under which the
  dispatcher acts; it is minted with the run, is not an agent, holds no
  credential outside the server and is never the launcher. The run principal
  exercises only the authority W5 gives the run (ceiling and effects).
- **Success needs approved Work, not finished steps.** A run whose every
  step instance is terminal and none is `failed` (`handled` and `skipped`
  do not count as failed) enters `awaiting_approval`, not `succeeded`,
  except a run that ended through an exhaustion step (W2 rule 2a), which
  ends `failed` with its decision as output. A `failed` instance without an
  admitted handler makes the run `failed`. Finishing steps never implies
  verified completion. A run
  reaches `succeeded` only when each WorkId it links as a deliverable has an
  approved completion under ADR-0045 D2 (`work.complete.approve` naming the
  frozen check revision and the verified evidence, by the supervisor or, for
  a root, the operator).
- **Who approves: ADR-0045 D2, unchanged.** D2 says the assigned agent
  proposes completion and "its supervisor/operator approves"; root
  completion stays operator-approved. This ADR adds no approver and removes
  none, so it does **not** amend D2. Concretely: the supervisor of that Work,
  including a launching architect, may approve; the operator may approve.
  The run principal never approves any Work (it is not a supervisor in D2's
  sense; it only exercises the launcher's assignment authority, W3
  `assign`). No agent that worked on that Work may approve it: every
  assignee in its assignment history, every agent the run or anyone else
  assigned or spawned for it, and the agent that proposed its completion;
  this exclusion applies to a supervisor too, if it worked on that Work.
  Such a call is refused with `self_approval`.
- **Runs with no deliverable WorkId** (for example a release run) do not
  skip approval. All steps terminal moves them to `awaiting_approval` with
  the observed evidence (for a release: the dispatch run, the landed release
  commit and its parent, the tag). The operator closes them with
  `run.close { run, expected_run_revision, verdict: accept | reject, note }`
  (`operate`, not delegable, refused for attested agent callers, exit 13):
  `accept` → `succeeded`, `reject` → `failed`. Nothing else makes such a run
  `succeeded`. Approval later invalidated by a new acceptance
  revision (ADR-0045 D2) moves a `succeeded` run back to
  `awaiting_approval`.
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
  transfer as uncertain, with their persisted effect keys and payload hashes,
  and reconcile on the target before the run resumes.
- **Resolving retained uncertainty.** An effect that is still `uncertain`
  after handoff (the target cannot prove it either way, or lacks the read
  credential the query needs) keeps its run in `uncertain`; the run admits
  no new step on that subject. It resumes in exactly two ways: a later
  reconcile query proves the outcome under the same effect key, or the
  operator resolves it with `run.resolve_effect { effect_id,
  expected_run_revision, resolution: applied | absent | abandon_and_retry |
  abandon_and_fail, note }`.
  That verb is operator authority (`operate`), not delegable (W5), refused
  for attested agent callers (exit 13), and audited with what was known when
  it was resolved. Resolutions: `applied` records it; `absent` permits one
  retry under the same `effect_id`; `abandon_and_retry` closes the effect as
  abandoned, fails that **attempt** (not the step) and starts a new attempt
  with a new key if the step has attempts left, otherwise the step fails;
  `abandon_and_fail` closes the effect and fails the step. A failed step
  fails the run unless a failure handler reads it (W2 rule 1), which records
  it `handled`; run `failed` is terminal: nothing reopens it, and continuing
  means a new launch.
  `abandon_and_retry` is refused for `release` (a new release is a new run
  with a new decision). No timer, epoch change or restart resolves
  uncertainty on its own.
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
  A `foreign_effect` block is lifted only by the operator through
  `run.resolve_foreign { run, effect_ref, expected_run_revision, resolution:
  accept | cancel }`, with the same authority, refusal and audit as
  `run.resolve_effect`. `accept` records the foreign effect as the step's
  outcome only when it matches the step's intended payload (for a merge:
  that PR, merged at the head the step approved); a mismatch refuses
  `accept` and leaves only `cancel`. The run never re-acts on that subject
  in either case, and every later step re-reads it.

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
            # ParentArchitect resolves when the item is raised
            # ("Addressee assignment" below)
  scope:   { repo, subtree: attested ADR-0027 edges of the delegate, depth <= 2 },
  classes: [ # permission classes
              worktree_tool | plan_approval | form_elicitation
            | widen_directory | widen_network | widen_sandbox
              # request-decision classes
            | scope ],
  decisions: [allow_once, deny],          # permission classes; allow_always
                                          # is never delegable. A decision
                                          # class answers with the item's
                                          # declared options
  expires_at | standing,                  # ad hoc: finite, default 8 h, at most 7 d
                                          # standing: adopted defaults only (owner Q1/Q2)
  by, created_at }
```

- **One resolver.** `decide_authority(principal, request) → Operator |
  Delegated(id, revision) | Refused(reason)` is called by `hook.decide`
  (PTY, ADR-0034 §1) and `agent.decide` (structured, ADR-0036 §10b) alike.
  The structured `Principal` gains a delegated variant: the architect's
  verified agent identity (ADR-0036 §1a) **plus** being the item's stored
  addressee under a still-valid delegation ("Addressee assignment"),
  rechecked at the mutation fence. An MCP lease's scopes alone still
  authorise nothing (S6e).
- **Binding state, operator verbs.** `flk authority delegate|revoke|show`;
  durable `DelegationSet` and `DelegationRevoked` events; refused for an
  attested agent caller (exit 13); loud like ADR-0035 §6.2 (notification,
  channel, standup). Revocation takes effect at the next decision, and
  every pending item addressed under the revoked delegation reroutes to the
  operator ("Authority loss"); a decision already committed stands.
- **Audit.** Every permission close records `principal`, `delegation_id` and
  `delegation_revision` beside the existing S6 audit fields, so "who allowed
  this" always names a human grant.
- **Never delegable**: trust dialogs and login (ADR-0034 §6 rows 5–6),
  `allow_always` session grants (S6), preset escalation (ADR-0036 §10b),
  question-policy widening (ADR-0034 §2), model-tier changes (ADR-0021 §2),
  resume (ADR-0036 §1a), the merge, release and protection gates
  (ADR-0035 §6.6), and resolving run uncertainty (`run.resolve_effect`,
  `run.resolve_foreign`, W4). A `request-decision` step declares its
  class. The delegable decision class is `scope` (for example W2's
  `decide-split`). The non-delegable decision classes are `release`,
  `deploy`, `outside_merge` and `effect_uncertain` (W3 rule 4); no
  `DecideDelegation` may list them, and adding a class needs an amendment
  here. **Every non-delegable decision class can only be decided under the
  `operate` scope** (or the attested local operator CLI/TUI, which holds
  it). This narrows S6e's question-answer row for these classes: an
  `answer`-scoped client token or an operator-bound channel's `reply`
  scope, which suffice for other questions, is refused with
  `decision_requires_operate` and audited, and the item stays pending.
- **Every delegation is explicit and operator-issued; defaults are a
  template, never live authority.** The node-scoped operator setting
  `authority.delegation.defaults` (ADR-0042 registry, `G/N` only, so a repo
  file cannot set it) holds a proposed row: `ParentArchitect` for
  `worktree_tool`, `plan_approval` and `form_elicitation`, `allow_once` and
  `deny`, which is ADR-0034 §6 rows 1–3. The widening classes start empty,
  as `architect_may_grant` did. The template authorises nothing by itself.
  The operator turns it into binding state once, with `flk authority adopt
  defaults`, which writes `DelegationSet` records (grantor, revision) and a `DelegationDefaultsAdopted { template_revision,
  delegation_ids, by }` marker. From then on:
  - a config reload, restart, upgrade or template edit never writes, renews
    or re-creates a delegation; only an operator verb does;
  - a revoked or expired delegation stays revoked or expired, even when the
    template still lists its row; recreating it is a new `flk authority
    delegate` with a new id;
  - renewal of a finite delegation is the operator verb `flk authority
    renew <id>`, never automatic.
  Finite expiry and these defaults conflict: an adopted default that expired
  after at most 7 days without renewal would put every tree back to
  operator-only, the alternative this W5 rejects. Recommended (owner
  questions 1 and 2): adopted defaults are **standing** grants, revocable at
  any time but not expiring, and loud in `flk authority show` and the
  standup; ad-hoc delegations, including every widening class, stay finite.
  Whether the template is adopted at rollout or left unadopted is owner
  question 1; in every case it reaches binding state only through the
  operator's adopt step, never on first use.
- **Owner-typed decision (DECIDED).** Architects answer delegates'
  permission prompts **only** through a `DecideDelegation`. No edge, role,
  config key or run confers that authority by itself.
- **`architect_may_grant` is withdrawn** before it ships (#1184 is open). Its
  meaning moves into delegation classes; #1184 implements against this
  resolver instead of a config key.

### Inheritance and ceilings

- A delegation is bound to `(AgentId, generation)` or resolved through an
  attested role when an item is raised ("Addressee assignment"). It is **never inherited**: not by a fork
  (ADR-0036 §9 creates a new AgentId; lineage grants nothing, as S6e says for
  transcripts), not by a spawned child, a reviewer or a run.
- Capabilities only shrink down the tree: `effective(child) = requested(type)
  ∩ granted(type) ∩ effective(parent) ∩ ceiling(run)`. Permission preset rank
  is at most the parent's (ADR-0036 §10b rank). Question policy only narrows
  (ADR-0034 §2). Read-only forks pass a tool subset of the parent (ADR-0036 §9).
- **Widening decisions stay inside the child's and run's ceilings.** A
  delegated `allow_once` for `widen_directory`, `widen_network` or
  `widen_sandbox` resolves to `Delegated` only when the requested widening
  lies within the asking child's ceiling (its type grant's preset and
  capabilities, `effective(parent)`) and within `ceiling(run)` when the child
  belongs to a run. A request beyond either ceiling is outside every
  delegation: the resolver returns `Operator` (the prompt goes to the
  operator), never `Delegated`. Above-ceiling widening is preset escalation,
  which is never delegable (above). The audit records which ceiling bounded
  the decision.
- **Run ceiling.** The operator grants a workflow per repo:
  `WorkflowGrant { repo, def_hash, revision, capabilities, effects: [land,
  file-issue, …] }`. A run launched by the operator gets that ceiling. A run
  launched by an architect or owner gets `capabilities ∩
  effective(launcher)` for its agents, and needs a `workflow.launch` grant
  for that `def_hash`.
- **Workflow effects have their own authority, separate from agent
  capabilities.** `merge` (the `land` op), `release` and `file-issue` are not
  agent capabilities (W1), so intersecting with the launcher's capabilities
  would always yield nothing. Effects are authorised by binding state only:
  `effects(run) = WorkflowGrant.effects ∩ launch_effects`, where
  `launch_effects` is everything in `WorkflowGrant.effects` for an operator
  launch, the `effects` listed on the operator-issued `workflow.launch`
  grant for an architect launch, and the owner's ADR-0035 §6.6 gate rows for
  an owner launch. Each effect is checked again at its mutation fence
  against current state (next bullet). `land` additionally needs the binding
  merge policy (ADR-0035 §6.6) at that moment; `release` additionally needs
  its per-instance, non-delegable operator decision bound to the commit
  (W3), whatever the grants say. No agent, decide delegation or capability
  can supply a missing effect.
- **Pinned revisions and current revocation.** A run pins its grant
  revisions (W2 rule 5) as an upper bound. Every authority check (spawn,
  effect, decide) uses `pinned ∩ current`: a revocation, narrowing or expiry
  after launch takes effect at the run's next check, at the mutation fence;
  a widening after launch does not reach the running run, which needs a new
  launch to use it. A decision under a delegation names `(id, revision)` and
  is valid only while that delegation is live (unrevoked, unexpired) and no
  revision since the named one has narrowed it; a renewal or widening
  revision keeps it valid, a narrowing one reroutes (`narrowed`). Runs never hold decide
  authority; a permission prompt inside a run-spawned agent goes to the same
  resolver as any other, along the spawned agent's edge: the launching
  architect's edge, or, for an operator launch, `ask_operator` as a
  server-started root (ADR-0034 §2).
- **Resident owner.** An owner's limits are already binding state (ADR-0035
  §6.2); its delegations are just more of it, and a shadow owner holds none.

### Addressee assignment

`decide_authority` is checked when someone decides; the addressee is fixed
when the item is raised. ADR-0048's inbox renders this stored value.

- **Stored at raise.** When a permission prompt or a `request-decision`
  item is raised, the server computes its addressee once and stores
  `addressee: Operator | Delegated { delegate, delegation_id,
  delegation_revision }` in the same acknowledged write that raises the
  item. `delegate` is a concrete `Agent(AgentId, generation)` or
  `Owner(repo)`, never the unresolved `ParentArchitect`.
- **`ParentArchitect` at raise time.** It resolves to the asker's current
  attested ADR-0027 parent, as `(AgentId, generation)` at that moment,
  provided that parent is live and holds an architect role. With no such
  parent the delegation does not match.
- **Candidates.** A delegation matches when it is live, lists the item's
  class (and, for a permission, the decision kinds the prompt offers), its
  scope covers the asker, the request is within the ceilings of
  "Inheritance and ceilings", and its delegate resolves to a live
  principal. No match → `Operator`.
- **One winner, deterministically.** Among matches: (1) the delegate
  nearest the asker on the attested edge chain (parent before
  grandparent; `Owner(repo)` ranks after every edge delegate); (2) then
  `Agent(...)` before `ParentArchitect` before `Owner(repo)` delegations;
  (3) then the lowest `delegation_id`. The rule uses only stored state, so
  replay after restart picks the same winner.
- **Only the addressee decides.** At decision time `decide_authority`
  accepts the operator or the stored addressee (that identity and
  generation, under the stored delegation still valid per "Pinned revisions
  and current revocation"). Any other principal, including another agent
  that holds a matching delegation, is refused `not_addressee`; the stored
  delegate after its delegation lapsed is refused `delegation_lapsed`. An
  operator answer to a delegated item is allowed and audited as the
  operator's.
- **Changes reroute, never re-resolve.** A change to the asker's parent
  edge (`orphaned`), to the delegate's generation (`generation_changed`), to
  the stored delegation (`expired`, `revoked`, `narrowed`) or a handoff that
  does not carry the delegate (`handoff`) produces a `PermissionRerouted`
  with that reason (below). The item goes to the operator; it is never
  reassigned to another matching delegate.

### Authority loss: expiry, revocation and orphaning

W5 owns this transition; ADR-0048's inbox references it rather than
defining its own. The record name `PermissionRerouted` and its reason
values `expired`, `revoked`, `narrowed`, `orphaned`, `generation_changed`
and `handoff` are stable names that other ADRs cite; changing them needs an
amendment here. A pending permission prompt or `request-decision`
question addressed to a delegate (architect or owner) is **rerouted to the
operator by the server** when the delegate's authority ends before it
answers:

- its delegation expires, is revoked, or is narrowed so it no longer
  covers the prompt's class or ceiling;
- the delegate is orphaned or replaced: it stops or is gone, its generation
  changes (restart, resume, fork), or the asker's ADR-0027 edge to it is
  broken;
- the delegate's host hands the run or agent off and the delegate is not
  carried with it.

The server commits `PermissionRerouted { request_id, from: (principal,
delegation_id, revision), reason: expired | revoked | narrowed | orphaned |
generation_changed | handoff, to: operator, at }` in the same acknowledged
write that changes the prompt's addressee, then the prompt appears in the
operator's inbox as an operator item, with ADR-0045 D8 escalation from that
moment. The prompt is never auto-answered, never defaulted and never
dropped. An answer from the former delegate that arrives after the reroute
is refused at the mutation fence (`delegation_lapsed`) and audited. A
reroute is not reversed when a new delegation appears later; the operator
answers it. Whether the operator may hand a rerouted prompt back to a new
delegate is open (owner question 6).

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
| **Duplicate external effects**: a lost response produced two merges, two issues or two releases | effect intent before call, a persisted effect key per sub-effect with a payload hash, guarded merge, no retry of an unguarded effect without proof or an operator decision (issues), commit-bound and `effect_id`-correlated release dispatch, release never auto-retried. |
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

- **P0: none.** ADR-0048 renders the Run view and the workflow-definition
  facet in its GUI-P2 phase; nothing here waits on GUI-P0 or GUI-P1. The
  runtime depends on ADR-0045 slices 1–12 (acknowledged writes through
  the CI adapter).
- **P1: read-only run view of existing loops.** Project ADR-0045 loops,
  structured delegate edges (never PTY agents, W1) and queued start intents (ADR-0027 §6), and armed goals as
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
| W3 | Inject a crash after `effect_intended` and before the call, and after the call before `effect_observed`, for land, file-issue and release against a fake GitHub: exactly one merge and at most one dispatch; an unproven issue create and an uncertain dispatch each raise an operator decision rather than retrying, and a fake that completes the first create after an empty search still yields one issue. Two fan-out instances, two review rounds, and a rerun, an update-branch and a member close at the same head and attempt each get distinct effect keys. The same `effect_id` with a different payload refuses `effect_payload_mismatch`. A crash between the `assign` commits recovers an already-applied assignment revision without a second generation or a second queue item. A moved head refuses landing. A rerun failure at the same SHA is observed. With the release adapter absent, `release` refuses live mode. While a `release` holds the (repo, `dev`) slot, a `land` onto `dev` waits; a fake merge onto `dev` made outside flk while the release PR waits is refused in both item 2 forms: with the merge guard the release PR does not merge and stays open; with the follow-up refusal `main` does not move and `dev` keeps an untagged release commit; either way the run ends `failed`, the slot is released, and a new run can cut vX again. The slot is held before the release decision is raised. A second dispatch while `release/vX` exists refuses instead of force-pushing it. |
| W4 | A step that fails with a failure handler (`step.<id>.outcome == failed`) is recorded `handled` and the run still reaches `awaiting_approval`; the same failure without a handler makes the run `failed`; a handler that does not `needs` the failed step refuses validation. The launching architect, as supervisor, may approve a run's Work; the run principal, the Work's assignee and the agent that proposed completion each get `self_approval`. Restart between every pair of ledger events: the refolded run has the same step states, and uncertain effects reconcile before any new step is admitted. An effect still uncertain after handoff keeps the run `uncertain` until a proof or `run.resolve_effect`, which an attested agent gets exit 13 for. A shadow run issues no effect verb (`run_in_shadow`) and reports divergence against a scripted fake driver. A live run observing a foreign merge blocks with `foreign_effect`, and `accept` refuses a foreign merge at a different head. A run whose steps all finish sits in `awaiting_approval` until `work.complete.approve`, which refuses `self_approval` from the run principal or any agent that worked on that Work; a run with no deliverable WorkId reaches `succeeded` only through `run.close accept`. `abandon_and_retry` starts a new attempt key and `abandon_and_fail` leaves the run terminal `failed`. A review group whose last round has blockers skips that round's fix. A crash between the `assign` commits after the generation moved on proves the earlier assignment from history. |
| W5 | The same in-worktree prompt, on a legacy PTY and a structured fake delegate, gets the same answer path. Without an adopted delegation, the architect's decide refuses on both; editing or reloading the defaults template creates none, and a revoked adopted delegation stays revoked across reload and restart. A widening prompt is architect-answerable only with a matching live delegation and within the child's and run's ceilings; beyond either it goes to the operator. It refuses after expiry or revocation, and the audit names the delegation. A run pinned to a grant revision loses an effect revoked after launch. An architect-launched run without `land` on its `workflow.launch` grant cannot merge. Expiring, revoking or stopping the architect while a prompt is pending reroutes it to the operator with `PermissionRerouted`, and the old architect's late answer refuses `delegation_lapsed`; the same reroute happens when the delegation is narrowed below the prompt's class, when the architect's generation changes (restart or resume), and when the architect is not carried by a handoff. Addressee assignment: a prompt raised with two matching delegations stores the nearer delegate, and restart keeps the same addressee; another agent holding a matching delegation is refused `not_addressee`; `ParentArchitect` stores the concrete parent and generation at raise; reparenting the asker reroutes with `orphaned`; a renewal revision does not reroute. A `scope` decision is answerable under a `scope` delegation; a delegation listing `effect_uncertain` or `release` refuses creation. An `answer`-scoped reply, and a channel `reply`, to a `release` decision are refused `decision_requires_operate` and the decision stays pending; the same reply under `operate` is accepted. A fork of the architect cannot decide. A run-spawned child cannot exceed its launcher's preset rank. No run view or count includes a PTY agent. |
| W6 | The validator refuses a workflow carrying a registry-owned duration. The skill lint flags a landing command in a role text whose workflow is live. |
| W7 | Every pre-mortem row above maps to at least one W1–W6 test. |

## Owner questions

These stay open after acceptance (2026-11-03): the owner asked for more
detail before deciding them.

Recommendations below are recommendations, not decisions. Where the
independent review (a reviewer, fix round 1) recommends differently from this
ADR's author, both are given.

1. **Default delegation row (W5).** Should the operator adopt the
   `ParentArchitect` template row for `worktree_tool`, `plan_approval` and
   `form_elicitation` at rollout, preserving ADR-0034 §6, or leave it
   unadopted, making every prompt operator-only until the operator
   delegates? Either way only the operator's adopt step creates it.
   *Author recommendation:* adopt at rollout. *Reviewer recommendation:*
   default delegation off.
2. **Delegation expiry (W5).** Ad-hoc delegations: default 8 hours, at most
   7 days, or no expiry with revocation only? Adopted defaults: standing
   (revocable, not expiring) or finite like ad-hoc grants?
   *Recommendation (author and reviewer):* ad-hoc expiry 8 h by default, at
   most 7 d. *Recommendation (author, fix round 2):* adopted defaults
   standing, because finite defaults without auto-renew lapse into
   operator-only, which W5 rejects; auto-renew is ruled out because a reload
   must never recreate authority.
3. **Checkpoint budget (W4).** Runs share ADR-0045's 8 MiB aggregate budget,
   or reserve a separate 4 MiB? *Recommendation (author and reviewer):* one
   shared 8 MiB budget; the reviewer adds that it must hold recovery
   reserves, so uncertain effects, reroutes and escalations can always be
   recorded when the budget is otherwise full (ADR-0045's reserved
   control-state space).
4. **Launch authority (W5).** May a resident owner or architect launch adopted
   workflows under a `workflow.launch` grant, or only the operator in P2?
   *Recommendation (author and reviewer):* allow it, with the grant specific
   to one `def_hash`.
5. **First workflows (W8).** `land-issue` and `release` in shadow first, or
   `land-issue` alone? *Recommendation (author and reviewer):* shadow both
   landing and release (release stays shadow-only until the W3 adapter
   prerequisite lands).
6. **Handing a rerouted prompt back (W5 "Authority loss").** After a prompt
   is rerouted to the operator, may the operator hand it to a new delegate,
   or must the operator answer it? Not covered by the review; this ADR
   assumes the operator answers it until decided.

## Decided

- **Architects answer delegate permissions only through `DecideDelegation`**
  (owner-typed, 2026-11-03). No edge, role, config key or run confers
  permission-decide authority by itself (W5).

## Boundaries

- Deploys, host applies and server lifecycle remain outside the vocabulary
  (ADR-0035 §6.6). Rollout after a release stays an operator task, recorded by
  `file-issue`.
- Cross-host runs and remote spawn follow ADR-0045 D5 and #1380; nothing here
  adds a mesh command.
- Workflow-level cost budgets and provider-limit handling reuse ADR-0035 §6.5;
  this ADR adds no budget mechanism.
- Workflow definitions (W2 "Workflow definition identity"), agent types
  and runs are defined by this ADR. ADR-0048 defines only how the GUI
  projects them as its Workflow and Run objects.
