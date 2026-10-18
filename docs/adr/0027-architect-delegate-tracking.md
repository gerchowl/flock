# ADR 0027 — An architect's delegates are a server-held edge, and everything else is a projection of it

- Status: Proposed
- Date: 2026-10-11
- Issues: #632 (the epic this splits). Composes with #573 (goal mode), #663
  (finished/stalled events, PR + CI join), #694 (landing queue), #697 (turn
  state vs background activity), #805 (entity model), #815 (sidebar tree),
  #822 (delegate budget), #395 / ADR-0023 (decisions). Field evidence: #819.
- Decision owner: operator.
- Implemented: none. Slice issues are listed in §9.
- Revised: 2026-10-11 after the PR #966 review (architect succession, hook-output re-inject, #697 and #850 delivery, worktree provenance for reap).
- Amended: 2026-10-11 by ADR-0028 §5 (owner decision 1, PR #1006): the `fyi` digest wakes an idle architect only when it opens a wait the architect registered. See the amendment note in §3.

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
  So does a caller whose ancestry resolves to no pane. That includes `ssh
  host flk delegate start` from another machine (§8) and a spawner that
  detached itself (`setsid`, `nohup` under a reparenting shell), which
  `spawn_caller()` cannot tell apart from the operator. That fallback must not
  be silent: `agent.start` returns `edge: none (unattested)` and `flk delegate
  start` prints it, so an architect learns at start time that this delegate
  will not appear under `--mine`.
- **Keys are `AgentId`s**, not pane ids, which change across moves and
  restarts (#573's owner decision 2, #805). A delegate that is hibernated and
  resumed keeps its edge.
- **The live index is a fold.** On start the server folds `AgentDelegated`
  into an in-memory `architect → [edge]` index, exactly as lineage folds
  `AgentForked`. Log rotation could drop an old live edge, which lineage
  today simply accepts. An edge can carry teardown authority (§5), so losing
  one silently is worse: the server re-emits every **open** edge with
  `reissued: true` once per server start. An open edge therefore always has a
  copy in a recent log file. Which edges are open is decided against the
  **live agent records and worktrees** at that moment, not only against logged
  `AgentRemoved` events, because a crash can lose the removal event.
- **A delegate that goes away does not close its edge.** `AgentRemoved` with
  no resumable session moves the edge to `gone`. If the edge owns a worktree
  (§5), the edge stays open as `gone, worktree retained`, still listed and
  still eligible for the merge-gated reap. Closing it on removal would leak
  the worktree with nothing left pointing at it. A `gone` edge that owns
  nothing closes.
- **The edge closes** when it is reaped (§5), when a `gone` edge owns nothing,
  or by `flk delegate forget <delegate>` (operator only). It is never edited
  in place. Later facts (state, PR number, reap receipt, reassignment, goal
  link) are their own events that reference the delegate's `AgentId`.
- **Old servers.** `agent.start` gains an optional `label`. Per #868's skew
  rules, the client sends it only to a server that advertises the capability,
  and otherwise omits it and says so rather than failing late on an unknown
  field.

`flk delegate`'s client-side registry stays. It holds what only the CLI needs:
the brief, the turn cursor, the submit time. It is no longer the answer to
"which delegates are mine".

### 1a. When the architect is replaced or gone, its edges are orphaned, never dropped

An architect's `AgentId` survives a resume (#582), a hibernate and a
compaction. It does not survive a harness change: #819 reports two
Claude → Codex takeovers, each a new agent with a new id, and an architect
that simply exits records `AgentRemoved`. Without a rule, every edge it held
would be unreachable: `--mine` empty for the successor, no digest target, reap
receipts addressed to nobody.

- **Orphaned edges.** When the architect is removed with no resumable session,
  its open edges become `orphaned`. They stay in `delegate.list --all` under
  an `orphaned (architect gone)` group. Their digest lines go to the operator
  as notification-log records (ADR-0016), not to an agent inbox. Nothing about
  an orphaned edge is reaped automatically, whatever its policy: teardown
  authority belonged to an architect that is no longer there to answer for it.
- **Explicit transfer.** `flk delegate reassign <from-architect>
  <to-architect> [--delegate <name>...]` moves edges to a successor and emits
  `DelegateReassigned { delegate, from, to, by }`. It works on live and
  orphaned edges alike, and the successor's `--mine` then lists them with
  their reap policies re-enabled.
- **Reassign is an operator verb.** It is kept off MCP and refused for an
  attested agent caller (the ADR-0014 authority boundary). Edges carry
  teardown authority, so an agent that could adopt edges could get another
  agent's worktree reaped. A takeover is operator-initiated in every #819 case
  anyway. A successor adopting its predecessor's edges on its own start, with
  the predecessor's consent token, is a possible later refinement and is not
  decided here.
- **A known limit of the operator boundary.** "Operator" means "no pane in
  the caller's ancestry" (ADR-0014). An agent that detaches a child
  (`setsid`, a reparenting `nohup`) makes that child attest as the operator,
  so it could run `reassign` or `forget`. This ADR does not close that gap,
  which is ADR-0014's. It bounds the damage instead: reassigned or not, a reap
  still needs the merge gate to pass, worktree ownership (§5), a clean,
  pushed tip, and no other open edge on the checkout. A mis-reassigned edge
  can therefore at most remove a worktree whose work is already merged.
  `DelegateReassigned` records `by` for the audit.

### 1b. Named dialogs and explicit conversation-lock choices (#1667)

The existing CLI registry (§1) also records the latest explicit Codex lock
choice in its JSON entry, without a new event kind or persistence format.
`codex_conversation_locked` is a named human block in the harness dialog table,
recognised only in the live screen tail by the lock notice, continuation
instruction and retry/fork/exit footer. Quoted history above a later composer
is not this state. Detection feeds ADR-0028 §1's human-attention axis.

The live-tail footer may end exactly at `r retry f fork`: this is accepted as
a truncated footer, including blank trailing lines. Text or a composer after
the footer invalidates the match; scrollback alone is not a lock.

No automatic path answers the lock. A start blocked there retains its pane,
checkout and round-0 record and returns `agent_blocked` (ADR-0029 exit 6).
Status and wait name the same dialog at startup or later; wait exits 6.
Readiness consumes the lock diagnosis in `agent.get`'s existing
`blocked_reason`, without another screen RPC. A lock reason requires
`agent_status: blocked`; the server caches the live diagnosis per terminal
content revision and projects that status with its reason, even before the
asynchronous detector publishes the transition. `pane.get` / `pane.list` and
agent reads use the same live status derivation. Internal lock evidence is
`Locked`, `Unlocked`, or `Unavailable(reason)`. A poisoned terminal-core mutex
or snapshot failure yields `Unknown`, with each typed reason class logged once per
terminal, and no lock reason. Snapshot failures are cached until the content
revision changes, just like successful diagnoses. Each agent record derives status
once and reuses that observation for its status, reason and state. It never fails a list request or proves an unlock.
PTY writes, resize and mode
changes invalidate the cache; user viewport scrolling does not. Offline and
hibernated lifecycle states still outrank a lock diagnosis. Other reasons retain their
existing semantics: `resume_failed` can describe an offline identity. A plain
brief send refuses with the registered `codex_conversation_locked` code.

`delegate send --resolve-lock retry|fork` is the explicit choice, mutually
exclusive with a brief or await. The CLI holds the existing delegate lock,
subscribes before observing, durably records the intent, then calls guarded
`pane.resolve_codex_lock`. That operation rechecks the live screen in the
server's owner turn and admits exactly one `r` or `f` with no Enter. A per-pane
fence refuses another choice with `injection_pending` during the bounded write
and a fixed 60-second settle window measured from its completed write. The
actual receipt releases the fence immediately on zero-byte failure, including
deferred failure; an observed unlock releases it only once the pending write
has finished or been retired, so a transient repaint cannot admit two keys. Failed or partial
writes retain their first terminal-write timestamp, so later cancellation or
observation cannot extend the settle window. Pending writes retain their
existing bounded write budget. The fence is runtime-only and resets on server
handoff. A pane without a runtime refuses with `pane_not_found` rather than
`agent_not_ready`.
The CLI uses the same 60-second maximum for confirmation, with smaller
`--ready-timeout` values shortening its wait. After inspecting a still-locked
pane, the operator can choose again after the settle window; flock never
re-sends a key automatically. An older
server rejects this method; it cannot silently ignore a guard and send a raw
key. MCP exposes no new keyboard tool. Mesh VERSION remains 5.

After admission the CLI confirms that the live screen left the lock, using
status notifications and bounded condition polling within the existing
readiness budget, never re-sending. The registry holds only the latest choice,
its timestamp and confirmation bit while the lock remains; unknown future
metadata is preserved raw. Resolution, an observed unlocked screen in status,
an observed unlocked outcome, or the next submitted round clears it. Older flock versions read the additive
key but drop it when rewriting an entry. Status or wait that needs to clear
metadata refuses with `injection_pending` if a concurrent registry writer holds
the lock; wait then returns that typed refusal instead of its normal outcome.
An unconfirmed intent is not proof of delivery. A silent producer, stream
loss, or uncertain write returns registered `codex_lock_unconfirmed` with
ADR-0029's after-inspection exit 8. The operator inspects before another choice.
A pre-request registry durability failure sends nothing; a post-request write
failure can retain an unconfirmed intent. Successful resolution leaves the round
unchanged and does not promise that the next dialog is ready for a brief.
Restoring a missing registry entry remains #1285.

### 2. `delegate.list` is the one read, and every surface is a projection of it

A socket verb `delegate.list { architect: self | <AgentId> | all, branch?, repo? }`
returns edges joined, at read time and from cached state only, with:

- the delegate's live `agent_status`, harness, pane and workspace;
- its branch and the poller's cached `PrStateInfo` for it (#663 adds CI);
- the age of the edge and of the status;
- the edge state: `live`, `hibernated, awaiting merge`, `gone`, `gone,
  worktree retained`, `orphaned`, and `closed, not merged` for a PR that was
  closed without merging (see §5 on batch landings).

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
(ADR-0008, ADR-0018), read by the architect through its tools. The digest is
the first server-authored agent message, so it gets a distinct sender: `from:
flock`, rendered as such, with no reply expected (intent `fyi`).

**How it reaches the architect.** The digest body is never typed into a pane.
Two existing paths tell the architect it has mail, and both are reused
unchanged:

- at a turn boundary, flock's Stop hook writes its mail nudge as hook output
  (`src/cli/hook.rs`), which the harness puts in context. Nothing touches the
  composer, so `input_not_empty` cannot arise.
- an architect that is idle gets the idle-wake sentence (`src/app/idle_wake.rs`):
  a fixed line with no sender or body, submitted through #850's guarded
  injection. That injection already moves a composer draft aside and restores
  it, and it holds the operator's input while it types. The digest adds no
  second injector, so it cannot race #850's lock.

**Amendment (ADR-0028 §5, owner decision 1, 2026-10-11): wake follows the
wait.** As written, the second path never fires for a digest: ADR-0018 lets
only `needs_reply`, `blocking` and answers to a question idle-wake an agent,
and the digest is `fyi` (measured in ADR-0028: an `fyi` to an idle Claude
produced no wake in 60 s). The digest stays `fyi`. It wakes an idle architect
when it opens a wait that architect registered (ADR-0028 §5: a `park` on its
delegates, or a `flk events wait` on them), and only then; an architect that
did not park is not interrupted and reads the digest at its next turn
boundary. ADR-0018's rule for peer mail is unchanged. #961's "parked architect
is woken" acceptance therefore depends on ADR-0028 slices E5 and E6, in
addition to #697.

**Precondition: #697.** An architect almost always has a background shell
running, and while it does flk reports it `working`. Idle wake then never
fires, and an architect parked waiting on its delegates hears nothing until
something else ends a turn. The digest is correct without #697 (mail is queued
and delivered at the next turn boundary), but it is not timely. #961 therefore
depends on #697's turn-state split for its "parked architect is woken"
acceptance, and states that dependency rather than working around it.

- **Triggers in slice 1:** a delegate's status settles into `idle`, `done`,
  `blocked` or `gone`, or comes back to `working` from one of those. A status
  must hold for a settle window (default 20 s, configurable) before it counts.
  This is the #819 false-idle filter (flapping Codex status, compaction,
  quota screens) applied once, in the server.
