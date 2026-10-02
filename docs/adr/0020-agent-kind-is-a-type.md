# ADR 0020 — The spawn agent kind is a type, not an allowlist; it gains opencode, and a caller may never name a profile

- Status: Accepted
- Date: 2026-10-02
- Issues: #452, #453. **Amends ADR-0014** — §1's closed `AgentKind` enum and §3's
  scrubbed environment both stand unchanged; what changes is that the enum gains a
  second variant, and the reasoning behind its closure is recorded because it was
  widely misread as a Claude-only policy.
- Decision owner: operator. Raised by an operator question — "the old ADR says we
  just do claude for now, then it can be marked superseded?" — which turned out to
  identify a real documentation defect and no design change at all.

## Context

ADR-0014 §1 kept `AgentKind` closed, and that closure is load-bearing: a
caller-supplied `Vec<String>` argv is "one refactor away from being defeated,"
because a future tool that lands the same type inherits the escape hatch and
"a tool-table review will not catch it." §3 keeps the child's environment
scrubbed behind an allowlist, so a caller cannot hand a child a credential it
scraped.

Two issues then reported that flock "cannot launch opencode" over MCP, and that
`flock_agent_start` has no `env` parameter so callers must smuggle child
environment through argv via a generated bootstrap script. Both were filed in
good faith by an operator building a dispatcher, and both proposed the obvious
fix: widen the enum to free-form argv, and add an `env` map composing over the
inherited environment.

**Both proposals are refused here**, and neither is a narrowing of what those
issues asked for, because both are already satisfied by the design ADR-0014
shipped — they were blocked on a distinction nobody had written down.

The distinction: **the enum is a safety type, not a safety allowlist.** An
allowlist says *these binaries may run*. The closed enum says *the caller names
an agent, and flock assembles that agent's argv server-side from a reviewed
table*. #452 offers "document why the closed set exists (if it is a safety
allowlist, say so)" — it is not one, and that is exactly why adding a variant is
sound where accepting an argv is not.

The refusal to widen is not "claude for now." Nothing in ADR-0014 ever said
that, and the operator's instinct that the Claude-only-ness was provisional is
correct while the conclusion that it should be superseded is not, because the
Claude-only-ness was never the decision:

- the in-source reason for closure is the *shape* of the argument, not the count
  of agents — "a free string would let a caller select a **weaker-sandboxed
  profile**, or **name a binary outright**" (`src/spawn/mod.rs`), and the same
  comment says "adding an agent is a variant here plus its argv assembly";
- ADR-0014's own Context lists Claude-only-ness as a defect it exists to fix
  ("`branch_plan` refuses everything else with `ForkUnsupported`");
- the allowlist already anticipates a non-Claude kind as a supported state: "a
  kind with no entry is not an error: it gets the baseline, which is enough to
  start a process" (`src/spawn/allowlist.rs`);
- the 2026-09-08 amendment made `[spawn.env]` validate against the *detection*
  enum, which has carried opencode among its seventeen members all along.

## Decision

### 1. The enum is closed, and gains `AgentKind::OpenCode`

The spawn `AgentKind` is a type-level invariant and stays one. It gains a second
variant, `OpenCode`, with the same shape as `Claude`: a `parse` arm, a
`supported()` entry, and an `argv()` arm returning `["opencode", <prompt>]`.

This closes #452 and #453 without a schema change to either, because both
already derive from the enum. The MCP tool's `agent` parameter is generated as
`AgentKind::supported()` (`src/mcp/tools.rs`), so the schema widens itself. And
the environment half needs no change at all: `allowlist::for_argv` resolves the
child's agent from `argv[0]`, which means `[spawn.env.opencode]` begins to be
consulted the moment the argv says `opencode`. The env machinery is already
agent-generic — it keys off the seventeen-member detection enum, and its own
tests already exercise `Agent::Codex`.

So #453's bootstrap-script workaround disappears as a consequence, and its
`env` parameter stays refused: an `env` map composing over the scrub is the
escape hatch §3 declines.

### 2. No compiled credential keys for opencode initially

`CLAUDE_CREDENTIALS` is compiled into the allowlist for a stated reason: a Claude
profile directory holds stored OAuth credentials, so allowing the directory while
refusing the key would be incoherent.

Whether opencode keeps stored credentials somewhere analogous is not yet
established, and guessing is the failure mode §3 exists to prevent. So opencode
ships with **no** compiled credential keys. It receives the baseline plus whatever
`[spawn.env.opencode]` declares — which is the existing rule for a kind with no
entry, and fails toward "enough to start a process". If opencode turns out to
need compiled keys, that is an additive follow-up with its own evidence.

### 3. A caller may never name a profile

