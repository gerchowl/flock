# ADR 0027 — An architect's delegates are a server-held edge, and everything else is a projection of it

- Status: Proposed
- Date: 2026-10-11
- Issues: #632 (the epic this splits). Composes with #573 (goal mode), #663
  (finished/stalled events, PR + CI join), #694 (landing queue), #697 (turn
  state vs background activity), #805 (entity model), #815 (sidebar tree),
  #822 (delegate budget), #395 / ADR-0023 (decisions). Field evidence: #819.
- Decision owner: operator.
- Implemented: none. Slice issues are listed in §9.

## Context

An architect is an agent that hands whole tasks to other agents (`flk delegate
start`, `flk agent start`, `flock_agent_fork`), reviews what comes back, lands
it and tears the worktrees down. flk has no notion of "the agents *I* started".
Every architect run therefore builds that layer itself. The reference build is
the flock fleet's own `.fleet/` directory, and #819 found the same shape in
about a dozen other conductor runs:

| Hand-rolled piece | What it does | What flk already half-knows |
| --- | --- | --- |
| `agents.json` | name, space, pane, issue, PR, branch, worktree, goal, check, per delegate, kept by hand and patched by `spawn*.sh` | the agent record, its worktree and branch, `PrStateInfo` per branch |
| `supervisor.py` (90 s poll) | status transitions → `[fleet] …` lines typed into the architect's pane while idle, PR-open and CI-finished lines, reap on merge, per-delegate goal nudges, ctx scraping and `/compact` with a STATUS.md handshake | `PaneAgentStatusChanged` events, the msg inbox and idle wake, `branch_merge_gate`, `worktree.kill`, `flock_self_compact` |
| `tasks.json` + `tasks.py` | a gated checklist under a frozen goal, each box ticked by a shell gate | nothing (this is #573's subtask layer) |
| `landqueue.sh`, `batch.sh`, `landbatches.sh` | serial or batched landing, blocked list per owner | nothing (this is #694) |
| `gates/*.sh` | `merged`, `branch-landed`, `shipped`, `rolled` predicates | `branch_merge_gate` covers `merged` |

The hand-rolled layer works but drifts: the registry is keyed by names and
workspace ids copied by hand, the digest is typed into a TUI, the ctx figure is
scraped off a status line, and it all dies with the session that started the
poller. Its single most load-bearing fact is not any of the features. It is the
**edge**: "agent B was started by agent A, for this purpose". Everything else
in the table is derived from that edge plus state flk already holds.

What exists today (verified at `95201ffa`):

- **The caller is already attested.** `App::spawn_caller()`
  (`src/app/api/spawn.rs`) walks the socket peer's process ancestry to a pane
  and returns `SpawnCaller::Agent { pane_id, agent_id, .. }` or `Operator`.
  `msg.send` stamps its sender by the same walk, and `handle_agent_start`
  already resolves it (`src/app/api/agents.rs`). An agent cannot assert
  someone else's identity through it.
- **Lineage exists, but only for spawn and fork.** `agent.spawn` and
  `agent.fork` record `spawned_by` on the child terminal, and fork emits a
  durable `AgentForked` event that `agent.lineage` folds across restarts
  (`src/app/api/lineage.rs`). A plain `agent.start`, which is what `flk
  delegate start` drives, records no parent at all.
- **Delegate records are client-side.** `flk delegate` keeps one JSON file per
  delegate under `state_dir()/delegates/<socket hash>/` (`src/cli/delegate.rs`).
  The server never sees them. `status --json` already reserves `"goal": null`
  for #573.
- **The durable log is the audit substrate** (ADR-0005): `EventHub` persists
  `EventKind`s to `event-log.jsonl`, and lineage and notifications are both
  folds over it.
- **Server-side reap is possible.** `branch_merge_gate` (`src/worktree.rs`) is
  the squash-aware merged verdict, and `handle_worktree_kill` is the guarded
  teardown. Neither needs a client process.
- **PR state is polled in one batched call** (`src/pr_poll.rs`, keyed per
  repo and branch). It carries no CI conclusion yet (that is #663).
- **The post-compaction re-inject exists.** `self_compact_on_session_start`
  types the armed continuation when SessionStart reports `source=compact`.
- **No goal record exists.** #573 is unbuilt.
- **No server-authored agent message exists.** Every `PendingMessage` today
  comes from `msg.send`, `msg.reply`, mesh mail or log seeding.

## Decision

### 1. The delegation edge is recorded by the server, at start, from the attested caller

Whenever `agent.start`, `agent.spawn` or `agent.fork` succeeds and
`spawn_caller()` resolves to `SpawnCaller::Agent`, the server emits a durable
`AgentDelegated` event:

```text
AgentDelegated {
  architect: AgentId,        // attested, never taken from params
  delegate:  AgentId,
  via:       start | spawn | fork,
  workspace, worktree?, branch?, repo?,   // what the start produced
  label?:    String,         // free text from the caller, e.g. "#614"
  reissued:  bool,           // true when re-emitted, see below
}
```

- **The architect is never a parameter.** There is no `--architect` flag, for
  the same reason `msg.send` has no asserted sender: the edge drives teardown,
  so a spoofable edge is a way to get another agent's worktree reaped.
- **The operator is not an architect.** An `Operator` caller records nothing.
  So does a caller whose ancestry resolves to no pane, which includes `ssh
  host flk delegate start` from another machine (§8).
- **Keys are `AgentId`s**, not pane ids, which change across moves and
  restarts (#573's owner decision 2, #805). A delegate that is hibernated and
  resumed keeps its edge.
- **The live index is a fold.** On start the server folds `AgentDelegated`
  into an in-memory `architect → [edge]` index, exactly as lineage folds
  `AgentForked`. Log rotation could drop an old live edge, which lineage
  today simply accepts. An edge carries teardown authority (§5), so losing one
  silently is worse: the server re-emits every **live** edge with
  `reissued: true` once per server start. A live edge therefore always has a
  copy in a recent log file. An edge whose delegate is gone (`AgentRemoved`) is
  closed and is not re-issued.
- **The edge ends** when the delegate is removed or reaped (§5). It is never
  edited in place. Later facts (PR number, reap receipt, goal link) are their
  own events that reference the delegate's `AgentId`.

`flk delegate`'s client-side registry stays. It holds what only the CLI needs:
the brief, the turn cursor, the submit time. It is no longer the answer to
"which delegates are mine".

### 2. `delegate.list` is the one read, and every surface is a projection of it

A socket verb `delegate.list { architect: self | <AgentId> | all, branch?, repo? }`
returns edges joined, at read time and from cached state only, with:

- the delegate's live `agent_status`, harness, pane and workspace;
- its branch and the poller's cached `PrStateInfo` for it (#663 adds CI);
- the age of the edge and of the status.

`architect: self` resolves through `spawn_caller()`. Surfaces:

- `flk delegate list [--mine | --architect <id> | --all] [--json]`, grouped per
  architect, so `--all` shows the whole tree;
- MCP `flock_delegate_list`, read-only, so it fits ADR-0014's MCP boundary
  without a new authority;
- the `--branch` filter is the ownership lookup #694 needs to send a blocked PR
  to its owner;
- #815's sidebar tree renders this same list. It does no git or network I/O
  per row (#805 §5).

No new git or `gh` call happens inside `delegate.list`.

### 3. The architect is told by a digest message, not by polling and not by typing

The server sends the architect a **digest**: an ordinary inbox message
(ADR-0008, ADR-0018), delivered at a turn boundary and through idle wake. It
is never typed into a busy TUI. The digest is the first server-authored agent
message, so it gets a distinct sender: `from: flock`, rendered as such, with no
reply expected (intent `fyi`).

- **Triggers in slice 1:** a delegate's status settles into `idle`, `done`,
  `blocked` or `gone`, or comes back to `working` from one of those. A status
  must hold for a settle window (default 20 s, configurable) before it counts.
  This is the #819 false-idle filter (flapping Codex status, compaction,
  quota screens) applied once, in the server.
- **Coalescing:** at most one undelivered digest per architect. A new
  transition rewrites the pending digest's body rather than queuing a second
  message. One wake therefore carries one line per changed delegate, for
  example `bunny-614 idle (PR #624 open) · bunny-621 working · astra-623 gone`.
- **Later triggers come from other issues, not new plumbing:** #663's
  `agent.finished` / `agent.stalled` / `pr.ci_completed`, #820's
  `needs_human`, and the reap and queue receipts in §5 and §6. Each is one
  more line kind in the same digest.
- **Opt-out** per architect (`flk delegate digest off`), because an architect
  that runs its own `flk wait` loop does not want both.

### 4. After a compaction, the architect gets its delegate table back

`self_compact_on_session_start` already types a continuation after
`source=compact`. When the compacted agent has live edges, the continuation is
followed by the rendered `delegate.list --mine` table, bounded to a fixed
number of rows. That retires the hand-kept `agents.json` re-read and the
GOAL.md-plus-registry re-inject step of the supervisor. The frozen goal, the
mutable status and the pre-compact status handshake belong to #573's goal
record. When that record exists, its re-inject renders goal, checklist, status
and this table in that order (§7).

### 5. Lifecycle: done hibernates, merged reaps, and the server does both

Each edge carries a reap policy, set at start (`--reap-on merged|never`,
default `never` until slice 5 has run in the field):

- **`merged`:** when the PR poller reports the delegate's branch merged, the
  server re-checks with `branch_merge_gate` (squash-aware, #321 / #631), then
  runs the same guarded teardown as `worktree.kill`. The teardown refuses
  dirty, unpushed or advanced work, as it does today. On success it emits
  `DelegateReaped { delegate, branch, pr, merge_evidence }`. On refusal it
  emits nothing destructive and puts the refusal reason in the digest. A
  repeated poll or a server restart re-runs the check, which is idempotent:
  a reaped edge is closed and is not considered again.
- **Done with an open PR** hibernates rather than reaps (the owner's
  2026-10-09 lifecycle comment on #632). The edge then shows
  `hibernated, awaiting merge`, so an empty pane is not read as live headless
  work. Review fixes resume the same agent and worktree.
- **Not decided here:** the context-fill threshold for resume-then-compact
  (needs #581's harness-provided fill), multi-repo deliveries, and non-PR
  retention. They are follow-ups. Until then a delegate with no PR is never
  reaped by policy.
- **Batch landings** (`landed-in-batch` in the hand-rolled layer) are #694's
  to express. #694 decides how a batch close counts as merged, and the reap
  rule consumes that verdict.

### 6. Queued starts are intents with a gate, and slice 1 of them only notifies

`flk delegate queue <name> --after pr:<n>|issue:<n>|delegate:<name> --brief
<path> [delegate start options]` stores a **start intent** on the architect's
edge list, server-side and durable. Gates are evaluated on the events that can
open them (PR merged per the poller, delegate reaped or gone, issue closed via
`gh` on a bounded sweep). A gate that cannot be evaluated stays shut and its
reason shows in `delegate.list`.

When the gate opens, the digest says `ready: <name> (after PR #614 merged)`
together with the exact `flk delegate start` command. The architect starts
it. **Auto-start is deliberately not in this ADR.** `delegate start` is
composed client-side today (worktree, agent start, brief submit, readiness,
rollback), and moving that composition into the server is its own decision.
Caps decide whether an opened intent may start. Those are #822's: a
per-architect count is `delegate.list` filtered to live edges, and #822 adds
nested subagents and build slots on top.

### 7. How this composes with the neighbouring issues

- **#573 goal mode** owns the goal record: the frozen root and checks, the
  mutable status, the gated subtask checklist, the pre-compact handshake and
  the parked verdict. #632 contributes two things to it. An edge may carry a
  `goal` link (the `"goal": null` that `delegate status` already reserves), and
  a subtask's owner is a delegate `AgentId` rather than a typed name. The
  goal's re-inject includes this ADR's delegate table.
- **#663 events** replace raw status transitions as digest triggers as they
  land. The digest is #663's consumer for an architect, not a second event
  system (ADR-0005 P5).
- **#694 landing** consumes `delegate.list --branch` to route a blocked PR to
  its owning delegate or its architect, and its merged verdict feeds §5.
- **#697** is a precondition for §4 to be reliable on architects with
  background shells, which is almost all of them. §4 does not work around it.
- **#805** supplies the identity block. `delegate.list` rows use it once it
  exists. Until then they carry `AgentId`, pane, workspace, branch and repo
  explicitly.
- **#815** renders §2's tree. **#820** adds a `needs_human` digest line.
  **#822** owns caps.

### 8. Non-goals

- **Cross-host edges.** A delegate started on another host through `ssh`
  records nothing (§1). Attributing it needs a mesh-attested caller
  (ADR-0026), which is a later decision. Until then that architect keeps
  explicit tracking for remote delegates.
- **Merge policy.** Review and merge remain the architect's (or the goal's).
  flk reaps after a merge. It never merges.
- **A new transport.** Digests are inbox messages and ride the existing
  delivery paths.
- **Replacing `flk delegate`'s client registry.** It keeps its job (§1).

### 9. Slices

Each slice is independently shippable and testable, and is sized at roughly
400 diff lines or less. Dependency order:

| # | Slice | Depends on | Issue |
| --- | --- | --- | --- |
| 1 | `AgentDelegated` edge from the attested caller at `agent.start` / `agent.spawn` / `agent.fork`, folded and re-issued across restarts and live-handoff | | #959 |
| 2 | `delegate.list` verb, `flk delegate list --mine/--architect/--all`, MCP `flock_delegate_list` | 1 | #960 |
| 3 | Coalesced `from: flock` digest on settled delegate status transitions | 1 | #961 |
| 4 | Post-compaction re-inject appends the architect's delegate table | 2 | #962 |
| 5 | `--reap-on merged`: server-side merged check, guarded teardown, `DelegateReaped` receipt, refusals in the digest | 1, 3 | #963 |
| 6 | Queued start intents with event-driven gates, notify-only | 2, 3 | #964 |
| 7 | Goal link on the edge, and goal re-inject includes the table | 2, #573 | #965 |

**Slice 1 ships alone.** With it, every start an architect makes is a durable,
attested fact in the log. `flk lineage` and the event log can already show
it, and nothing else changes. Slices 2 and 3 together retire `agents.json` and
the supervisor's status loop. Slice 5 retires its reap loop, and slice 4
retires its re-inject step for every part except the goal (slice 7).

## Consequences

- An architect's registry stops drifting, because nobody writes it. It is
  derived from starts the server saw.
- Teardown authority now follows attested ancestry. A wrong edge could reap a
  worktree under `--reap-on merged`, which is why the architect is never a
  parameter and why the reap re-runs the merge gate instead of trusting the
  poller.
- The first server-authored agent message is introduced. Its sender is
  distinct, so nobody mistakes a digest for a message from a peer agent, and
  it expects no reply.
- Delegates started by hand, by the operator, or from another host are
  invisible to `--mine`. That is correct for the first two, and §8 names the
  third.
- `agent.start` gains a side effect (an event) for agent callers. Operator
  starts are unchanged.

## Alternatives rejected

- **Make `flk delegate`'s client-side JSON the registry and have the server
  read it.** It is per socket path and per user, it is written by a client that
  may die mid-write, and it misses `agent.start` and fork entirely.
- **A separate store file for edges.** ADR-0005 P5 rules out a second event
  system, and lineage already shows the fold-and-seed pattern works.
- **Edge as a parameter (`--architect`).** It is spoofable, and the edge
  carries teardown authority.
- **Type digests into the architect's pane.** That is exactly the
  hand-rolled failure (claude-config#18, #559). The inbox exists.
- **One message per transition.** A fleet of eight produces dozens per hour.
  The supervisor already had to batch them (`len(task_lines) > 6`).
