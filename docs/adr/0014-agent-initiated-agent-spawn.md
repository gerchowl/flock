# ADR 0014 — An agent may start an agent, but only through a narrowed verb with a lineage-aware ceiling

- Status: Accepted
- Date: 2026-08-21 (accepted 2026-08-21)
- Issues: #329 (the tool + the cap); consumes #332's run-id join; constrained
  by ADR-0005 (durable event log as the audit substrate) and ADR-0006
  (structured addressing, never a flat string).
- Decision owner: operator. Design consolidated from two independent
  fresh-context reviews (capability-boundary and wire-contract) run on the
  #329 proposal, both of which refused that proposal as filed.
- Amendment (2026-09-08): §3's allowlist stands, and so does its inversion —
  `env_clear()` plus an explicit set, decided here and shipped in #383. What
  changes with #397 is who writes one half of the table. The keys whose
  absence changes WHICH ACCOUNT the child runs as are now DECLARED BY THE
  FLEET in `[spawn.env]` (`crate::config::SpawnEnvConfig`), keyed by agent
  name and merged over a compiled default, so a fleet whose agents find their
  credentials differently no longer waits for a flock release to say so. That
  declaration feeds the carry (`spawn::env`) and the allowlist
  (`spawn::allowlist`) from ONE table, so a variable cannot be resolved from
  the requester and then cleared by the scrub. Two things stay flock's: the
  scrub itself, which is the security property rather than the policy; and the
  refusal, since a declared key that is absent still produces
  `agent_profile_unresolved` rather than a child that starts on the default
  profile. The credential-shaped keys §3 lists (`ANTHROPIC_*`) stay compiled
  in for a reason worth stating — the declared list carries a refusal, and a
  missing `ANTHROPIC_API_KEY` must not refuse a spawn. Different semantics,
  different table. The `AgentKind` enum stays closed; a config key names an
  agent flock already detects and an unknown one is a diagnostic, not a parse
  error.

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

#### Agent identity text (#317)

Agent names, hook-reported agent labels and manual pane labels are untrusted
renderer input. Reject C0/C1 controls, ESC/CSI/OSC introducers, line breaks,
Unicode line separators, and Unicode format characters (category Cf), except
ZWJ (U+200D). Variation selectors U+FE00–FE0F remain allowed. Reject the entire
Unicode tag block U+E0000–E007F and labels longer than 64 Unicode scalar values
before trimming. This also prevents zero-width lookalikes from evading name
uniqueness checks.

`TerminalState::set_agent_name`, the manual-label setter and hook/metadata
setters validate before mutation. Rename, creation, report-agent and metadata
APIs return `invalid_agent_name` without echoing rejected text. The pane rename
modal displays a refusal and preserves its input and previous label. Safe empty
names retain the existing clear-name behavior; creation and agent reports
require a nonempty name.

Older mesh v5 peers and saved PTY sessions can carry invalid labels. Receivers
drop the invalid identity field and warn with a typed code and fixed source,
without its content. Restore drops any invalid manual pane label, independently
of the agent name. Invalid structured records remain inactive with raw data
retained (ADR-0028 §3a). Navigator, agent-panel, toast and desktop-notification
labels escape unsafe text at the final display boundary. Overlong legacy
labels display a truncated prefix with a visible `…` marker. Pane/agent JSON
and GUI status events drop invalid agent labels rather than forwarding them.
This does not change the wire layout or mesh version. The separate delegate
filename restriction remains narrower than the general Unicode display rule.

#### Presentation text (#2090)

Titles and peer workspace/activity text are presentation, not identity. Strip
characters forbidden by the identity policy above, preserving ZWJ and variation
selectors, rather than refusing the record. Apply this at metadata admission and
projection (including older/internal producers), peer JSON, binary fleet and
relay receive boundaries, and notification output. Mesh v5 layout stays unchanged
so older peers remain readable. The GUI and CLI receive sanitized strings, and
the navigator sanitizes legacy text at its render boundary. Presentation text
has no identity-length cap: retain full sanitized text for search, and truncate
only when drawing into the available width. Metadata API titles retain their
existing 80-scalar budget. Empty reported agent labels return
`invalid_agent_name` with a nonempty-label message, separately from policy refusals.

