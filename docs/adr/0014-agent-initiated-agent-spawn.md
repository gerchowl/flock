# ADR 0014 — An agent may start an agent, but only through a narrowed verb with a lineage-aware ceiling

- Status: Accepted
- Date: 2026-08-21 (accepted 2026-08-21)
- Issues: #329 (the tool + the cap); consumes #332's run-id join; constrained
  by ADR-0005 (durable event log as the audit substrate) and ADR-0006
  (structured addressing, never a flat string).
- Decision owner: operator. Design consolidated from two independent
  fresh-context reviews (capability-boundary and wire-contract) run on the
  #329 proposal, both of which refused that proposal as filed.

## Context

A dispatcher agent — a "foreman" that triages issues and farms implementation
out to children — cannot start a fresh agent today. The only spawn verb on the
MCP surface is `flock_agent_fork`, and fork is the wrong primitive for
dispatch: `--fork-session` **copies** the parent's whole transcript into the
child (`src/app/worktrees.rs`, the resume plan is
`["claude", "--resume", <sess>, "--fork-session"]`). A 150k-token foreman
forking five implementers costs ~900k and hands every child triage chatter
irrelevant to its one issue. Fork is cheap only when the parent is small, and a
dispatcher's context is never small. Fork is also Claude-only —
`branch_plan` refuses everything else with `ForkUnsupported`.

The exclusion of `agent.start` from MCP is deliberate and documented
(`src/mcp/tools.rs`): anything not in the closed table refuses with
`not_exposed_via_mcp`, and `agent.start`, `pane.close`, `worktree.remove` and
pane `send_*` are named as the mutating verbs kept off. `agent_fork` is the one
spawn mutation allowed because it is **bounded** — one child, one linked
worktree, from an existing session, stamped with a `run_id` whose
`Agent-Run:` trailer makes the child's commits revertable by `flk revert-run`.

#329 proposed exposing `agent.start` with "no raw argv" as the constraint. Two
independent reviews concluded that is necessary but nowhere near sufficient.

## Decision

**Agent-initiated spawn ships as a NEW narrowed verb with a lineage-aware
ceiling — never as the existing `agent.start` with a constrained wrapper.**

### 1. A new `Method::AgentSpawn`, not a reuse of `Method::AgentStart`

`AgentStartParams.argv: Vec<String>` is a shell-level "run this binary"
primitive backing `flk agent start ... -- <cmd>`. A constraint that lives only
in the MCP builder is one refactor away from being defeated: any future tool
that also lands `Method::AgentStart` inherits the raw-argv escape hatch, and a
tool-table review will not catch it because the type still says "argv is a
`Vec<String>`, do what you like".

`handle_agent_start` also performs none of the run-id minting, trailer
installation, or lineage recording that the fork path does. Bolting those onto
`AgentStart` would retroactively change what the CLI verb does.

The **tool** an agent calls is nevertheless named `flock_agent_start`: tool
names describe intent ("start an agent"), and an agent hunting for that
capability should find it under that name. What it builds is
`Method::AgentSpawn`. `Method::AgentStart` stays off the MCP surface, so the
split is exactly the point — the label is ergonomics, the Method is the
constraint, and only one of the two survives a careless refactor.

**The invariant belongs in the type system.** `AgentSpawn` carries a closed
`AgentKind` enum (argv assembled server-side), and the trailer/lineage code runs
unconditionally on that path. This is the same posture as `ActionSpec`
(`src/checks/config.rs`): a closed enum, never a shell string.

### 2. Location is a discriminated union over places flock already owns

Not a free-form `cwd`. `start_agent` today accepts any string and falls back to
`current_dir()` then `/`, with no check that the path is inside a workspace
flock owns — an MCP caller could point a git-aware child at any repo on disk,
and `flk revert-run` only walks repos it is told about.

Accepted forms: a `worktree_path` that `flock_worktree_list` returned, a
`new_branch` (+ optional base) using the fork path's branch logic, or a
`workspace_id`. Same shape as `MessageTarget` — the codebase's established
idiom for "one of several kinds of address".

### 3. The child's environment is opt-in, not inherited

The PTY child currently inherits the caller's environment unfiltered. That
includes `GH_TOKEN`, `SSH_AUTH_SOCK`, `ANTHROPIC_API_KEY`, `CLAUDE_CONFIG_DIR`
— and, if the one-shot run-id guard is ever missed, a stale `FLOCK_RUN_ID` that
would misattribute another pane's commits. An agent-initiated spawn starts from
a scrubbed baseline with an explicit allowlist, the way `[[checks.script]]`
already does (`env_clear()` plus a declared allowlist).

### 4. The opening prompt is untrusted input

On `agent.fork`, `pivot` lands inside an agent whose role is already
established. On a fresh spawn, the prompt **is** the whole opening turn — and a
foreman's prompts are derived from GitHub issue bodies, the exact author-trust
surface `issue_guard` was rebuilt around after a body-authored trigger let any
GitHub user drive flock into acting on a public repo. The spawn carries a
system-owned preamble the caller cannot displace, and refuses control-sequence
bytes as `msg_send` already does.

### 5. The ceiling is lineage-aware, not a single global counter

A flat `max_concurrent_agents` is a footgun on its own: an adversarial or
merely buggy subtree occupies N−1 slots, starves the dispatcher, and pushes it
to fork instead — turning the cap into a fleet-wide denial switch. Three limits,
outermost last:

| limit | refusal | why |
| --- | --- | --- |
| fork-tree depth (default 2) | `at_lineage_depth` | stops recursive spawn without needing to detect intent |
| live descendants per parent | `at_fanout_limit` | bounds one bad dispatcher |
| `[fleet] max_concurrent_agents` | `at_agent_capacity` | the outer wall |

