# ADR 0034 — Delegates never ask the human: flk owns one policy point per harness hook, a delegate's question becomes a structured question to its architect, and only the root of a delegation tree reaches the operator

- Status: Proposed
- Date: 2026-10-12
- Issues: #1160 (the spike this records). Composes with ADR-0027 (delegation
  edges, #632), ADR-0028 (state axes §1, registered waits §5, channels §8,
  open questions §8a; slices E5 #1031, E6 #1032, E8 #1037, E9a #1038, E9b
  #1039, E9c #1040), ADR-0029 (exit codes), ADR-0030 (stop ladder rungs 3–5,
  #1035), ADR-0032 (compiled context), ADR-0014 (the operator authority
  boundary). Related: #1074 (event triggers and the hook and script surface),
  #503 (extension surface), #662 / #626 (hook install, sync and Codex hook
  trust), #605 / #1116 / #828 (trust dialogs), #1162 / #950 (injection into
  composers), #1023 (attention axis, landed).
- Decision owner: operator.
- Implemented: none. P1–P4, P6 and P7 below are re-runnable as
  `tests/probes/harness_question_hooks.py`, and all six passed on
  2026-10-12 against the versions recorded in the inventory. The slices
  (§10) are filed after acceptance.

## Context

The owner asked on 2026-10-12: *"we should also have all harnesses hooks
somewhat exposed to flk?! so we can e.g. omit any questions interfaces and
such of delegates — they shall ask their architect — and only the architect is
allowed to ask questions."*

### What happens today when a delegate asks