- **Coalescing without rewriting.** ADR-0026 makes message content
  immutable under its idempotency key, and a retransmit with different content
  under the same key is a conflict. So a queued message is never edited.
  Transitions accumulate in a per-architect **digest buffer** in the server,
  outside the mailbox. The buffer is flushed into one new, immutable message
  only when the architect holds no undelivered digest. While one is still
  undelivered, new transitions wait in the buffer, and the next digest carries
  all of them. A delegate that changes twice in that time gets one line, with
  its latest state. A wake therefore carries one line per changed delegate,
  for example `bunny-614 idle (PR #624 open) · bunny-621 working ·
  astra-623 gone`. The buffer is rebuilt from the log on restart, so a
  transition is neither lost nor sent twice.
- **Later triggers come from other issues, not new plumbing:** #663's
  `agent.finished` / `agent.stalled` / `pr.ci_completed`, #820's
  `needs_human`, and the reap and queue receipts in §5 and §6. Each is one
  more line kind in the same digest.
- **Opt-out** per architect (`flk delegate digest off`), because an architect
  that runs its own `flk wait` loop does not want both.

### 4. After a compaction, the architect gets its delegate table back through hook output

The table is **not** typed. `self_compact_on_session_start` returns early
unless a self-compaction was armed (`src/app/self_compact.rs`), so ordinary
auto-compaction, which is most compactions, would never get a table on that
path. And typing a multi-line table into a TUI is exactly what §3 rejects
(#850).

Instead, flock's own SessionStart hook does it. When a session starts with
`source=compact` (or `resume`), the hook asks the server for the agent's
`delegate.list --mine` and, when there are open edges, writes the rendered
table as hook output: Claude's SessionStart `additionalContext`. The Stop hook
already composes its mail nudge from server state this way. The table is
bounded to a fixed number of rows plus a `+k more: flk delegate list --mine`
tail.

- **Claude Code:** SessionStart hook output, as above. This covers armed and
  unarmed compactions alike.
- **Codex:** Codex hooks carry an `additionalContext` field too (flock's
  hook-trust code already normalizes its limit,
  `src/integration/codex_hook_trust.rs`). The slice verifies that Codex's
  session-start event honours it before relying on it, and otherwise falls
  into the last case below.
- **opencode:** the plugin adapter injects it on its session-compacted event.
  #573 already plans that adapter, so the table rides on it.
- **A harness with no session-start hook** gets no table. The architect calls
  `flock_delegate_list` itself, and the slice documents that.

That retires the hand-kept `agents.json` re-read and the GOAL.md-plus-registry
re-inject step of the supervisor. The frozen goal, the
mutable status and the pre-compact status handshake belong to #573's goal
record. When that record exists, its re-inject renders goal, checklist, status
and this table in that order (§7).

### 5. Lifecycle: done hibernates, merged reaps, and the server does both

Each edge carries a reap policy, set at start (`--reap-on merged|never`,
default `never` until the reap slice has run in the field).

**An edge authorises teardown only of a worktree its own start created.** An
edge from `agent.start --cwd <someone else's worktree>` is an attested fact
about who started an agent. It says nothing about who owns that checkout, and
it must not let the architect's policy remove it. So:

- `worktree.create` records its attested creator in a durable
  `WorktreeCreated { path, branch, created_by: AgentId }` event (it already
  resolves `spawn_caller()`, `src/app/api/worktrees.rs`);
- an edge **owns** its worktree when, at the moment the edge is recorded, the
  delegate's worktree path matches a `WorktreeCreated` whose `created_by` is
  the edge's architect, or when the `agent.fork` that produced the delegate
  created it. The server cannot see "the same `delegate start`": it sees two
  socket calls from one attested caller. So ownership is defined as **path
  plus creator**, not as one invocation. An architect that creates a worktree
  and later starts a second delegate in it owns it on both edges, and the
  "another open edge points at it" rule below is what keeps the first
  delegate's checkout safe;
- ownership is decided once, recorded on the edge, and **travels with it on
  reassign** (§1a). A reassign is an operator act, so the successor inherits
  exactly the teardown authority the predecessor had, no more. Re-deriving it
  against the new architect would make every reassigned edge unreapable;
- reap refuses an edge that does not own its worktree, and says so in the
  digest. A checkout that another open edge still points at is never removed
  either.

The policy itself:

- **`merged`:** when the PR poller reports the delegate's branch merged, the
  server re-checks with `branch_merge_gate` (squash-aware, #321 / #631), then
  runs the same guarded teardown as `worktree.kill`, with the owner's
  2026-10-09 conditions on #632:
  - the teardown refuses dirty, unpushed or advanced work, as it does today;
  - **merged means merged**: a done claim, green CI, or a PR closed without
    merging is not enough. A multi-repo delivery with one PR still open is not
    merged;
  - evidence and resumable metadata, **including ignored files**, are copied
    outside the checkout before it is removed;
  - the delegate's own surviving children are settled first; an open child
    blocks the reap;
  - success emits `DelegateReaped { delegate, branch, pr, merge_evidence,
    retained }`. A refusal emits nothing destructive and puts its reason in
    the digest;
  - a repeated poll, a duplicate merge observation or a server restart re-runs
    the check, which is idempotent: a reaped edge is closed and is not
    considered again.
- **Orphaned edges** (§1a) never reap automatically.
- **Done with an open PR** hibernates rather than reaps (the owner's
  2026-10-09 lifecycle comment on #632). The edge then shows
  `hibernated, awaiting merge`, so an empty pane is not read as live headless
  work. Review fixes resume the same agent and worktree.
- **Not decided here:** the context-fill threshold for resume-then-compact
  (needs #581's harness-provided fill), multi-repo deliveries, and non-PR
  retention. They are follow-ups. Until then a delegate with no PR is never
  reaped by policy.
- **Batch landings never reap under this ADR.** The hand-rolled batch
  landing closes member PRs unmerged with the `landed-in-batch` label. Under
  "merged means merged" such a PR does not reap, and `delegate.list` shows it
  as `closed, not merged` so it is visible rather than silently stuck. Whether
  a batch close counts as merged is #694's decision, and the reap rule will
  consume that verdict once #694 makes it.

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
- **#697** is a precondition for §3's digest to wake a parked architect,
  because an architect with a background shell reads as `working` and idle
  wake never fires. §3 does not work around it. §4 no longer depends on it,
  since the table rides the SessionStart hook rather than an armed
  self-compaction.
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
| 1 | `AgentDelegated` edge from the attested caller at `agent.start` / `agent.spawn` / `agent.fork`; `gone` without closing; re-issue checked against live records; unattested starts reported | | #959 |
| 2 | `delegate.list` verb, `flk delegate list --mine/--architect/--all`, MCP `flock_delegate_list`, all edge states | 1 | #960 |
| 3 | `from: flock` digest from a server-side buffer, one immutable message at a time, through the existing nudge and wake paths | 1; #697 (wake acceptance only) | #961 |
| 4 | Delegate table as SessionStart hook output after compaction or resume | 2 | #962 |
| 5 | Hibernate-on-done: `hibernated, awaiting merge` state | 1, 3 | #963 |
| 6 | Worktree provenance and `--reap-on merged`: ownership check, merge gate, evidence retention, children settled, receipt | 1, 3, 5 | #968 |
| 7 | Queued start intents with event-driven gates, notify-only | 2, 3 | #964 |
| 8 | Goal link on the edge, and goal re-inject includes the table | 2, #573 | #965 |
| 9 | Orphaned edges and operator `flk delegate reassign` / `forget` | 1, 2 | #969 |

**Slice 1 ships alone.** With it, every start an architect makes is a durable,
attested fact in the log. `flk lineage` and the event log can already show
it, and nothing else changes. Slices 2 and 3 together retire `agents.json` and
the supervisor's status loop. Slices 5 and 6 retire its reap loop, slice 4
retires its re-inject step for every part except the goal (slice 8), and slice
9 makes a harness takeover keep its fleet.

## Consequences

- An architect's registry stops drifting, because nobody writes it. It is
  derived from starts the server saw.
- Teardown authority now follows attested ancestry **and** worktree
  provenance. A wrong edge alone cannot reap a worktree: the reap also needs
  the edge to own the checkout (§5), re-runs the merge gate instead of trusting
  the poller, and never runs for an orphaned edge.
- The first server-authored agent message is introduced. Its sender is
  distinct, so nobody mistakes a digest for a message from a peer agent, and
  it expects no reply.
- Delegates started by hand, by the operator, or from another host are
  invisible to `--mine`. That is correct for the first two, and §8 names the
  third.
- `agent.start` gains a side effect (an event) for agent callers. Operator
  starts are unchanged.
- An architect that is replaced by a new agent (a harness takeover) loses
  nothing, but it gets its fleet back only through an operator
  `reassign`. Until then the orphaned edges are visible, report to the
  operator, and are never reaped.

## Alternatives rejected

- **Make `flk delegate`'s client-side JSON the registry and have the server
  read it.** It is per socket path and per user, it is written by a client that
  may die mid-write, and it misses `agent.start` and fork entirely.
- **A separate store file for edges.** ADR-0005 P5 rules out a second event
  system, and lineage already shows the fold pattern works. The re-issue
  rule in §1 covers what lineage does not: rotation.
- **Edge as a parameter (`--architect`).** It is spoofable, and the edge
  carries teardown authority.
- **Type digests into the architect's pane.** That is exactly the
  hand-rolled failure (claude-config#18, #559). The inbox exists.
- **Rewrite the pending digest in place.** ADR-0026 makes message content
  immutable under its idempotency key. A server-side buffer gives the same
  coalescing without touching custody.
- **Re-inject the table by typing it after compaction.** Only armed
  self-compactions take that path, and typing is what §3 rejects. Hook output
  covers every compaction.
- **Let an agent adopt edges.** Edges carry teardown authority, so adoption
  stays an operator verb (§1a).
- **One message per transition.** A fleet of eight produces dozens per hour.
  The supervisor already had to batch them (`len(task_lines) > 6`).