Depth and fanout are answerable from the lineage edges already emitted
(`AgentForked`), so this needs no new bookkeeping — only that the edge is
written **before** the child is exec'd, fail-closed, mirroring the existing
run-id guard.

### 6. The ceiling lives in the shared spawn funnel

Not in the MCP tool. It counts live run-tagged descendants — not in-flight RPCs
— so an operator's keyboard fork and an agent's spawn see the same headroom.

### 7. `fleet pause` refuses agent-initiated spawns

`fleet_pause` deliberately exempts human keystrokes: pause halts the scheduler
and delivery, not human agency. **An MCP-originated spawn is not a human.** It
refuses while paused, which is the first case where the pause switch has to
distinguish caller class rather than mechanism.

### 8. Refusals must separate "retry is safe" from "retry fails identically"

Every refusal carries structured data beyond the tag — `at_agent_capacity`
carries current/limit/retry-after, `unknown_agent_kind` carries the supported
set, `branch_exists` carries the existing path so the caller can pivot to a
`worktree_path` spawn. An agent that cannot tell a transient refusal from a
permanent one will retry-loop, and that is the failure this rule prevents.

## Consequences

**Gained.** Dispatch stops paying the transcript-copy tax. Every
agent-created agent is attributable (`flk lineage`) and revertable
(`flk revert-run`), because the spawn path cannot skip either. The blast radius
of a compromised or confused dispatcher is bounded by three independent limits
rather than by prompt compliance.

**Given up.** Two spawn verbs with overlapping intent now exist —
`agent.start` (operator, raw argv) and `agent.spawn` (agent, narrowed). That
duplication is deliberate: the alternative is one verb whose safety depends on
which caller reached it, which is exactly the property that cannot be enforced
in a type.

**Cost.** Env scrubbing needs a per-agent allowlist, and getting it wrong
breaks spawns in ways that surface as an agent failing mysteriously on start.
It should ship with the allowlist logged at spawn. #359 was that failure
arriving before the allowlist did — a variable a login shell supplied and the
server never had — which is why the table is keyed per agent and the log line
was not treated as optional.

**Not settled here.** Whether the depth default of 2 is right — it is a guess
until there is telemetry. Ship it refusing at 2 and widen on evidence, because
widening a limit is a decision and narrowing one is an incident.

## Implementation status

Accepted means the decision is committed to, not that every part is built.
As of acceptance (#345):

| § | decision | state |
| --- | --- | --- |
| 1 | `Method::AgentSpawn`, closed `AgentKind`, server-side argv | landed |
| 2 | location as a union over known checkouts | landed, minus `new_branch` — a caller creates the checkout first and spawns into it by path |
| 3 | env allowlist | landed (#347). `env_clear()` plus a per-agent table (`src/spawn/allowlist.rs`), armed only on the agent-spawn path, with the resolved keys on the spawn's log line so an omission is a read rather than a bisect. The deny-list stays on beneath it as a second line. Not settled: a fleet on Bedrock or Vertex needs `AWS_*` / `GOOGLE_APPLICATION_CREDENTIALS`, which §3's own deny-list strips — allowing a third-party cloud credential through is the operator's call, not the implementer's |
| 4 | untrusted prompt: system-owned preamble, control-byte refusal | **not landed** (#348). The prompt is length-capped and passed as one argv element, so nothing reaches a shell — but a foreman's prompts come from issue bodies, and neither the preamble nor the byte filter exists yet |
| 5 | lineage-aware ceiling (depth → fanout → capacity) | landed |
| 6 | ceiling in the shared spawn funnel | landed (#349). `agent.spawn` and `agent.fork` both ask one App-side funnel, which gates a caller CLASS rather than a verb: a caller whose process ancestry lands in an agent's pane meets the three limits, an operator meets none of them. Agent-initiated fork children are stamped with `spawned_by`/`spawn_depth`, so they count and the two verbs cannot be alternated past the cap. The TUI keyboard fork never enters the funnel at all — deliberately, per the operator exemption. Note the fork half is NOT behind `[fleet] agent_spawn_enabled`: that switch opts a node into `agent.spawn`, and gating an already-shipped verb on it would leave the bypass open on every node that never set it |
| 7 | `fleet pause` refuses agent-initiated spawn | landed |
| 8 | refusals separate retryable from terminal | landed |

The remaining gap is #348 — tracked as an issue rather than left in this
table, because an Accepted ADR whose unbuilt parts live only in prose is how
they get forgotten.

What #349 turned out to be about, worth keeping: the ceiling could not be
described by naming verbs. `agent.spawn` gated, `agent.fork` not, and the two
cost the fleet the same thing — so the cap held only for as long as an agent
reached for the verb that had it. What it gates now is who is asking, which is
the same line `fleet pause` draws in §7 and the only one that does not have to
be re-drawn every time a verb is added.

## Alternatives rejected

**Expose `agent.start` with a constrained MCP wrapper** (the #329 proposal).
The constraint is not expressible in the type, so it holds only as long as
every future caller remembers. Rejected on the same grounds ADR-0006 rejected a
flat-string address: if the wire type permits the bad shape, review is the only
thing standing between you and it.

**A flat concurrency cap alone.** Rejected — see §5. It does not bound depth,
and its failure mode is starving the legitimate dispatcher first.

**Prompt-level convention** ("check capacity before you dispatch"). Rejected:
it fails silently, under exactly the concurrency it is meant to protect
against.