A Claude delegate that calls `AskUserQuestion` draws a choice dialog in a pane
nobody is watching. E1c (#1023, landed) reads it as `needs_human{question}`,
which is meant to page a human. The architect hears of it only through the
digest, and could answer it only by having keys typed into the delegate's
pane (E9b, unbuilt). So the one party that should answer, the architect, is
the last to know. The human is paged for a question the architect could have
answered from the brief. A Codex delegate calling `request_user_input` and an
opencode delegate calling `question` behave the same way.

### What flk installs today (verified at `d63ec5d6`)

| Harness | Where | Events flk registers | Asset version |
| --- | --- | --- | --- |
| Claude Code | `$CLAUDE_CONFIG_DIR/settings.json` (profile scope), stub `hooks/flock-agent-state.sh` | `SessionStart`, `UserPromptSubmit`, `Stop` (`src/integration/mod.rs:43-47`) | 10 |
| Codex | `$CODEX_HOME/hooks.json` plus `[features] hooks = true` and a trusted hash for flk's own entry in `config.toml` (`mod.rs:1529-1610`, `codex_hook_trust.rs:10`) | `SessionStart` only | 7 |
| opencode | `~/.config/opencode/plugins/flock-agent-state.js` (`mod.rs:1714`) | `event` (`session.created/updated/status`), `experimental.session.compacting` | 6 |

- Every stub is a no-op outside a flk pane. It exits unless `FLOCK_ENV=1`,
  `FLOCK_SOCKET_PATH` and `FLOCK_PANE_ID` are set
  (`src/integration/assets/claude/flock-agent-state.sh:18-20`), and it forwards to
  `flk hook <harness> <action>`. The hook body talks to the server with a
  500 ms socket deadline and swallows every error (`src/cli/hook.rs:41`),
  because a hook must never wedge its agent.
- `flk integration sync` (#662) reconciles installed assets and MCP launch
  paths. `verify_integration_manifest` (`mod.rs:1337`) diffs registrations for
  Claude only.
- **The role is already known at spawn.** `agent.start` and `agent.spawn`
  resolve the attested caller before they spawn the pane
  (`src/app/api/agents.rs:182`, `src/app/api/spawn.rs:203`), and record the
  `AgentDelegated` edge afterwards (#959, `spawn.rs:329`). `apply_pane_env`
  (`mod.rs:275`) is where the server stamps the pane's environment.
- **Spawn flags today.** `flk delegate start` adds `--ask-for-approval never
  --sandbox workspace-write` for Codex, and nothing for Claude or opencode
  (`src/cli/delegate.rs:592-620`). A restart preserves `--disallowedTools`,
  `--settings` and `--ask-for-approval` (`src/agent_restart.rs:233-300`).
- **Unbuilt neighbours this design needs.** ADR-0028's wait registry (E5),
  wake follows the wait (E6), and structured questions (E9a, E9b); ADR-0030's
  deterministic rungs (G2).

### Harness inventory, measured on a macOS (Apple Silicon) fleet host, 2026-10-12

Versions: **Claude Code 2.1.296**, **Codex CLI 0.160.1**, **opencode
1.18.35**, flk 1.2.0. Sources are the vendor docs (Claude:
<https://code.claude.com/docs/en/hooks>, `/permissions`; Codex:
<https://learn.chatgpt.com/docs/hooks> and its config reference; opencode: the
`v1.18.35` source tag and its `docs/plugins`, `docs/permissions`), plus the
probes marked **P1–P8** below. Each probe ran with the inherited `FLOCK_*`
variables removed, Claude inside a private tmux server, and touched no live
flk server.

**Hook events and what each can decide:**

| flk policy point | Claude Code 2.1.296 | Codex 0.160.1 | opencode 1.18.35 |
| --- | --- | --- | --- |
| `session.start` | `SessionStart` (matchers startup / resume / clear / compact / fork); `additionalContext` | `SessionStart` (startup / resume / clear / compact); `additionalContext` | `event` `session.created` |
| `prompt.submit` | `UserPromptSubmit`; block, `additionalContext` | `UserPromptSubmit`; block | `chat.message`; rewrite parts |
| `tool.before` | `PreToolUse`: `permissionDecision` allow / deny / ask / defer, `updatedInput`, `additionalContext`; exit 2 blocks | `PreToolUse`: deny with reason, or allow plus `updatedInput`; exit 2 blocks (P6) | `tool.execute.before`: a throw aborts the call, mutating `output.args` rewrites it |
| `tool.after` | `PostToolUse`, `PostToolUseFailure`; replace output, `additionalContext` | `PostToolUse`; replace the result | `tool.execute.after`; rewrite the result |
| `permission.request` | `PermissionRequest`: `decision.behavior` allow / deny (exit 2 not honoured) | `PermissionRequest`: `decision.behavior` allow / deny; no decision shows the prompt | no hook: the `permission.ask` type exists but nothing calls it in 1.18.35. A plugin sees `permission.asked` on the bus and can answer through `client.permission.reply` (what `opencode run` does; UNVERIFIED from a plugin) |
| `question` | `AskUserQuestion` through `PreToolUse` (P1) | `request_user_input` through `PreToolUse` (P7) | `question` tool; `question.asked` on the bus, answered with `Question.reply` (UNVERIFIED from a plugin) |
| `elicitation` | `Elicitation`: accept / decline / cancel plus content | no hook; `mcp_elicitations` and `tool_call_mcp_elicitation` are config | not inventoried |
| `notification` | `Notification` (permission_prompt, idle_prompt, elicitation_dialog, agent_needs_input, …); observe only | legacy `notify` program, turn-complete only | `event` bus |
| `stop` | `Stop`: `decision: block` continues the turn | `Stop`: `decision: block` continues with the reason as a prompt | `session.idle` on the bus; cannot block |
| `compact.before` | `PreCompact` (blockable) | `PreCompact` | `experimental.session.compacting` (adds context) |
| `session.end` | `SessionEnd` (1.5 s budget) | `SessionEnd` (1 s default, 3 s max) | `session.deleted` |

**Payloads.** Claude's `PreToolUse` stdin carries `session_id`,
`transcript_path`, `cwd`, `permission_mode`, `tool_name`, `tool_input` and
`tool_use_id` (P1). Codex's carries the same names plus `turn_id` and `model`
(P6, P7). opencode passes `{tool, sessionID, callID}` and `{args}` to an
in-process function.

**The three question tools share one shape.** Measured `tool_input`:

```text
Claude  AskUserQuestion    {questions: [{question, header, options: [{label, description}], multiSelect}], answers?}   (P1)
Codex   request_user_input {questions: [{id, header, question, options: [{label, description}]}]}                    (P7)
opencode question          {questions: [{question, header, options: [{label, description}], multiple?}]}              (v1.18.35 tool/question.ts)
```

All three map onto ADR-0028 §8a's `question` payload without loss.

**Probes:**

- **P1, Claude: a hook can answer the question.** A `PreToolUse` hook on
  `AskUserQuestion` returned `permissionDecision: allow` and `updatedInput`
  with `answers: {"Which color?": "red"}`. No dialog was drawn. The model
  received `User answered Claude's questions: · Which color? → red` and
  replied `red`. The docs state that allow without `answers` is not enough,
  and that `ExitPlanMode` (plan approval) is answered the same way.
- **P2, Claude: removing the tool works.** With `--disallowedTools
  AskUserQuestion`, an interactive session reported the tool absent and drew
  no dialog. (In `-p` mode the tool is absent anyway, so a `-p` A/B proves
  nothing.)
- **P3, Claude: a hook that times out fails open.** A `PreToolUse` hook with
  `timeout: 5` that slept for 15 s let the call through, and the dialog was
  drawn.
- **P4, Claude: `onFailure: "block"` fails closed.** The same hook with
  `"onFailure": "block"` (2.1.295 and later) blocked the call with
  `PreToolUse:AskUserQuestion hook error: [sleep 15]: timed out; blocking
  because onFailure is "block"`, and the model reported the error.
- **P5, all three: hooks inherit the spawn environment.** A variable set when
  the harness was spawned was visible in a Claude hook, a Codex hook and an
  opencode plugin's `process.env`. opencode was probed through `opencode run`
  only. Its TUI is UNVERIFIED, although flk's own plugin already depends on
  `FLOCK_*` in TUI panes.
- **P6, Codex: a deny reaches the model.** A `PreToolUse` deny on the shell
  tool returned to the model as `Command blocked by PreToolUse hook:
  <reason>`, which it quoted verbatim.
- **P7, Codex: `request_user_input` is live in default mode and hookable.**
  The `default_mode_request_user_input` feature reads `false`, yet in a
  default-mode `codex exec` the model called `request_user_input`. The run had
  `approval: never`, set by the host's `config.toml` (the value flk's
  `--ask-for-approval never` sets), and `danger-full-access`, not flk's
  `workspace-write`. `PreToolUse` received it with the structured
  `questions` above, and a deny returned the reason to the model. So an
  approval policy of `never`, which flk's delegate start sets, does **not**
  stop Codex from asking. Q5 re-checks this under flk's exact argv.
- **P8, opencode: removing the tool works only through permissions.**
  `OPENCODE_PERMISSION='{"question":"deny"}'` and `permission.question:
  "deny"` in `OPENCODE_CONFIG_CONTENT` both drop the tool
  (`opencode debug agent build`). `tools: {question: false}` does **not**,
  when the base config has `permission: "allow"` (this host's), because the
  legacy map is merged under explicit permissions.

**Other facts the design relies on:**

- **Where hooks come from.** Claude merges hooks from user, project, local,
  managed, plugin and `--settings` sources, and runs them all. A
  `--settings` JSON hook ran for one session (probe). Interactive Claude holds
  back every settings-file hook until the folder is trusted. Codex runs
  hooks from `$CODEX_HOME` and from trusted projects, skips unmanaged hooks
  until their hash is trusted, and accepts per-run `-c hooks.…`. opencode
  loads plugins from the global and project `plugin(s)/` directories,
  in-process under Bun.
- **Timeouts.** Claude command hooks default to 600 s, and Codex hooks
  default to 600 s. opencode awaits plugin hooks with no timeout of its own
  (`plugin/index.ts:291-295`), so a plugin must bound itself.
- **Managed lockout.** Claude's managed `allowManagedHooksOnly` and Codex's
  managed `allow_managed_hooks_only` disable user hooks entirely. On such a
  machine no flk hook runs.

## Decision

### 1. One policy point per harness hook, decided by the server

flk names one cross-harness vocabulary of **policy points** (the first column
of the inventory table): `session.start`, `prompt.submit`, `tool.before`,
`tool.after`, `permission.request`, `question`, `elicitation`,
`notification`, `stop`, `compact.before` and `session.end`. Each harness
adapter maps its native events onto them. The table is part of the contract,
and a harness that lacks a point says so rather than emulating it.

- **The decision is the server's.** A hook calls `flk hook <harness>
  <point>`, which sends one new socket request:

  ```text
  hook.decide {
    point,                  // a policy point above
    harness,                // claude | codex | opencode
    session_id?, tool_name?, tool_input?,   // bounded, as the harness sent them
    hold_ms,                // how long this hook may wait for an answer
  }
  → { verdict: pass
             | deny   { reason }
             | answer { answers, reason }      // the question was answered
             | defer  { question_id, reason }  // the question is open; end the turn
    }
  ```

  The pane is attested by the same ancestry walk `msg.send` uses (ADR-0014).
  It is never taken from the request.
- **flk wires a point only when a policy or a state axis consumes it.** This
  ADR consumes `question`, `permission.request` and `elicitation`, plus
  `tool.after` on the question tools only if the owner accepts the §9
  amendment to ADR-0028 §8a (owner question 6; slice Q10).
  `session.start`, `prompt.submit`, `stop` and `compact.before` are already
  wired for state and hand-off. Nothing installs a catch-all `tool.before`.
  The question entries use a narrow matcher (`AskUserQuestion|ExitPlanMode`
  for Claude, `request_user_input` for Codex), so an ordinary tool call
  spawns no process and pays nothing. A `flk hook` call outside a pane exits
  in about 10 ms (measured, five runs of `flk hook claude session`).
- **Hooks stay at profile scope**, installed and reconciled by the existing
  `flk integration install` / `sync`. They do not go into per-session
  `--settings` (see Alternatives). The stub's pane gate is unchanged, so an
  operator's own sessions outside flk never reach the server.
- **Decisions are durable; raw hook traffic is not.** A routed, answered,
  escalated or denied question writes a durable event (§3). Every other hook
  call stays a pane report, as today. The event log does not grow with tool
  calls.
- **No user code runs in the hot path.** Scripts that react to hook points
  belong to #1074 and #503. Once those are decided, the durable events from
  this ADR are their read-only input, and nothing here gives a script a vote
  inside a hook.

### 2. Question policy is an attribute of the agent, defaulted from its edge

Each agent record carries `questions: ask_human | route | deny`:

| Agent | Default | Meaning |
| --- | --- | --- |
| no parent edge (operator-started, the root architect) | `ask_human` | as today; the harness shows its dialog |
| live parent edge (ADR-0027 §1) | `route` | questions go to the architect as structured questions (§3) |
| `deny` (explicit only) | — | questions are refused with a reason telling the agent to proceed without asking; for headless or cron-started agents with nobody to route to |

- **Set at start, keyed by `AgentId`.** `agent.start`, `agent.spawn` and
  `agent.fork` compute the default from the edge they are about to record, and
  accept an explicit `questions` parameter (`flk agent start --questions …`,
  `flk delegate start --questions …`). The attribute survives a restart, a
  resume and a hibernation exactly as the edge does.
- **Only narrowing is open to agents.** An architect may start its delegate
  with `deny` instead of `route`. Widening a delegate to `ask_human` is an
  operator verb, refused for an agent caller with exit 13 (ADR-0029). That is
  the ADR-0014 boundary: an agent cannot hand another agent a direct line to
  the human. A delegate cannot change its own policy.
- **Reassign and orphan follow the edge.** After `flk delegate reassign`,
  routed questions go to the new architect. An orphaned edge (ADR-0027 §1a)
  routes to the **operator**: a notification-log record, as ADR-0027 §1a
  already does for orphaned digest lines, and an entry in `flk question
  list`. Posting it to channels as well is E9c's business, not this ADR's.
  Nothing is left waiting on an architect that no longer exists.
- **Questions already open follow the edge too.** On reassign, the server
  re-addresses every open question of the moved edges to the new architect as
  a new message with `readdressed_from` set. The delegate's wait stays on the
  original `question_id`, and the first answer through either message wins.
  On orphan, open questions are re-addressed to the operator the same way.
- **The env hint.** At spawn the server also stamps `FLOCK_QUESTIONS=route`
  or `=deny` into the pane (`apply_pane_env`). It is never authority. Its
  only use is the fail-closed decision in §4 when the server cannot answer.
  A delegate that edits its own environment can at most show a dialog to
  nobody, which is what happens today.

### 3. Mechanism: intercept the question, convert it into a question to the architect, and answer it in the same tool call

Recommended: option **(c)**, built on the denial path of **(b)**, with **(a)**
rejected as the primary mechanism (see Alternatives).

**The flow for a `route` agent:**

1. The delegate calls its question tool. The adapter's `question` hook sends
   `hook.decide { point: question, tool_input, hold_ms }`.
2. The server creates a **message-sourced question** (ADR-0028 §8a) from the
   delegate to its architect: intent `needs_reply`, the prompt, options,
   `multi_select` and `allow_other` copied from `tool_input`, and a
   `question_id` equal to the message's correlation id. It records
   `agent.question_routed { question_id, from, to, harness, tool }` and
   registers a wait for the delegate on `reply:<question_id>` (E5).
3. `needs_reply` mail wakes an idle architect (ADR-0018, ADR-0028 §5). The
   architect answers with `flk question answer <id> --option …` or MCP
   `flock_question_answer` (E9b), or with an ordinary `flk msg reply`.
4. **If the answer arrives within `hold_ms`**, the server returns `answer`,
   and the adapter returns it in the harness's own form:
   - **Claude:** `permissionDecision: allow` with `updatedInput.answers` (P1).
     The model receives a normal tool result. An `additionalContext` line
     names the architect who answered.
   - **Codex:** `request_user_input`'s input has no `answers` field (P7), so
     an `updatedInput` rewrite has nowhere to carry the answer. The hook
     denies with the answer as the reason instead: `Answered by your
     architect <name> (question <id>): "<label>". Continue with this answer.`
     P6 shows that the reason reaches the model verbatim. Slice Q5 must show
     that the model then continues with the answer.
   - **opencode:** the plugin answers through `Question.reply` on the
     `question.asked` event if slice Q6's first step proves this works from a
     plugin. Otherwise it throws in `tool.execute.before` with the answer as
     the message, the same as Codex. What the model sees on a throw is
     UNVERIFIED and is part of Q6's first step.
5. **If not**, the server returns `defer`, and the adapter refuses the call
   with: `Your question <id> is with your architect <name>. Do not ask the
   human. End your turn now; flk will wake you with the answer (read it with
   flock_msg_read).` The delegate ends its turn, and its state reads
   `waiting_on_event` with the wait on the question (§5). When the answer
   arrives as a `msg.reply`, the delegate is woken under E6's
   wake-follows-the-wait rule.

**The hold budget.** The default is `[questions] hold_ms = 120000`. The hook
entry is installed with `timeout` = hold + 30 s and, on Claude, `onFailure:
"block"` (P4). A hook killed for any reason then blocks the call instead of
falling through to a dialog nobody sees (P3). 120 s covers an idle
architect's wake, at 261–498 ms per ADR-0028, plus a turn of thought. It also
stays well inside the 5-minute prompt-cache window, so a held turn does not
pay a cold cache when it resumes. Longer waits take the `defer` path, which
costs nothing while it waits.

**Other question surfaces that a `route` agent has:**

- **Plan approval** (`ExitPlanMode` on Claude) is routed the same way, since
  it is a question with two options.
- **Third-party "ask a human" MCP tools** (for example a Telegram ask tool)
  are listed in `[questions] human_tools`, with no entries by default. For a
  `route` agent the hook denies them with the routing reason, pointing at
  `flock_question_ask`. flk cannot know their schemas, so it does not convert
  them.
- **An explicit ask.** A new `question.ask { prompt, options?, multi_select?,
  allow_other?, hold_ms? }` (CLI `flk question ask`, MCP
  `flock_question_ask`) is the same server path without a hook. It routes to
  the caller's architect, returns `answered` or `pending{question_id}`, and
  registers the wait. A harness or version without the hook uses it, briefs
  and the compiled delegate context (ADR-0032) teach it, and every deny
  reason points at it. Called by a root (`ask_human`), it opens a question
  to the operator, which is ADR-0028 §8a's `needs_reply` addressed to the
  operator.

**For `ask_human` and `deny` agents.** `ask_human` returns `pass`: the
harness shows its dialog, exactly as today. `deny` returns `deny` with the
reason `Questions are disabled for this agent. Choose the most conservative
option, state the assumption in your result, or end your turn with a blocked
report.`

### 4. The role comes from the server, and an unreachable or older server never wedges a delegate

The agent record (§2) is the source of truth. `FLOCK_QUESTIONS` is consulted
only when the server cannot decide:

| Situation | Verdict | Why |
| --- | --- | --- |
| server answers `hook.decide` | the server's verdict | it holds the edge and the policy |
| server unreachable, no reply within the question deadline (hold + 5 s; the 500 ms deadline of the other hook calls does not apply to questions), or too old for `hook.decide` (`server_version_gap`), and the hint is `route` or `deny` | `deny`, with the fallback reason: `flk could not reach your architect. Do not ask the human. Choose the most conservative option, state the assumption in your result, or end your turn with a blocked report.` | **fail closed for the question tool only.** Failing open draws a dialog in a pane nobody watches, which is the wedge itself: the agent sits in `needs_human` until a human happens to look. A denial returns control to the model at once. |
| server unreachable or too old, and no hint (a root, or a pane spawned by a pre-ADR server) | `pass` | the agent is not known to be a delegate, so today's behaviour is the safe one |
| the hook itself dies, or the harness kills it | Claude: blocked by `onFailure: "block"` (P4). Codex and opencode: the adapter's own deadline is shorter than the harness timeout, so it answers `deny` first | a killed hook must never fall through to a dialog |

**A deliberate exception to ADR-0030 §7** ("an old server never makes a new
hook block"). The hinted rows above refuse a question tool when the server
is unreachable, silent, or older (the last happens only after a downgrade
handoff, since only a server that knows `hook.decide` stamps the hint). The
exception is narrow on purpose: it covers only question points and only
hinted panes. Refusing a dialog nobody will answer does not stop the agent's
work, while passing it would. Goal verdicts and every other hook keep
ADR-0030 §7's rule. Owner question 8 asks for this exception explicitly.

Only question points fail closed. Every other point fails open, as every flk
hook does today (`src/cli/hook.rs`): a broken flk never stops a delegate from
working. A Claude older than 2.1.295 has no `onFailure`. There the
installer omits the key, `flk integration status` reports `question hook:
fails open on timeout (claude < 2.1.295)`, and the adapter's internal deadline
(hold + 5 s) is what keeps the hook inside the timeout.

### 5. A routed question parks the delegate, not the human, and it never deadlocks the architect

- **The delegate.** While a routed question is open, the delegate's attention
  axis stays `none`, not `needs_human{question}`. Its `waiting` axis is `on {
  predicate: reply:<question_id> }` (ADR-0028 §5's `reply:<corr>`, since the
  question id is the message's correlation id), and the server knows the
  addressee from the message. While the hook holds, the turn is
  `working`. After `defer`, the turn is `idle` and the phase is
  `waiting_on_event` (ADR-0028 §1). No `agent.needs_human` event is emitted,
  so no operator notification or channel post fires. On the goal ladder the
  delegate is at rung 4: it is parked silently and the judge is not called
  (ADR-0030 §2).
- **The architect.** It receives a `needs_reply` question from `delegate:<name>`
  with options. `delegate.list` shows the edge as `question open (<age>)`,
  and the digest gains a `question` line kind. It is woken because the mail
  is `needs_reply`, not because of the digest.
- **Amendment to ADR-0030 §2, rungs 4 and 5.** Rung 4 does not apply to the
  architect while its inbox holds an unanswered `needs_reply` question. That
  covers a park on `any-delegate-of-mine changed` and every other registered
  wait. Rung 5 does not count a delegate whose open wait is a question
  addressed to this architect, even though its phase is `waiting_on_event`.
  Without both clauses, an architect whose goal parks on its delegates and a
  delegate waiting on that architect would wait on each other silently, and
  the stop falls through to the later rungs instead. Owner question 9.
- **Escalation is explicit and keeps the link.** The addressee can answer, or
  call `question.escalate { question_id, note? }` (CLI `flk question
  escalate`, MCP `flock_question_escalate`). That opens a new question to the
  addressee's own parent, with `escalated_from` set and the original prompt
  and options plus a `from <delegate> via <architect>` line. A root has no
  parent, so its escalation goes to the operator (`flk question list`, the
  notification log, channels under E9c). The first answer to the escalated
  question answers the whole chain. The server delivers it down, so the
  original delegate is woken with it, and each hop's architect gets an `fyi`
  digest line saying who answered. A nested architect is itself a delegate
  under `route`, so its questions climb one edge at a time and only the root
  reaches the operator. The root architect may still simply ask through its
  own question tool, which `ask_human` allows. Escalation is recommended
  because it keeps the answer linked and delivered.
- **Deadlines.** Each routed question's wait carries `[questions]
  wait_deadline` (default 2 h). When it expires, the delegate is woken with
  `no answer from <architect> within 2h`, plus the conservative-choice
  sentence, and the question becomes `expired` (ADR-0028 §8a). The architect
  gets a digest line.

### 6. Permission prompts and dialogs: what the architect may answer, and what must reach a human

| Prompt inside a delegate | Who answers | How |
| --- | --- | --- |
| Tool permission for an action inside the delegate's own worktree and sandbox (Claude `PermissionRequest`, Codex `PermissionRequest`, opencode `permission.asked`) | **architect** | routed as a question with options `allow once` / `deny`. The adapter answers with the harness's allow or deny (Claude and Codex through `decision.behavior`, opencode through `client.permission.reply`, to be verified in Q8) |
| Plan approval (`ExitPlanMode`) | **architect** | §3 |
| MCP elicitation, form mode, with no credential field | **architect** | Claude's `Elicitation` hook accepts with content. On Codex and opencode it is a human dialog until a hook exists. Q8 disables form elicitation for `route` agents where the config allows it (Codex `mcp_elicitations`) |
| A permission that **widens** the delegate's authority: Codex sandbox escalation, opencode `external_directory`, Claude additional directories, network access the profile restricts | **human**, unless the operator lists the class in `[questions] architect_may_grant` (empty by default) | routed to the architect as a question with the single option `escalate`. The architect can only pass it up (§5). An agent widening another agent's authority beyond what the operator configured is the ADR-0014 boundary |
| Trust dialogs: Claude folder trust (including #1116's variant with pre-approved permissions), Codex hook review (#626), Codex folder trust (#828) | **human, never routed** | trusting a folder or hook means consenting to run its hooks and settings, which is code execution. flk keeps pre-trusting only what it created and owns (#605, `codex_hook_trust.rs`), and otherwise refuses the start with a named refusal (`startup_dialogs`). A trust dialog that appears anyway is `needs_human{trust}` and pages the operator |
| Login, auth and credential prompts, and URL-mode (OAuth) elicitations | **human** | `needs_human{login}` as today |
| Quota and limit screens | nobody | `blocked{provider_limit}` (ADR-0028), not a question |

### 7. Security: hooks are code execution, so flk installs only its own

- **What flk installs:** per harness, one stub whose bytes are compiled
  into the binary (`include_str!`), the registrations for the points it uses,
  and for Codex the trusted hash of **its own entries only**, as #626
  established. It never trusts or edits an entry it did not write. The stub
  runs `$FLOCK_BIN`, which the server stamps into the pane. If a delegate
  repoints `FLOCK_BIN`, it affects only that delegate.
- **Verification:** `verify_integration_manifest` today checks Claude
  registrations only. It extends to Codex `hooks.json` entries, Codex trust
  hashes and the opencode plugin. Every installed stub is compared byte for
  byte with the embedded asset, not only by its version marker.
  `flk integration status` lists, per harness, which policy points are wired
  and whether the question point fails closed (§4).
- **Operator opt-in:** installation stays an explicit `flk integration
  install` / `sync`, as today. Routing itself is on by default for agents
  with an edge once the hook is installed (owner question 1). `[questions]
  enabled = false` turns every verdict into `pass`. A managed policy that
  disables user hooks disables all of this, and `flk integration status`
  reports it.
- **Relation to #503.** This ADR adds no third-party code path. It adds a
  built-in policy and a normalized, read-only event vocabulary that #503's
  extension surface and #1074's triggers can consume once they are decided.
  It deliberately does not answer #503's questions (signing, sandboxing,
  timeouts for third-party scripts).
- **Not a security boundary.** Question routing governs cooperative agents.
  A delegate that defeats it (by editing its own environment, killing its
  hook, or typing into a human channel it was given) gains nothing beyond
  today's behaviour. The security boundaries remain ADR-0014's operator verbs
  and ADR-0027's teardown rules.

### 8. Skew

- **New wire surface.** `hook.decide`, `question.ask` and `question.escalate`
  are new methods, gated by #868. The `questions` field on agent records and
  the `agent.question_routed` / `agent.question_escalated` kinds are additive.
  A new persisted kind first passes ADR-0028 §3a's reader check.
- **Hook newer than server:** `server_version_gap` from `hook.decide` follows
  §4's table: `pass` without a hint, `deny` with one. The hint exists only if
  the pane was spawned by a server that knows `hook.decide`, so a mixed host
  stays consistent.
- **Server newer than hook (an outdated stub):** no `question` entry is
  installed, so the dialog draws as today and the delegate reads
  `needs_human{question}`. `flk integration status` reports `outdated`, and
  `sync` fixes it.
- **Harness versions:** the adapters key on behaviour measured at the
  versions above. `tests/probes/harness_question_hooks.py` is re-run on every
  harness bump the fleet pins. Claude below 2.1.295 is covered by §4.
- **Mesh:** questions are per host. A delegate started over `ssh` has no edge
  (ADR-0027 §8) and so defaults to `ask_human`. Until mesh-attested edges
  exist, a remote start should pass `--questions deny` (owner question 5).
- **Protocol VERSION stays 5** for the mesh, because nothing here is relayed.

### 9. Amendments to accepted ADRs (each needs the owner's acceptance)

- **ADR-0028 §8a, sources:** the hook becomes the primary source of the
  structured `question` payload on every harness where it is installed. It
  sees the exact options, which a screen scrape can only reconstruct. Screen
  detection stays the fallback (owner question 6). For `ask_human` agents
  the hook observes and returns `pass`; `PostToolUse` reports the answer, so
  `agent.question_answered` gets a structured source too.
- **ADR-0030 §2, rungs 4 and 5:** the exception in §5 (owner question 9).
- **ADR-0032, delegate layer:** one line telling delegates that questions go
  to the architect through the question tool or `flock_question_ask` (Q9).
- **ADR-0027 §3:** the digest gains a `question` line kind (`<delegate>
  asks: <prompt> [<n> options]`).

### 10. Slices

Each slice is ≤ 400 lines of production code (doc comments and tests
excluded), independently shippable, and states its skew.

| # | Slice | Depends on | Acceptance | Skew |
| --- | --- | --- | --- | --- |
| Q1 | `questions` policy on the agent record, defaulted from the edge at start / spawn / fork; explicit `--questions` on `agent start` and `delegate start`; narrowing-only for agent callers; `FLOCK_QUESTIONS` stamped in `apply_pane_env`; shown in `agent.get` and `delegate.list` | #959 (landed) | An agent started by an agent reads `route` and its pane env has `FLOCK_QUESTIONS=route`. An operator start reads `ask_human` with no hint. A restart keeps the value. An agent widening a delegate to `ask_human` is refused with exit 13 | Field only. The client sends `questions` only to a server that advertises it, and says so otherwise |
| Q2 | `hook.decide` with verdicts `pass` / `deny` from the policy and §4's failure table; Claude `PreToolUse` entry on `AskUserQuestion\|ExitPlanMode` (timeout, `onFailure: block` where ≥ 2.1.295); `[questions] human_tools` deny; `CLAUDE_INTEGRATION_VERSION` bump. The `route` verdict is `deny` with "state the question and options in your final message and end your turn", until Q4 | Q1 | Isolated flk with a throwaway Claude delegate: `AskUserQuestion` draws no dialog and the model gets the reason. A root agent's dialog draws as today. With the server stopped, a hinted delegate is denied with the fallback text and an unhinted agent passes. No process spawns for a non-question tool | New method, gated. The old-server path is §4 |
| Q3 | `question.ask` (CLI and MCP): message-sourced question to the caller's architect with options, `hold_ms` long-poll, wait registered, orphaned edge routed to the operator; `agent.question_routed` | Q1, E9a's `options` on `msg.send` (#1038), E5 (#1031), E6 (#1032) | A fake architect answering within hold returns `answered`. No answer returns `pending` and the delegate reads `waiting_on_event`, not `needs_human`. A later answer wakes it (E6, #1032). An orphaned delegate's question lands in `flk question list` for the operator | New method, gated, exit 78 against an old server |
| Q4 | Claude `answer` and `defer` verdicts: hold, `updatedInput.answers`, the `defer` reason, `[questions] hold_ms` and `wait_deadline` | Q2, Q3, E9b (#1039), E6 (#1032) | Sandbox probe: a delegate asks; `flk question answer <id> --option 2` from the architect inside the hold returns option 2 to the model, with no dialog. After the hold, the delegate ends its turn and is woken with the answer. Multi-select and Other both round-trip | Fields only beyond Q2 |
| Q5 | Codex adapter: `PreToolUse` on `request_user_input` with answer-in-reason and `defer`; `hooks.json` entry and its trusted hash; `CODEX_INTEGRATION_VERSION` bump | Q2, Q3 | Probe: a Codex delegate's `request_user_input` is answered by the architect, and the model continues with the answer (the open point of §3). The trust hash covers only flk's entries | Codex re-reviews hooks whose hash changed. The installer re-trusts flk's own entries, as #626 does |
| Q6 | opencode adapter. Step 1: prove `Question.reply` and `client.permission.reply` from a plugin, and record what the model sees when `tool.execute.before` throws (the branch taken is recorded in the PR). Then `question.asked` → `hook.decide` → reply, or a `tool.execute.before` throw with the answer; every await bounded; `OPENCODE_INTEGRATION_VERSION` bump | Q2, Q3 | Probe in an opencode TUI pane: the `question` tool is answered by the architect. A stalled flk never holds the plugin past its bound | Plugin only |
| Q7 | Architect side: `question.escalate` with chained answers; digest `question` line; `delegate.list` column; the rung-5 exception in G2's ladder | Q3, E9b, #961, G2 (#1035) | A delegate → architect → operator chain: the operator's answer reaches the delegate, and the architect is told who answered. An architect parked on two delegates is not parked while one has a question addressed to it | New method, gated |
| Q8 | Permission requests and elicitations by §6's table: Claude `PermissionRequest` and `Elicitation`, Codex `PermissionRequest`, opencode `permission.asked`; `[questions] architect_may_grant`; form elicitation off for `route` Codex delegates | Q4, Q5, Q6 | One case per table row: in-worktree permissions are answered by the architect; a widening permission reaches the operator only through escalation; a trust dialog is never routed | Additive |
| Q9 | Verification and teaching: byte-exact manifest verification for the Codex and opencode stubs, entries and trust hashes; `flk integration status` policy-point listing; `docs/next` pages; the architect skill and brief template; one line in ADR-0032's delegate layer saying questions go to the architect through the question tool or `flock_question_ask` | Q2, Q5, Q6 | `flk integration status` flags a hand-edited stub and a missing question entry. A delegate's compiled context carries the line | Client and docs only |
| Q10 | (only if owner question 6 is accepted) The hook as the primary question source: for `ask_human` agents `hook.decide` observes the question tool (records `agent.question_opened` with exact options) and returns `pass`; a `tool.after` entry on the question tools reports the answer as `agent.question_answered`; screen detection stays the fallback | Q2, E9a (#1038) | A root agent's AskUserQuestion yields `agent.question_opened` with the exact options before the dialog draws, and the answer from `PostToolUse`. With the hook removed, the screen detector still opens the question | New kinds share E9a's; reader check per ADR-0028 §3a |

Order: Q1 → Q2 can land now, before the ADR-0028 slices, and already ends
"dialog in a pane nobody watches" on Claude. Q3 waits on E9a's message half and
E5. Q4–Q6 follow Q3 in any order. Q7 follows E9b and G2. Q8 and Q9 close it
out, and Q10 rides on E9a if the owner accepts it. E9a's screen half is not on this path, because routed questions are
message-sourced.

## Consequences

- A delegate's question reaches the one party that can answer it from the
  brief, with its options intact, and the answer returns into the same tool
  call when the architect is quick. The operator is paged only by roots, by
  explicit escalation, by orphaned edges and by §6's human-only classes.
- `needs_human` again means a human is needed. Supervisors, channels and
  notifications stop firing for questions an agent can answer.
- flk now ships policy into each harness's hook path. That is new
  responsibility: three adapters to keep current, measured by a probe script
  instead of being assumed.
- Codex delegates gain a hook entry, and with it a hash re-review whenever the
  entry changes. The installer absorbs that for flk's own entries.
- An architect has a new kind of work: answering. Its digest and inbox carry
  it, and rung 5 stops it from mistaking an owed answer for parking.

## Alternatives considered

- **(a) Disable the question tools at spawn as the mechanism**
  (`--disallowedTools AskUserQuestion`, P2; `OPENCODE_PERMISSION`, P8).
  Reliable where it works, but:
  - it routes nothing: the model, robbed of its structured tool, writes the
    question into prose and stops, so the architect parses free text and the
    options are lost;
  - Codex has no equivalent: the feature flag reads off, yet the tool is live
    (P7);
  - it needs an argv or env rewrite on every spawn path (`agent.start` takes
    a raw argv; restart, fork and TUI starts each differ), and it is frozen
    at spawn, so a reassign or an orphan cannot change it;
  - for opencode it would overwrite the user's own `OPENCODE_PERMISSION`.

  Kept only as the operator's explicit `deny` choice where they prefer the
  tool gone. Rejected as the mechanism.
- **(b) Deny alone, pointing at `flk question ask`.** Reliable on all three
  harnesses (P1, P6, opencode's throw), but every question costs a second
  model step and depends on the model following the redirect. Kept as the
  floor: the `defer`, fallback and `human_tools` paths, and Q2's interim
  behaviour. Not the end state.
- **Per-session `--settings` hooks only for delegates.** Claude would load
  them (probe, `-p` mode), but they need an argv rewrite on every spawn path,
  and they have no Codex or opencode equivalent of the same shape. In
  interactive Claude they probably also wait on folder trust: the docs say
  every settings-file hook does, and `--settings` specifically is
  UNVERIFIED. Profile-scope hooks with a server-side role
  cover every start path, including restarts and TUI starts, and follow
  reassigns.
- **Role from the env alone.** Spoofable, frozen at spawn, and blind to
  reassign and orphan. Kept as a hint for §4 only.
- **Fail open everywhere** (the existing hook rule). For a question tool,
  failing open is the wedge: a dialog to nobody. Rejected for question points
  only.
- **Answer delegates' on-screen dialogs by typing (E9b's screen path) instead
  of intercepting.** It works after the fact, races the composer (#1162,
  #950), and still pages a human through `needs_human`. Intercepting before
  the dialog draws avoids all three.
- **Automatic escalation to the operator after a timeout.** It would let a
  slow architect leak delegates' questions to the human. The explicit
  `escalate`, plus a deadline that wakes the delegate rather than the human,
  keeps "only the architect asks".
- **A catch-all `tool.before` hook forwarding every tool call to the server.**
  It costs a process spawn and a socket round trip per tool call for no policy
  that needs it today. Points are wired when a policy consumes them.

## Risks

- **Harness drift.** A Claude, Codex or opencode release can change
  `updatedInput.answers`, deny semantics or the plugin API. Mitigation: the
  probe script, version-keyed adapter behaviour, and `flk integration status`
  reporting a degraded point instead of silently failing open.
- **Codex's answer arrives as a "blocked" message.** The model may treat it as
  an error rather than an answer. Q5's acceptance tests exactly this. If it
  fails, Codex stays on `defer` plus `flock_question_ask`, and the ADR is
  amended with the measurement.
- **A held turn looks like `working`.** For up to `hold_ms` a supervisor sees
  a working delegate with `waiting: on{question}`. The waiting axis is what
  distinguishes the two.
- **Architect load.** A fleet of delegates can queue many questions. They
  coalesce into the digest's `question` lines, and E9b's `flk question list`
  shows them in one view.
- **Managed hook lockout** disables routing on such machines. It is reported,
  not worked around.

## Test strategy

- **Harness facts:** `tests/probes/harness_question_hooks.py` (P1–P4, P6, P7)
  on every pinned harness bump.
- **Policy and failure table:** unit tests over `hook.decide`'s planner as a
  pure function of policy, hint and server reachability, in the style of
  `src/cli/hook.rs`'s `plan` tests. One case per row of §4.
- **End to end:** a sandbox runbook in `tests/probes/` (isolated flk, short
  `XDG_CONFIG_HOME`, `FLOCK_*` unset) that starts a throwaway Claude delegate
  from a fake architect, asks, answers through `flk question answer`, and
  asserts no dialog was drawn, the model got the answer, and no
  `agent.needs_human` was emitted. The same runbook covers `defer` and the
  wake. It covers Codex and opencode in Q5 and Q6.
- **Rung 5 exception:** a ladder table case in G2's tests.

## Open questions for the owner

Each has a recommendation. Accepting the ADR accepts the recommended answer
unless the owner says otherwise.

1. **Default for delegates once the hook is installed.** Recommended: `route`
   by default, with `[questions] enabled = false` as the off switch.
   Alternative: opt-in per architect, which leaves today's unwatched dialogs
   as the default.
2. **Orphaned delegates' questions.** Recommended: route to the operator
   (§2), consistent with ADR-0027 §1a. Alternative: `deny`, which keeps the
   operator out but leaves the delegate guessing.
3. **Hold and deadline defaults.** Recommended: `hold_ms` 120 s (inside the
   prompt-cache window) and `wait_deadline` 2 h.
4. **May an architect grant authority-widening permissions?** Recommended:
   no by default, with classes opted in through `[questions]
   architect_may_grant` (§6).
5. **Remote delegates started over `ssh`.** Recommended: architects pass
   `--questions deny` until mesh-attested edges exist (ADR-0027 §8). The
   architect skill says so.
6. **Hook as the primary question source for every agent** (the §9 amendment
   to ADR-0028 §8a). Recommended: yes. It gives E9a exact options for roots
   too, and leaves screen detection as the fallback.
7. **Escalation verb vs a free re-ask.** Recommended: ship
   `question.escalate` (§5), so an operator's answer flows down the chain
   automatically. The root architect's own question tool stays allowed.
8. **The fail-closed exception to ADR-0030 §7** (§4). Recommended: accept
   it for question points on hinted panes only. Alternative: fail open
   there too, which shows the dialog to nobody whenever the server is down.
9. **The ADR-0030 rung 4 and 5 amendment** (§5). Recommended: accept both
   clauses, so an owed answer is never mistaken for parking.