`AgentSpawnParams` has five fields and gains no sixth. There is no `profile`
parameter, and this is stated as a decision rather than left as an omission,
because the absence is load-bearing and the operator question made it explicit.

The child's profile is **read, not asserted**: `RequesterEnv::Attested` reads the
requester's live process, or flock's own record of the session being forked, via
a bounded process-ancestry walk. The rule is already written down for caller
class and applies here unchanged — "a caller-supplied identity would be a claim,
and depth is exactly the thing a runaway caller would want to lie about"
(`src/app/api/spawn.rs`).

Naming a *different* profile is available to an operator and unavailable to an
agent. It already exists for an operator: `flk agent start` is not scrubbed,
because it is the operator's own terminal, so
`CLAUDE_CONFIG_DIR=~/.claude-work flk agent start …` selects a profile today. An
operator choosing their own account is not a security event; an agent choosing one
is an escalation primitive, because ADR-0014 §4 establishes that a dispatcher's
prompts are derived from **GitHub issue bodies** — attacker-authored text. A
caller that could name a profile would move a child onto an account with more
access while the child's commits carried an `Agent-Run:` trailer attributing them
to the parent.

This extends to multi-profile, which needs no new mechanism: two agents on two
accounts differ by the selector their parent processes carry, and ancestry already
distinguishes them. The refusal cases beside it stay as they are — an unreadable
requester refuses rather than guessing "the default profile, i.e. possibly
someone else's account", and `RequesterEnv::Absent` stays deliberately *not* a
refusal, because there is no selector to get wrong.

### 4. `agent.start` and `agent.spawn` are not unified

An operator proposed collapsing both verbs into one allowlisted-command
primitive, on the reasoning that `<agent> <args>` is one use case of "launch a
program in a pane" — a real duplication, repeated across `flk agent start`,
`[[checks.script]]`, `flk pane run`, and the hooks in `assets/claude/`.

The observation is right and the merge is refused. ADR-0014's Consequences
already name this alternative and decline it: "one verb whose safety depends on
which caller reached it … is exactly the property that cannot be enforced in a
type." An allowlist enforced at the builder is the constraint-at-the-builder that
§1 says a future tool would inherit silently. And the two verbs read caller class
*oppositely on purpose* — a human typing `flk agent fork` proceeds unbounded,
while `agent.spawn` refuses an unattested caller, because there is no agent to
bill a ceiling to. The duplication is the price of safety not depending on the
caller, not an oversight to tidy.

The debt the observation names is real and is not dismissed by this decision; see
Deferred.

## Deferred

**The declared-agent set should become config-extensible.** Today adding a
harness is a code change and a release, which is the wrong tax for a fleet that is
explicitly mixed, and it is the one part of the operator's proposal worth keeping.

Direction recorded, deliberately not decided here: a `[spawn.agents.<name>]`
table holding a fixed `argv` template with **exactly one** substitution slot (the
prompt) plus that agent's `env_keys`, validated at config load, where an unknown
or malformed entry is a **diagnostic and not a parse error** — the posture the
2026-09-08 amendment already took for `[spawn.env]`, and the discriminated-union
shape §2 already blessed as `SpawnLocation`. No shell, no glob, no variable
expansion: that is what keeps it clear of §1's forbidden "config string that
reaches a shell."

It is deferred rather than recorded here because this ADR is immutable once
Accepted, and the schema is not designed yet — in particular how the table
interacts with `[spawn.env]` (one table feeding both the carry and the allowlist,
per the amendment, versus two) is unresolved. **One gap is already visible and
should be answered by that design:** `[spawn.env]` is keyed by agent name alone,
so a fleet cannot declare one key set per agent *and* per profile, and two
`claude` agents on two accounts are distinguishable only by ancestry. If the
declared table is keyed by name and profile, that gap closes.

## Consequences

**Gained.** The MCP surface can launch opencode, so a mixed fleet is
dispatchable over the tool surface and not only from an operator's shell.
`[spawn.env.opencode]` becomes declarable, which retires #453's generated
bootstrap script. And the enum's rationale is written down somewhere a reader
finds it, which is the actual defect #452 exposed.

**Unchanged, deliberately.** No caller-supplied argv. No caller-supplied
environment. No caller-supplied profile. `agent.start` stays operator-only and
off MCP; `agent.spawn` stays narrow. The preamble behind which the child's opening
turn is composed cannot be displaced, because the caller still never touches argv.

**Closed as fixed by a different route than requested.** #452 and #453 asked for
free-form argv and a composing `env` map. Both were refused. Both are satisfied
because the machinery they could not see was already agent-generic and waiting on
one enum variant — which is worth saying plainly in the closing comments, since
the reporter documented a limitation that was never the design's intent.