Peer receive admission includes project keys, because the sidebar derives its
project-group label from that key before considering `project_label`. JSON
workspace/agent admission and binary/relay conversion share a single admission
method per record. Generic schema deserialization remains data-only, without
peer warnings; the JSON summary parser, binary conversion and relay merger are
the admission boundaries. Each peer presentation field retains at most 256 Unicode
scalar values after sanitization: workspace, project key/label, branch, activity,
peer display name, version, icon, error and origin display labels.
The cap bounds render/search work after the existing frame-size check. Identity
labels still use 64 scalars; thermal labels additionally retain their 16-byte cap.
Unknown icon names resolve to a blank icon; dial reasons are classified tokens.
Opaque workspace/pane/agent/node identifiers are not edited into new identities:
valid printable ASCII tokens up to 256 bytes remain exact; malformed identifiers
are cleared with a content-free `invalid_params` warning. Empty directory entries
are omitted. Workspace ids already support the legacy id-less/name-focus path.
SSH destination and ProxyJump grammar is checked with the same 256-byte bound:
invalid relay rows are refused, and invalid carried routes are disabled rather
than partially rewritten. The reserved home target remains supported. The mesh
layout and VERSION remain unchanged.
Reported hosts are hostname identities, not agent display names. Admit only
ASCII LDH labels: letters, digits and internal hyphens, with an alphanumeric
first and last byte, 1–63 bytes per label and at most 253 bytes overall.
Dot-separated labels may not be empty; a trailing dot is refused. IDNA A-labels
(`xn--bcher-kva`) are accepted as plain ASCII, without decoding or Unicode conversion.
IPv4 text naturally meets this grammar. Canonical bare IPv6 literals are also
admitted; bracketed or expanded noncanonical IPv6 reports are refused. Reject
spaces, underscores, Unicode
(including ZWJ, combining marks, variation selectors and confusables), controls
and overlong values before keying or presentation cleaning. Never trim, strip,
truncate or normalize a rejected identity into an admitted one. Valid spelling
is retained apart from the existing ASCII case-insensitive cache lookup.

The reported host comes from the node's configured `name` or its OS hostname;
a node with an invalid identity must configure an LDH name to participate in
peer summaries, then restart: the advertised hostname is cached for the session.
SSH destinations are separate: `user@host`, ports, bracketed IPv6 and
SSH aliases keep their existing destination/ProxyJump grammar and 256-byte bound.
They are not admitted as opaque reported host identities. For an absent or
empty host, one shared key function parses the SSH destination: remove the
optional user and numeric port (1–65535), unbracket IPv6 and canonicalize its
address, then apply the same host validator. Bare IPv6 means an address, not an
address with a guessed port. DNS names are lowercased without DNS resolution;
IPv4 and canonical IPv6 literals keep their complete address. Trailing dots,
underscores, malformed authorities and every other invalid fallback are refused
before either relay-cache or carried-fleet keying, with the same content-free
`invalid_params` diagnostic. Routes retain their original spelling. Relay `via` edge names retain the shared
forbidden-scalar predicate and 256-byte routing bound; they are not reported
host fields. An empty `via` cannot route a row.

Startup checks the effective configured or OS name without renaming it. A
registered `invalid_node_host` warning (`after_human`) reports the LDH/IP rule
and instructs the operator to set `name` and restart. It appears in startup
configuration diagnostics, `flk config check`, and human-readable `flk status`;
`flk status --json` exposes `{code, message, recovery}` as `node_host_warning`
when present. Logs emit `config.node_host_invalid` with the typed code, rule and
recovery, without the rejected name. The server emits it once after logging is
initialized; config reads and status clients return diagnostics without emitting
that startup event.

Configured peer rows start with their strict `[[peers]].name` key. After the
first successful poll, their identity set also includes the strict admitted
reported-host key. A relayed or carried row matching either member collapses
into the config peer's one row. Invalid members are absent, never raw-text
fallbacks. The SSH alias supplies no identity, so an underscore alias works with
a valid configured name. Invalid configured names cause the entire entry to be
ignored: no polling or routing. The typed config-load diagnostic asks the
operator to rename `[[peers]].name`, for example from `Node A` to
`node-a.invalid`; the SSH alias can stay unchanged.
All other displayed rows require an admitted reported or parsed host key.
Invalid rows never render, including carried rows whose route was disabled at
admission. Admission records the typed refusal once; rendering does not log it.

The reserved `<home>` (`HOME_SWITCH_TARGET`) is a carried origin-summary control
route, not a hostname or SSH destination. It is exempt from fallback SSH-host
validation when the reported host is absent, and from SSH route validation.
Any nonempty reported host must still pass the strict rule. The sentinel never
creates a hostname key or authorizes rendering a keyless row.

An invalid raw host refuses a direct JSON summary, drops a relayed row, or
disables a carried binary route and excludes that row from display. Invalid
`via` refuses the batch. A content-free
`peer.summary.identifier_dropped` warning carries `invalid_params` and the fixed
field name only. Generic deserialization remains data-only. The mesh layout and
VERSION remain unchanged.


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
| 4 | untrusted prompt: system-owned preamble, control-byte refusal | landed (#348). `src/spawn/prompt.rs` composes the opening turn: flock's preamble, then the caller's text fenced under a tag minted per spawn, so the caller — whose text is fixed before the tag exists — cannot close the block and carry on in flock's voice. `AgentKind::argv` accepts only the composed type, which is what keeps the constraint off any one call site. Control sequences are refused rather than stripped (`src/control_bytes.rs`, one filter shared with `msg.send`'s strip policy), because the prompt is the whole turn and silently editing it runs a task nobody wrote. What the preamble may promise is bounded below |
| 5 | lineage-aware ceiling (depth → fanout → capacity) | landed |
| 6 | ceiling in the shared spawn funnel | landed (#349). `agent.spawn` and `agent.fork` both ask one App-side funnel, which gates a caller CLASS rather than a verb: a caller whose process ancestry lands in an agent's pane meets the three limits, an operator meets none of them. Agent-initiated fork children are stamped with `spawned_by`/`spawn_depth`, so they count and the two verbs cannot be alternated past the cap. The TUI keyboard fork never enters the funnel at all — deliberately, per the operator exemption. Note the fork half is NOT behind `[fleet] agent_spawn_enabled`: that switch opts a node into `agent.spawn`, and gating an already-shipped verb on it would leave the bypass open on every node that never set it |
| 7 | `fleet pause` refuses agent-initiated spawn | landed |
| 8 | refusals separate retryable from terminal | landed |

Every § is now built (#348 closed §4, #349 closed §6). The table stays because
an Accepted ADR whose gaps live only in prose is how they get forgotten — and
because the two rows above record what was decided while closing them, which
is not derivable from the code.

What #349 turned out to be about, worth keeping: the ceiling could not be
described by naming verbs. `agent.spawn` gated, `agent.fork` not, and the two
cost the fleet the same thing — so the cap held only for as long as an agent
reached for the verb that had it. What it gates now is who is asking, which is
the same line `fleet pause` draws in §7 and the only one that does not have to
be re-drawn every time a verb is added.

### What §4's preamble may promise

Mitigation, not a trust boundary. The preamble tells the child what it is
reading and raises the cost of an injection carried in relayed text; a model
can still be talked around, and §4 does not become a boundary by being
implemented. The P3 line — sender identity is routing and audit, never
authorization — applies to it exactly as it applies to a sender stamp, so
nothing downstream may treat a composed prompt as a safe one.

Two things it *does* promise, because both are properties of the construction
rather than of the child's judgement: the caller's text can never precede the
preamble, and the caller cannot close the fence, since the tag is minted after
its text is fixed. The boundaries that actually hold are the ones already
built — the closed `AgentKind` (§1), the environment allowlist (§3), the
lineage ceiling (§5), and the `Agent-Run:` trailer that keeps a talked-around
child's commits revertable.

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
