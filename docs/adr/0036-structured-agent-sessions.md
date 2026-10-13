# ADR 0036 — An agent can run as a structured session: flk owns its process and its one input queue, translates its typed event stream into flk's state, questions and waits, serves every frontend one wire contract, and forks it at any message boundary

- Status: Proposed
- Date: 2026-10-13
- Issues: #1299 (the spike this records, with the owner decisions of
  2026-10-13). Composes with ADR-0005 / ADR-0028 (the durable event log, seq
  cursors, §3a unknown kinds, the state axes of §1, waits of §5, channel
  tokens of §8, questions of §8a), ADR-0034 (`hook.decide`, the question
  policy, routing to the architect), ADR-0010 (`flk report`), ADR-0019 (push
  delivery into a Claude session), ADR-0029 (exit codes, the JSON error
  envelope), ADR-0032 (compiled context), ADR-0014 (the operator authority
  boundary), ADR-0035 §6.3 and #1177 / O19a #1208 (agents run as the separate
  `flk-agent` user, attested by uid), ADR-0026 (mesh delivery, which
  superseded the delivery contracts of ADR-0008, ADR-0018 and ADR-0019),
  ADR-0001 (the web bridge hosting rules), #868 (protocol gating). Designs
  #1301 (non-blocking questions, §2a) and the turn-end half of #697
  (self-compact and its continuation, §3a); records the owner decision of
  #1300 for unknown-event logging (§8). Extends `agent.fork` /
  `agent.lineage` (`flock_agent_fork`, `flock_agent_lineage`) rather than
  adding verbs.
- Follow-ups: #1307 (slice tracking), #1305 (cache tests 4 and 6), #1308
  (TUI parity gaps P16, P17), #1309 (live Codex and opencode runs).
- Decision owner: operator.
- Evidence: Appendix A was measured on 2026-10-13 against Claude Code
  2.1.296 (this host's `latest` channel; the fleet `stable` pin is 2.1.287,
  which carries the same hidden flags, see A.4), codex-cli 0.160.1 and
  opencode 1.18.35, in throwaway sessions under a scratch working directory
  with every `FLOCK_*` and `CLAUDE_CODE_*` variable removed. No flk server was
  started, reached or restarted. Total spend: about 1.6M tokens, 76% of them
  cache reads (A.9).

## Context

flk drives every agent through a terminal. Input reaches an agent through
several doors that do not know about each other: the operator typing into the
pane, `flk agent send`, `msg.send` / `msg.reply` delivery, channel replies,
and hook injections such as the Stop-hook recap. Output is a screen that the
detector (ADR-0028 §1, `src/detect/`) reads back into state. That costs
ordering and attribution of inputs, invites TTY races (the paste-trap class),
ships repaints rather than meaning to remote clients, makes transcripts hard
to query, and lets a fork start only from the latest point
(`agent.fork` today is `claude --resume <sid> --fork-session` in a new
worktree, `src/agent_resume.rs:187`, `src/app/api/worktrees.rs:227`).

`claude -p --input-format stream-json --output-format stream-json` gives a
typed event stream out and exclusive ownership of stdin in. The owner decided
on 2026-10-13 (#1299):

1. The structured session surface and runtime live fully in flk,
   independent of any private client.
2. One backend, several frontends: one frontend-neutral wire contract that
   the Rust TUI and a Bun/TypeScript web frontend shipped in flock both
   consume; TS types are generated from the Rust schema and drift fails CI.
3. The parser never drops or crashes on an event it does not know: it passes
   it through as a raw `unknown` event that every frontend can show as a
   collapsible JSON dump.
4. Each unknown shape is recorded once (kind, harness, version, redacted
   sample) and can be turned into a bug report through `flk report`, which a
   human submits (ADR-0010).

This record proposes the design and the slices. It changes no code.

## What the probes established

Each line cites Appendix A. Items the issue marked "verify" are settled here.

- **E1 Event model.** One `claude -p` process emits, per turn:
  `command_lifecycle` (queued, started, completed, cancelled; keyed by the
  input message's `uuid`), `system/init` (once per turn, not once per
  process), `system/status` (`requesting`, `compacting`), `stream_event`
  (raw Anthropic SSE: `message_start` … `message_stop`, deltas `text_delta`,
  `thinking_delta`, `signature_delta`, `input_json_delta`), `assistant` (one
  per content block), `user` (tool results; with `--replay-user-messages`
  also the replayed input), `system/thinking_tokens`, `rate_limit_event`,
  `system/permission_denied`, `system/compact_boundary`,
  `system/hook_started` / `hook_response` (with `--include-hook-events`),
  `control_request` / `control_response`, and `result`. A turn ends with
  exactly one `result`. (A.2, A.3)
- **E2 Truthful status comes from events, never the exit code.** A clean
  stdin close after an interrupted turn exits 1. An unknown model exits 1
  with a `result` whose `subtype` is `success` but `is_error` is true,
  `terminal_reason: "api_error"`, `api_error_status: 404`. An interrupted
  turn ends `subtype: "error_during_execution"`,
  `terminal_reason: "aborted_streaming"`. (A.5)
- **E3 Input identity and acknowledgement exist.** A `uuid` supplied on a
  stream-json `user` line is kept: it is the `command_uuid` of the
  `command_lifecycle` events, the replayed message's `uuid`, the
  `user_message_uuids` of `system/status` and `message_start`, and the
  transcript entry's `uuid`. flk can make it the idempotency key. (A.5)
- **E4 A message written mid-turn is folded into the running turn** at the
  next model step: it reports `queued`, then `started` while the first input
  is still running, and both inputs complete under one `result`. Holding a
  message until `result` is therefore flk's decision, not the harness's. (A.5)
- **E5 Hooks leak, and isolation works without a private config dir.** A
  default `-p` session runs the fleet's user-level SessionStart,
  UserPromptSubmit and Stop hooks (two each, observed as `hook_started` /
  `hook_response`). `--safe-mode` runs none but also ignores `--mcp-config`.
  `--setting-sources local` drops the user hooks while hooks passed through
  `--settings` still run; claude.ai connectors still attach unless
  `--strict-mcp-config` is given. (A.1)
- **E6 An isolated `CLAUDE_CONFIG_DIR` loses the login on macOS.** OAuth
  lives in the keychain under a service named for the first eight hex digits
  of sha256(config dir), so a fresh dir prints `Not logged in · Please run
  /login` and exits 1. Copying the entry would share a rotating refresh token
  between two dirs, which can log the original out. (A.1)
- **E7 `AskUserQuestion` is available in `-p`, but only when a host answers
  prompts.** With `--permission-prompt-tool stdio` the tool is listed in
  `system/init` and arrives as `control_request{subtype: can_use_tool,
  tool_name: AskUserQuestion, requires_user_interaction: true}`; the host
  answers with `updatedInput.answers` and the model continues with them.
  Without a host it is absent, even from deferred tool search. The issue's
  premise "AskUserQuestion is not available in -p" holds only for a session
  nobody answers. (A.6)
- **E8 Permissions round-trip over stdio or MCP.** Both
  `--permission-prompt-tool stdio` (a `can_use_tool` control request with
  `permission_suggestions`) and an MCP tool (`--permission-prompt-tool
  mcp__<server>__<tool>`) deny with flk's message, and the denial lands in
  `result.permission_denials`. With no host, anything that would prompt is
  denied automatically. Read-only shell commands such as `cat` never prompt.
  (A.6)
- **E9 A resume-at-message option exists.** `--resume-session-at <message
  uuid>` is a hidden CLI flag (in 2.1.287 and 2.1.296; the CLI form of the
  Agent SDK's `resumeSessionAt`), requires `--resume`, keeps the transcript
  up to and including that message, and composes with `--fork-session`. The
  original transcript is not touched. Stream `uuid`s of `assistant` and
  tool-result `user` events are the transcript's uuids, so a client can name
  a fork point straight from the stream. (A.4, A.7)
- **E10 Boundary forks read the cache.** Forks at the latest point, at a turn
  boundary, and between two tool calls of one turn read 99.3–99.7% of their
  input from cache. Ten concurrent forks of one warm point each read 100%.
  (A.7)
- **E11 A fork cannot reach behind the last compaction through the CLI.** The
  loader starts at the latest `compact_boundary` and keeps only the preserved
  messages; `--resume-session-at` a pre-compaction uuid that was preserved
  resumes the compacted context (7.3k tokens, not 40k). A copy of the
  transcript's byte prefix ending before the boundary, resumed with
  `--fork-session`, restores the full pre-compaction context with a 99.6%
  cache read. (A.7)
- **E12 Resume pins the tool schemas; tool changes do not invalidate the
  cache.** A fork with `--tools` reduced to `Bash` read 100% of the prefix;
  the model still saw `Read` and called it, and Claude Code refused it
  client-side ("No such tool available: Read. Read is disabled for this
  session"). Adding an MCP server on resume, or removing one, kept the hit
  (98.3–99.6%), and the added tool was callable. The issue's expectation "MCP
  server added or removed, `--tools` reduced: miss" does not hold at
  2.1.296. (A.7)
- **E13 The date is a recorded message, not part of the system prompt.**
  "Today's date is …" is a `<system-reminder>` attachment in the transcript,
  and the binary carries a "The date has changed …" attachment for a later
  date. That a fork across a day boundary therefore keeps its cache is the
  expectation, not yet measured (A.7.2, #1305). (A.7)
- **E14 Hot reload is cheap without compaction and resets the prefix with
  it.** Resuming a 41k-token session with a new MCP server read 98.9% from
  cache (first-token time 2.26 s). After `/compact` (40,032 → 1,237 tokens)
  the first turn read 0 and wrote 7,815. The `/compact` turn reports zero
  usage in `result.usage`; its cost (13.9k uncached input, 1.4k output)
  shows only in `modelUsage`. (A.3)
- **E15 Concurrent cold writes are not deduplicated.** Ten concurrent fresh
  sessions with identical new content each wrote 13,808 cache tokens; an
  eleventh, after them, read it all. Subscription sessions write the 1-hour
  cache (`ephemeral_1h_input_tokens`), which Anthropic prices at twice the
  base input rate (5-minute writes 1.25×, reads 0.1×; "Prompt caching",
  docs.claude.com/en/docs/build-with-claude/prompt-caching, pricing table).
  (A.7)
- **E16 A second input door exists.** Every `-p` process opens a messaging
  socket under `/tmp/cc-socks/` and reports it as
  `system/init.messaging_socket_path`. (A.2)
- **E17 Bytes.** Over all probe runs, tool results were 41.7% of stdout
  bytes, partial deltas 28.8%, `system/init` (repeated every turn) 10.6%,
  complete `assistant` events 7.8%. (A.8)
- **E18 Harness neutrality.** Codex (`app-server`, JSON-RPC, thread / turn /
  item) and opencode (`serve`, HTTP + SSE, session / message / part) carry
  every concept flk needs, including approvals, user questions, interrupt,
  resume, fork at a turn or message, compaction and cache token counts, but
  only in their server modes; their one-shot JSON modes (`codex exec
  --json`, `opencode run --format json`) stream no deltas and have no
  approval path. Approvals, user questions, errors, interrupts and reasoning
  of the server modes were read from their generated schemas only, not run
  live (#1309). (A.10)

## Decision

### 1. The agent-session runtime: `pty` or `structured`, per agent

An agent record gains `runtime: pty | structured`. `pty` is today's
behaviour and stays the default. `structured` is opt-in per start
(`flk agent start --runtime structured`, the MCP and socket equivalents) and
first ships for Claude Code only.

A structured agent is a child process of the flk server: `claude -p
--input-format stream-json --output-format stream-json --verbose
--include-partial-messages --replay-user-messages --include-hook-events
--permission-prompt-tool stdio --setting-sources local --strict-mcp-config
--settings <flk file> --mcp-config <flk file> --session-id <uuid>` (or
`--resume <id>`), started as the `flk-agent` user (§1a) with that user's
private config dir for the agent's profile as `CLAUDE_CONFIG_DIR`, and the
compiled context of ADR-0032 passed through `--append-system-prompt-file`
and `--add-dir`. The server owns stdin and stdout exclusively.

- **Isolation and login (E5, E6, ADR-0035 §6.3 point 1).** A structured
  agent never runs as the operator's uid. Its `CLAUDE_CONFIG_DIR` is a
  private dir per profile in the `flk-agent` user's `HOME`
  (`~flk-agent/.flk/claude/<profile>`), and that dir holds its own login:
  `claude /login` run once per profile as `flk-agent` (the token lands in
  `flk-agent`'s own keychain on macOS, or in the dir's credentials file on
  Linux), or a `claude setup-token` token written into that dir's settings
  as `CLAUDE_CODE_OAUTH_TOKEN` where no browser login is possible. The
  operator's config dir and keychain are never read or copied: sharing them
  would share a rotating refresh token between two dirs (E6). Inside that
  dir the session is still isolated by flags: `--setting-sources local` (no
  user-level hooks, permissions or plugins), `--strict-mcp-config` (no
  claude.ai connectors arriving late and shifting the tool set) and flk's
  own `--settings` and `--mcp-config`, so nothing that lands in the dir by
  accident changes the session. Because user-level permission rules are
  dropped, flk passes the agent's permission policy explicitly. The fleet
  Stop hook never runs in a structured session; what it did there is done
  by the server on `result` (§3a). The probes ran in the operator's profile
  only because O19a is not live yet; the flag behaviour of E5 does not
  depend on the user.
- **One process per session id.** The server holds a session lease (agent id,
  session id, pid, start time) in its state and refuses a second spawn,
  resume or fork-in-place for a leased id with `session_busy` (a new
  `ErrorCode`, retry class `Later`). Forks always take a new id
  (`--fork-session`), so they never contend.
- **Truthful status (E2).** The turn's outcome is the `result` event: `ok`
  when `is_error` is false; otherwise `api_error{status}`,
  `aborted{reason}` or `failed{errors}`. The exit code is recorded and shown
  but never decides the outcome. A process that dies without a `result` for
  a started input is `crashed`, and the input is re-queued as `held` (§3),
  never silently retried.
- **Stall and waiting.** `waiting_on_human` is exact: a `can_use_tool`
  control request is open. Working is `system/status: requesting` or an
  open `message_start`. A stall is a started input with no stdout event for
  `[sessions] stall_ms` while no control request and no tool call is open;
  a long-running tool is not a stall (its `tool_use` is open). A stall is
  reported as `agent.session_stalled{since}` and leaves the phase as it is
  (ADR-0028 §1 derivation is unchanged); it is an observation, not a timeout
  that kills anything.
- **The messaging socket (E16)** is a door flk does not own. Probe slice S0
  must find a supported way to disable it or bind it to the server; until
  then a structured session is not started (open question 5).
- **Hot reload.** On a config change (MCP server, skill, settings) the
  server waits for idle (no started input, no open control request), closes
  stdin, waits for exit, and respawns with `--resume <id>` and the new
  config. The first `system/init` of the new process is diffed against the
  last one of the old (tools, MCP servers, skills, plugins, slash commands,
  model, permission mode) and recorded as `agent.session_reloaded{diff}`;
  the diff is also queued to the agent as a `system` item so that it knows
  which tools appeared or went (E12: a removed tool's schema can still be
  visible and is refused when called). Compaction before a reload is not
  automatic: it costs a full uncached pass and resets the prefix (E14), so it
  is a separate operator choice. A fleet-wide reload is staged: one agent
  first, the rest after its first turn succeeds; agents that share a prefix
  reload together so they warm one entry (E15 shows simultaneous cold writes
  are paid per process, so "together" means one first, then the rest).

### 1a. Identity and the operator boundary for structured agents

`caller_is_local_operator()` (`src/app/api/mesh.rs:182`) treats a local
caller with no flk pane in its process ancestry as the operator. A
structured agent has no pane, so its tool calls (`flk msg send`, `flk
question answer`, any `flk` it runs through Bash) would pass. And a
WebSocket over TCP carries no peer pid at all. Three rules close that:

- **Structured agents require O19a (#1208).** A structured agent is started
  only on a host where `flk-agent` exists; elsewhere `agent.start --runtime
  structured` is refused with `structured_needs_agent_user` (a new
  `ErrorCode`, retry class `AfterHuman`). The server reads the peer uid on
  its socket (`getpeereid` / `SO_PEERCRED`, ADR-0035 §6.3); a `flk-agent`
  caller is never the operator, whatever its ancestry, so the operator-only
  verbs of ADR-0014 and ADR-0035 are refused with `operator_only` (exit 13).
  `caller_is_local_operator()` gains that uid check first; a detached
  (`setsid`) child of a structured agent is still `flk-agent` and still
  refused.
- **Which agent is calling.** The session lease (§1) records the process's
  pid and start time. A socket caller whose ancestry reaches a leased pid
  (with a matching `process_start_time`, so pid reuse cannot impersonate)
  is that agent: it is the `msg.send` sender and the `agent:<id>` producer
  of §3, exactly as a pane id attributes a `pty` agent today (ADR-0008,
  with ADR-0026's authenticated origin). A `flk-agent` caller that reaches
  no lease and no pane is an anonymous agent: it may read, and every send is
  refused with `msg_not_allowed`.
- **WebSocket clients always present a token** (§5). Loopback is not an
  identity: the operator's own browser is a client with an `operator`-scoped
  token that only the local operator can mint over the unix socket, and the
  `principal` in `welcome` names the token, never the address.

### 2. Mapping session events onto flk state, questions and waits

A new `StateSource::Session` joins `Screen`, `Hook`, `Plugin`,
`ProcessTree` and `WaitRegistry` (`src/api/agent_state.rs:169`). For a
structured agent it is authoritative for the turn and attention axes; the
screen detector is not run.

| Session event | flk effect |
|---|---|
| `command_lifecycle{started}` | queue item `delivered`; `Turn::Working` |
| `system/status{requesting}`, `stream_event/message_start` | `Turn::Working`; `ttft_ms` recorded |
| `system/status{compacting}`, `system/compact_boundary` | `agent.compacted{pre_tokens, post_tokens, trigger}` |
| `control_request{can_use_tool, tool_name: AskUserQuestion}` | a question record (§2a), `source: session`, options from `input.questions`; answered at once if the policy allows, otherwise deferred so the turn continues (§2a); routed per ADR-0034 §2 |
| `control_request{can_use_tool}` (any other tool) | `hook.decide{point: permission.request}` (ADR-0034 §1); `pass` answers allow, `deny` answers deny with the reason; `defer` holds the request open and makes it a question |
| `control_request{other subtype}` | answered with an error and logged as an unknown shape (§8) |
| `system/permission_denied`, `result.permission_denials` | `agent.permission_denied{tool, reason}` |
| `rate_limit_event{status != allowed}` | `Attention::Blocked{ProviderLimit, retry_after_ms}` from `resetsAt` |
| `result` | queue items `done`; `Turn::Idle`; usage recorded; outcome per §1 |
| process exit without `result` | `Lifecycle` unchanged, `Attention::Blocked{crashed}` |
| anything unknown | §8 |

Phase derivation is ADR-0028 §1's unchanged; only the inputs differ. A
question answered while its control request is still open becomes the
`control_response` (`behavior: allow`, `updatedInput` with `answers`), so the
one answer verb of §8a works for structured agents too. Registered waits
(ADR-0028 §5) wake a structured agent by queueing a `wake` item (§3) instead
of typing into a pane.

### 2a. Questions do not block (#1301)

A question is a record, not a state the turn waits in.

- **Record.** Every ask becomes a question record in the agent's stream:
  `question.asked { question_id, source: session | recap | message,
  ask_point: message_uuid, prompt, options, multi_select, allow_other }`.
  `ask_point` is the boundary the question was asked at: for an
  `AskUserQuestion` call, the tool-result message that closes it (§9's cut
  rules allow a fork there); for a question the E9 parsers (#1038, #1243)
  find in a turn's final text or `※ recap … Next: …` line, the final
  `assistant` message of that turn. Later states are appended as
  `question.answered { question_id, by, answer, via: in_place | fork }`,
  `question.forked { question_id, fork_agent }` and `reply.delivered {
  question_id, item_id }`. The ADR-0028 §8a kinds `agent.question_opened` /
  `agent.question_closed` are emitted to the global log as today, so
  `flk question list` and the TUI and web questions views read one source.
- **The turn does not wait.** An `AskUserQuestion` control request is
  answered at once by the gate when the question policy (ADR-0034 §2) can
  answer it (a routed architect answering within `[questions] hold_ms`, as
  ADR-0034 §3 does). Otherwise flk answers the control request with
  `behavior: deny` and the message "Recorded as question <id>; the answer
  will arrive as a message. Continue with work that does not depend on
  it." The tool call ends, the turn continues, and the agent is
  `working` or `finished`, never `waiting_on_human` for a question. This is
  ADR-0034's `defer` verdict applied to a structured session. A permission
  request is different: allowing a tool call later is not possible, so a
  `can_use_tool` for any other tool stays open (it does block) until
  `hook.decide` answers or the operator does; that is the only
  `waiting_on_human` a structured agent has. An agent that really cannot
  go on asks through ADR-0034 §3's explicit `flock_question_ask` with
  `blocking: true`; that call is held open as a permission is.
- **Answer in place.** `question.answer { question_id, answer }` on a
  deferred question queues one `reply` item (`anchor: question_id`,
  `delivery: at_idle`) for the agent's next turn boundary and records
  `question.answered{via: in_place}`.
- **Answer in a fork.** `question.fork { question_id, at?: ask_point |
  latest }` forks the agent at `ask_point` (default) or its current point
  through §9, as a fork with `return_to_parent` and `answers:
  question_id`. The fork is where the operator or a supervisor discusses
  and explores; the main agent keeps working and its transcript never sees
  the discussion. The fork ends with `question.resolve { question_id,
  answer }` (an MCP tool in the fork, or the operator), which records
  `question.answered{via: fork}` and queues the one `reply` item to the
  main agent with `producer: fork:<id>` and the fork id cited in the body.
- **Exactly one reply.** The question's states are `open → (forked →)
  answered`, and `reply.delivered` is recorded when that item's
  `command_lifecycle` reports `started`. A second answer, in place or from
  another fork, is refused with `question_closed` (exit 13, ADR-0028 §8a);
  more than one fork of a question may exist, and the first to resolve
  wins.
- **Unknown question shapes** go through §8 like every other unknown event.

### 3. One server-owned input queue per agent

Every input to a structured agent is an item in that agent's queue, which is
an event-log stream (§4) named `queue/<agent>`:

```text
{ item_id (uuid; becomes the stream-json message uuid and command_uuid),
  producer: human | agent:<id> | channel:<binding>/<user> | system:<source> | fork:<agent>,
  kind: prompt | message | reply | delegate_result | wake | reload_notice | finding,
  anchor?: { correlation_id | question_id | fork_point },
  priority: normal | urgent,
  delivery: at_idle | steer | interrupt,
  state: queued | held | stashed | delivered | done | cancelled | dropped,
  (queued → delivered → done | cancelled; queued → held | stashed | dropped;
   held | stashed → queued; delivered → held when the process dies without a result)
  body, attachments: [blob refs] }
```

- **One gate.** Each item passes `queue.gate` once: `send` (deliver now per
  `delivery`), `hold` (until a human releases it), `stash` (keep, never
  deliver unless moved) or `drop`. The gate is the policy point; ADR-0035's
  rows and ADR-0034's question policy plug in here.
- **Delivery (E4).** `at_idle` writes the item only after the current
  `result`; `steer` writes it at once and the harness folds it into the
  running turn; `interrupt` sends `control_request{interrupt}` first and the
  item after the cancelled `result`. Defaults: a human prompt is `steer`,
  everything else `at_idle`, `urgent` maps to `steer`. The operator can
  reorder, edit or drop items that are still `queued`.
- **Acknowledgement (E3).** `command_lifecycle` drives the item state, so
  "delivered" means the harness started it, not that bytes were written.
- **Ported producers.** `msg.send` / `msg.reply` delivery to a structured
  agent becomes a queue item instead of the inbox nudge and knock order
  (ADR-0026, which superseded the delivery contracts of ADR-0008, ADR-0018
  and ADR-0019; custody and the authenticated origin stay as ADR-0026
  defines them). A delegate's structured result and receipt become a
  `delegate_result` item for its parent. The Stop-hook recap and nudge
  become server work on `result` (§3a).
- **Storage.** The queue persists as `queue.jsonl` beside the agent's
  session streams (§4).

### 3a. Turn-end work keys on `result` and goes through the queue (#697)

#697's diagnosis comment (2026-10-13) found flk's own self-compact path
sound: an armed self-compact fires at turn end and its continuation is
delivered with a background shell, a held wait or a background subagent
live. The misses came from an external, client-side supervisor that polled
`agent_status` every few minutes and missed short idle windows between
back-to-back turns. For a structured agent there is no polling at all:

- **Trigger.** The turn end is the `result` event (E1). An armed self-compact
  (`flock_self_compact`, `flk pane arm-self-compact`) queues a `system`
  item with body `/compact`, `delivery: at_idle`, `priority: urgent`; the
  queue writes it right after that `result`, before any other `at_idle`
  item. Background shells, held waits and subagents do not matter, because
  nothing reads a pane status.
- **Continuation.** The post-compact continuation (the goal re-inject and
  status of #998, or a supervisor's prompt) is a second item anchored to the
  first; it becomes deliverable when the compaction's own `result` arrives
  (after `system/compact_boundary`) and is written then. A supervisor that
  wants to know uses the stream (`queue/<agent>` or the `events` stream),
  not `agent_status` polling.
- **Recap and wakes.** On every `result` the server lifts the recap from the
  final `assistant` text and wakes parked waits, which is what the Stop hook
  did for a `pty` agent.

### 4. Storage

- **Per-agent session stream.** Every stdout line of a structured agent is
  appended, with a per-stream `seq` and a receive timestamp, to
  `<data dir>/sessions/<agent>/<session>.jsonl`. This is the lossless copy;
  `event-log.jsonl` (ADR-0005) keeps only the cross-agent facts of §2 so its
  readers and its rotation stay as they are.
- **The queue.** `queue/<agent>` persists as
  `<data dir>/sessions/<agent>/queue.jsonl`: one line per item change (added,
  gated, edited, reordered, state change), appended before the change takes
  effect, and folded into the in-memory queue at server start (as ADR-0031
  folds goal state). The question records of §2a are lines in the same
  file. When the file passes `[sessions] queue_rewrite_bytes` the server
  writes a snapshot of the folded queue to a temporary file and renames it
  over the old one; `seq` continues across the rewrite.
- **Two kinds of seq.** ADR-0005's `seq` stays the one total order of the
  durable event log, and the `events` wire stream uses it. A per-agent
  stream (`agent/<id>`, `queue/<id>`) has its own `seq`, starting at 1,
  dense and strictly increasing within that stream only; it is a cursor for
  that stream and orders nothing across streams. Facts that other agents or
  supervisors act on (a question opened or closed, an item delivered, a
  compaction, a phase change) are also appended to the global log, carrying
  `{stream, stream_seq}` so a reader can jump from the global order into the
  per-agent stream. The global log is the only place where two agents'
  events are ordered against each other.
- **Blobs by hash.** A tool result, attachment or thinking block over
  `[sessions] blob_min_bytes` (default 2 KiB) is stored once under
  `<data dir>/blobs/<sha256>` and the stored line keeps `{blob: sha256,
  size, preview}` in its place. Partial deltas (`stream_event`) are relayed
  live and not persisted; the complete `assistant` event that follows carries
  the same content.
- **Rehydrate.** The native transcript Claude writes under the profile is a
  disposable working copy. A byte prefix of it (E11) is the only thing flk
  ever writes back, under a new session uuid, and only for a fork behind a
  compaction (§9). flk never edits a transcript line.
- **Not in this cut** (open question 10): sealed zstd segments, SQLite for
  hot state, a DuckDB query layer, and a thinking retention policy.

### 5. The client wire contract

One endpoint serves every frontend: `GET /v1/ws` (WebSocket) and
`GET /v1/blob/<sha256>` (HTTP). It runs inside the flk server process,
because the streams, the queue and the blobs are server state. It reuses the
axum stack of the `web` feature (`src/web/mod.rs`), not the `flk web`
subcommand, which stays the terminal bridge it is (one pty client per
socket). It is off by default, binds loopback by default, and follows
ADR-0001's hosting rules (`--allow-non-loopback` explicit, refused under
`tailscale funnel`, same-origin check on upgrade).

Messages are JSON text frames, one object each, tagged by `t`:

```text
client → server
  hello   { protocol, client: { name, version }, token? }
  sub     { sub_id, stream, after? }          stream ∈ agent/<id>, agent/<id>/live,
                                              queue/<id>, events, terminal/<pane>
  unsub   { sub_id }
  cmd     { cmd_id, verb, args }              cmd_id is the idempotency key
server → client
  welcome { protocol, server_version, capabilities, principal }
  ev      { sub_id, stream, seq, ts_ms, ev }  ev is the typed event (§6)
  gap     { sub_id, from, to }                as events.read (ADR-0028 §3)
  lagged  { sub_id, skipped_to }
  ack     { cmd_id, ok, result? | error{ code, message, exit, retry } }
```

- **Cursors.** Every stream except `agent/<id>/live` is a log with `seq`.
  `sub` with `after` replays from the cursor, then goes live. Reconnect,
  sleep and a second device are all "subscribe from my last seq". Delivery
  is at-least-once; clients dedupe on `(stream, seq)`. `agent/<id>/live`
  carries partial deltas only, has no cursor, and is sent only to
  subscribers that asked for it.
- **Commands.** `cmd` verbs are the socket API's own (`queue.add`,
  `queue.update`, `queue.gate`, `agent.interrupt`, `question.answer`,
  `agent.fork`, `agent.reload`, …), with the same params types. The server
  keeps `cmd_id`s for `[wire] idempotency_window_ms` (default 24 h); a
  repeat returns the first `ack`. Errors use ADR-0029 §4's envelope and
  `ErrorCode`s from `src/api/error_code.rs`.
- **Blobs.** `GET /v1/blob/<sha256>` is immutable: `Cache-Control: public,
  max-age=31536000, immutable`, `ETag` = the hash, range requests honoured.
  A client renders a large tool result from `preview` and fetches it only on
  demand (E17).
- **Protocol gating (#868).** The JSON wire has its own version,
  `WIRE_JSON_VERSION`, independent of the framed protocol's
  `PROTOCOL_VERSION`, bumped once per release that adds a verb or an event
  type. `hello.protocol` is compared with it. A client newer than the server gets
  `welcome` with the server's protocol and read-only capabilities; a `cmd`
  whose verb the server does not know is refused with `server_version_gap`
  (exit 78) before anything is allocated. A server never sends an `ev` whose
  type the client's protocol predates without the §8 unknown wrapper.
- **Auth.** Every client presents a bearer token in `hello`; the address it
  connects from grants nothing (§1a). The operator's own browser gets an
  `operator`-scoped token through `flk client open`, which runs over the
  unix socket (so `caller_is_local_operator()` and the uid check of §1a
  apply), mints a one-time code, and opens the client with it; the client
  exchanges the code for its session token once. Tokens follow the channel-token model of ADR-0028 §8
  (`src/channels.rs`): 32 random bytes as 64 hex digits, one file per client
  under `<data dir>/client-tokens/<name>.token`, mode 0600 in a 0700 dir,
  compared as SHA-256 digests in constant time, never logged, revoked by
  deleting the file. Each token carries scopes `read`, `send`, `answer`,
  `operate` (`operate` is what the operator's own client holds). Only the
  local operator (`caller_is_local_operator()` after §1a's uid check) can
  issue or revoke one (`flk client token add|rm|list`; else
  `operator_only`, exit 13), and verbs that are operator-only under ADR-0014's
  authority boundary (and ADR-0035's owner rules) stay operator-only
  whatever the token says unless it is `operate`-scoped. Token files live in
  the operator's data dir, which `flk-agent` cannot read (ADR-0035 §6.3), so
  structured agents cannot pick up a client token. `pty` agents that still
  run as the operator's uid until O19a can; that limit is #1177's.

### 6. One schema, generated TypeScript

The Rust types are the schema: the session event enum (§8), the wire
messages of §5 and every `cmd` params and result type. They derive `ts_rs::TS`
behind a `ts` cargo feature, and a test (`wire_ts_types_are_current`)
renders them and compares with the checked-in `web/client/src/gen/*.ts`; a
difference fails the test and therefore CI, and `just gen-ts` rewrites the
files. No hand-written TS type may describe a wire shape.

### 7. The Bun/TypeScript reference client

- **Layout.** `web/client/` is a Bun workspace: `src/gen/` (generated, §6),
  `src/wire.ts` (connect, hello, subscribe from cursor, reconnect with the
  last seq, idempotent commands with retry), `src/views/` (agent transcript,
  queue, questions), `src/unknown.ts` (§8). No framework is decided here
  (open question 8).
- **Build.** `bun build` produces static assets that the `web` feature embeds
  with `rust-embed`, as it embeds the terminal bridge's assets today. CI runs
  `bun install --frozen-lockfile`, `bun test` and the build; the nix package
  builds the assets in a fixed-output step pinned by `bun.lock`.
- **Release.** The assets ship inside the `flk` binary of a build with the
  `web` feature; there is no separate artefact to version.

### 8. Unknown events: pass through, show, log once, report

- **Lossless parse.** `SessionEvent` is a Rust enum over the known
  `type`/`subtype` pairs; every known variant keeps unknown fields in a
  flattened `extra` map, and an unparseable or unknown line becomes
  `SessionEvent::Unknown { raw, shape }`, with `raw` kept byte-for-byte. The
  parser never errors past the JSON layer; a line that is not JSON is
  `Unknown` too, with `raw` as a string.
- **Shape key.** `shape = harness + harness_version + type + subtype +
  sorted top-level keys`. A known event whose `extra` is non-empty is also a
  new shape.
- **Rendering.** Every frontend renders `Unknown` (and a known event's
  `extra`) as a collapsible JSON dump with the shape key as its title. That
  rendering is part of the reference client and of the TUI's structured view.
- **Logged once.** The first sighting of a shape on this server appends
  `session.unknown_shape { harness, version, shape, sample }` to the durable
  log and logs one `tracing` warning; later sightings bump a counter only.
  Owner decision (#1300): this logging goes through `tracing` only, with no
  direct writer, so when flock adopts vigil (#1300) it moves to vigil's
  OTLP/JSON Lines output with no call-site change. The durable-log kind is
  state, not telemetry, and stays in the event log.
  The sample is redacted: keys and JSON types are kept, every string becomes
  its length, numbers are kept, arrays keep their first element only. Older
  servers skip the new kind (ADR-0028 §3a).
- **Reportable.** `flk report --unknown-events` composes a report (ADR-0010)
  from the recorded shapes: the redacted samples, counts, harness versions,
  flk version. It is previewed, the human submits it, and the binary never
  does.

### 9. Forks at any message boundary

`agent.fork` gains `at` and `shape`; `agent.lineage` reports them. No new verb.

```text
AgentForkParams += { at?: { message_uuid } | "latest",
                     shape?: prefix | segment | slice,
                     slice_to?: message_uuid,
                     tools?: [names] , return_to_parent?: bool,
                     answers?: question_id }
```

- **Cut rules.** A fork point is the uuid of an `assistant` message that
  ends a model step (its successor is a tool result or a user message) or of
  a complete tool-result `user` message. Never inside an assistant step
  (signed thinking stays intact), never between a `tool_use` and its
  `tool_result`. Anything else is refused with `invalid_fork_point`.
- **Mechanism per point (E9, E11).** `latest` is `--resume <id>
  --fork-session`. A point inside the current compaction segment is
  `--resume <id> --resume-session-at <uuid> --fork-session`. A point behind
  the last compaction is a rehydrate: the byte prefix of the native
  transcript up to the line before that `compact_boundary`, written under a
  new uuid, resumed with `--fork-session` (and `--resume-session-at` when
  the point is earlier than the prefix's end). `slice` renders messages
  `at..slice_to` as text into a fresh session. `segment` is a prefix fork at
  the first message after a `compact_boundary`.
- **The hidden flag is gated.** On server start and on a harness version
  change, a probe confirms `--resume-session-at` is accepted (a dry resume
  of a two-line fixture with `--no-session-persistence`). If it is gone,
  points inside the current segment use the rehydrate path too, and
  `agent.fork` says which path it took.
- **Read-only forks (E12).** A fork that only answers a question passes
  `tools` (for example `Read,Grep,Glob`) plus a deny-all permission policy.
  Resume keeps the cached tool schemas, so this costs no cache miss; the
  removed tools are refused by the harness.
- **Cost guard.** `dry_run` reports the path, the prefix size (from the
  recorded usage at that point), whether it is warm (last request that
  included it within its TTL: one hour for `ephemeral_1h` writes, five
  minutes otherwise) and the expected cost as cache read or cache write.
- **Fan-out (E15).** Several forks of one cold point run the first alone and
  start the rest after its first `message_start`, so one process pays the
  write.
- **Return channel.** A fork with `return_to_parent` posts its finding as a
  `finding` item into the parent's queue (`producer: fork:<id>`, `anchor:
  fork_point`), through the parent's gate. A human can edit or drop it while
  it is `queued`. A question fork (§2a) is the same mechanism with `answers:
  question_id`: what it returns is a `reply` item anchored to the question,
  at most one per question, and `question.resolve` is how it ends.
- **Lineage.** `AgentForked` (`src/api/schema.rs`, emitted at
  `src/app/api/worktrees.rs`) gains `fork_point`, `shape`, `mechanism`
  (`fork_session | resume_at | rehydrate | slice`) and `source_session`.
  `LineageEdge` carries them. A structured fork does not need a new worktree;
  `agent.fork` keeps creating one only when `branch` or `path` is given.

### 10. TUI parity: what structured mode must rebuild

Claude Code's own TUI provides these; a structured agent's view has to
render them from events. "Source" says where the data comes from.

| # | Capability | Source in stream-json | Slice |
|---|---|---|---|
| P1 | Streaming text and markdown, code blocks | `stream_event` deltas, `assistant` | S9a |
| P2 | Tool call cards, collapsible output, Edit diffs | `assistant/tool_use`, tool-result `user`, `tool_use_result` | S9a |
| P3 | Thinking display | `thinking_delta`, `assistant/thinking` | S9a |
| P4 | Spinner, elapsed time, token counter | `system/status`, `system/thinking_tokens`, `message_delta.usage` | S9a |
| P5 | Permission dialog with allow once / always / deny and rule suggestions | `control_request{can_use_tool}.permission_suggestions` | S6 |
| P6 | AskUserQuestion picker (multi-select, other) | `control_request{AskUserQuestion}` | S6 |
| P7 | Plan mode approve / reject (`ExitPlanMode`) | `control_request{can_use_tool, ExitPlanMode}` | S6 |
| P8 | Interrupt (Esc) | `control_request{interrupt}` from flk | S5 |
| P9 | Queued messages shown, editable, cancellable | flk queue (§3), `command_lifecycle` | S5 |
| P10 | Prompt input with history, paste, image attach, `@file` mentions | flk-side composer; images as content blocks | S10 |
| P11 | Slash commands (`/compact`, `/model`, `/clear`, `/context`, `/cost`) and completion | `system/init.slash_commands`; sent as a prompt | S10 |
| P12 | Context usage meter, cost | `result.usage`, `modelUsage.contextWindow` | S9b |
| P13 | Compaction notice | `system/compact_boundary` | S9b |
| P14 | Subagent (Task) nesting | `parent_tool_use_id`, `--forward-subagent-text` | S9b |
| P15 | Todo list | `TodoWrite` tool input | S9b |
| P16 | Background shells and their output | `Bash` `run_in_background`; output needs the harness's own tool | gap (#1308) |
| P17 | Rewind / checkpoints (Esc Esc) | `--rewind-files` exists, unprobed | gap (#1308) |
| P18 | Rate-limit and usage banners | `rate_limit_event` | S4 |
| P19 | MCP server status | `system/init.mcp_servers` | S9b |
| P20 | Hook output | `system/hook_*` | S9b |
| P21 | Model and effort switching | respawn with `--model` / `--effort` at idle (§1 reload) | S11 |
| P22 | Resume picker | flk's agent list | existing |

P16 and P17 have no source in the stream today and are not promised;
#1308 tracks finding one or deciding against them.

### 11. Harness neutrality

One flk event model covers Claude, Codex and opencode at the level of §2:
session, turn, step, text, reasoning, tool call, tool result, approval
request and answer, user question, interrupt, usage with cache read and
write, compaction, resume, fork. The adapters differ in transport and in
four places that the schema must normalise rather than mirror:

1. **Usage.** Claude reports per turn in `result.usage` (input excludes
   cache); Codex `app-server` per thread in `thread/tokenUsage/updated`
   (its `exec` `input_tokens` appears to include cached tokens); opencode per
   step with `cache.read` / `cache.write` and a cost. flk stores
   `{input_uncached, cache_read, cache_write, output, reasoning}` per turn.
2. **Tool identity.** Claude: a generic `tool_use` then `tool_result`.
   Codex: typed items (`commandExecution`, `fileChange`, `mcpToolCall`).
   opencode: one `ToolPart` mutating through `pending` / `running` /
   `completed` / `error`. flk stores a generic call with a typed `kind`.
3. **Approvals.** Claude allow / deny with `updatedInput`; Codex accept /
   acceptForSession / decline / cancel; opencode once / always / reject. flk
   keeps `allow | allow_always | deny{reason}` and maps down.
4. **Fork granularity.** Claude forks at a message uuid; opencode at a
   message id; Codex at a turn (`thread/fork{lastTurnId}`). `agent.fork`
   refuses a mid-turn point on Codex with `unsupported_for_agent`.

Only the server modes qualify (E18): Codex through `codex app-server`
(stdio JSON-RPC), opencode through `opencode serve` (HTTP + SSE). Their
`exec --json` / `run --format json` modes are not used. Codex has `turn/steer`
and opencode a `todo.updated` event that Claude lacks; they map to `steer`
delivery and P15. Adapters for Codex and opencode are slices S14 and S15 and
not part of the first delivery.

## Failure modes and known limits

- **Hidden flags move.** `--resume-session-at`, `--permission-prompt-tool
  stdio` and `command_lifecycle` are undocumented in `--help`. The
  capability probe of §9 and the unknown-shape log of §8 are how flk notices;
  the fallback is the rehydrate path, never a silent wrong fork.
- **A removed tool stays visible (E12)** until compaction, and the model may
  call it; the call is refused by the harness. The reload notice says so.
- **Process crash mid-turn** leaves the input `held`; the operator or the
  gate decides whether to resend. Tool side effects of the crashed turn are
  not undone.
- **The 1-hour cache write costs 2× input.** A fork's cost guard shows it.
- **Structured agents need O19a (#1208).** Until `flk-agent` exists on a
  host, no structured agent starts there (§1a).
- **`pty` agents still share the operator's uid (#1177)** and can read client
  tokens until O19a; see §5.
- **The structured view needs a client.** A structured agent has no terminal
  to attach to; `terminal/<pane>` is for `pty` agents only.

## Open questions for the owner

Each has a recommendation; acceptance as recommended is the default.

1. **Isolation and login under `flk-agent` (#1177, ADR-0035 §6.3).** A
   structured agent runs as `flk-agent`, so it cannot use the operator's
   config dir or keychain at all. Log each profile in once as `flk-agent`
   (`claude /login` into a private config dir, refreshed in that dir only),
   or provision a `claude setup-token` token per profile? *Recommend
   `/login` per profile as `flk-agent` where a browser is available and
   `setup-token` on headless hosts, never a copy of the operator's
   credentials (E6), with the isolation flags of §1 on top (E5).*
2. **Build on `--resume-session-at`?** *Recommend yes, behind the §9
   capability probe, with the rehydrate path as the measured fallback (E11).*
3. **Questions and permissions over `--permission-prompt-tool stdio` or an
   MCP tool?** *Recommend stdio: it is the only path that brings
   `AskUserQuestion` back (E7), it needs no extra process, and it keeps the
   server the sole owner of the session's I/O.*
3a. **Non-blocking by default (#1301)?** *Recommend yes for questions
   (deferred, §2a), no for permission requests (held open), and an explicit
   `flock_question_ask{blocking: true}` for the rare ask that must block.*
4. **Default delivery per producer.** *Recommend human `steer`, everything
   else `at_idle`, `urgent` = `steer`, interrupts explicit only (§3).*
5. **The messaging socket (E16).** *Recommend: probe slice S0 finds a
   supported switch to disable it or point it at the server; no structured
   session ships before that.*
6. **Where the wire endpoint lives.** The `web` feature is conditional in
   the nix package (`withWeb`) and off in release builds today. *Recommend
   serving `/v1/ws` from the server process (§5) and turning `web` on in
   release builds, as part of S7.*
7. **TS generation.** `ts-rs` (TS straight from the types, drift checked by
   a cargo test) or `schemars` (JSON Schema first, TS through a Bun step).
   *Recommend `ts-rs`: the drift gate runs in plain `cargo nextest` and needs
   no Bun in the Rust CI job.*
8. **Web client framework.** *Recommend none for the reference client
   (plain TS and DOM, Bun's bundler) until a second view needs one.*
9. **Client tokens and scopes** as in §5, with no loopback trust (§1a).
   *Recommend as written; only an `operate`-scoped token, minted over the
   unix socket by the local operator, reaches operator-only verbs.*
10. **Storage first cut.** *Recommend per-agent JSONL plus the blob store
   only; zstd sealing, SQLite and DuckDB as later slices when a measured
   view needs them.*
11. **Thinking retention.** *Recommend keeping thinking blocks lossless (they
   are signed and needed for a byte-exact resume) and deciding retention
   with storage sealing.*
12. **The TUI's structured view.** The TUI is rendered by the server today.
   *Recommend that the server-side renderer subscribes to the same typed
   stream in-process (same Rust types as the wire), so both frontends share
   one contract at the type level, and that a client-side TUI is not part of
   this ADR.*
13. **Coexistence with `[slots]` and the switch paths.** *Recommend no
   change: structured streams travel over the wire endpoint and in-process
   subscriptions, not over the framed render protocol.*
14. **Rehydrated transcripts in the profile's projects dir.** The rehydrate
   path writes a byte prefix of an existing transcript under a new uuid.
   *Recommend accepting it as the ingest/rehydrate operation the issue
   allows, never editing a line, and deleting the copy when the fork ends.*

## Slices

Tracked in #1307. Ordered; each is at most about 400 production lines (tests and doc comments
excluded; TypeScript counts the same as Rust) and lands behind `runtime: structured` or the `web` feature.

| # | Slice | Depends on | Est. lines |
|---|---|---|---|
| S0 | Probe: the messaging socket of E16 (a supported switch to disable it or bind it to the server), recorded in the appendix | — | 0 (probe) |
| S1 | `SessionEvent` types for Claude stream-json: known variants with `extra`, `Unknown{raw, shape}`, shape key; redacted probe fixtures as tests | — | 350 |
| S2 | `session.unknown_shape` persisted kind (skip test per ADR-0028 §3a), once-per-shape registry logging through `tracing` only (#1300), `flk report --unknown-events` | S1 | 250 |
| S3 | Structured runtime: spawn as `flk-agent` with the §1 launch profile, session lease (`session_busy`), lease-based caller identity and the uid check of §1a, stdout pump into the per-agent stream, blob store, truthful outcome | S0, S1, O19a #1208 | 400 |
| S4 | State mapping: `StateSource::Session`, turn / attention / limit from events, stall observation, `agent.phase_changed` | S3 | 300 |
| S5 | Input queue: `queue/<agent>` stream, producers, gate, `command_lifecycle` acks, `at_idle` / `steer` / `interrupt`, `flk agent send` and msg delivery routed to it | S3 | 400 |
| S5b | Turn-end work through the queue: armed self-compact and its continuation on `result`, recap lift, wake of parked waits (§3a, #697) | S5 | 200 |
| S6 | Control requests: `can_use_tool` → `hook.decide` and §8a questions, `AskUserQuestion` answers, `ExitPlanMode` | S4, S5 | 350 |
| S7 | Wire endpoint in the server process: `/v1/ws` hello / sub / cmd, cursors, gap / lagged, idempotency window, `/v1/blob`, `WIRE_JSON_VERSION` gate, client tokens, `web` on in release builds | S3 | 400 |
| S8 | `ts` feature, `ts-rs` derives, generated `web/client/src/gen`, drift test, `just gen-ts` | S7 | 200 |
| S9a | Bun reference client: connect, follow an agent from a cursor, render P1–P4, unknown JSON dump, lazy blobs | S8 | 400 (TS) |
| S9b | Bun reference client: P12–P15, P19, P20, queue view | S9a | 350 (TS) |
| S10 | TUI structured view: server-side renderer from the in-process stream; composer with queue display (P9–P11) | S3, S4, S5 | 400 |
| S11 | Hot reload: idle → close → `--resume`, init diff event and notice, staged fleet reload | S5 | 300 |
| S12 | Forks: `at` / `shape`, cut rules, `resume_at` / `rehydrate` / `slice` paths, capability probe, cost guard, warm-then-fan-out, lineage fields | S3 | 400 |
| S13 | Return channel: `finding` items from forks into the parent's queue | S5, S12 | 150 |
| S6b | Non-blocking questions: question records, deferred answers, `question.answer` / `question.fork` / `question.resolve`, exactly one reply (§2a, #1301) | S6, S12 | 350 |
| S14 | Codex `app-server` adapter onto `SessionEvent` | S4 | 400 |
| S15 | opencode `serve` adapter | S4 | 400 |
| S16 | Storage sealing (zstd segments) and thinking retention | S3 | 250 |

The spike's demonstration ("one throwaway session rendered by both the TUI
and a thin Bun client") is S9a plus S10 against S3; this record does not ship
it, because the brief for #1299 limits the spike to the decision and its
evidence.

## Appendix A — Evidence (2026-10-13)

All runs used model alias `haiku` (resolved to `claude-haiku-5-5`), a
scratch working directory holding three small files, and a throwaway Python
driver that writes stream-json lines to stdin one turn at a time and records
every stdout line with a monotonic timestamp. The driver and the raw
captures are not committed (they contain account rate-limit state); the
commands below are enough to repeat any row. "Profile iso" means
`--setting-sources local --strict-mcp-config`; every run also passed
`-p --input-format stream-json --output-format stream-json --verbose
--include-partial-messages --include-hook-events --model haiku --tools
Bash,Read --allowedTools "Bash(ls:*)" Read` unless the row says otherwise.
Tokens are as reported by the turn's `result.usage`; "read %" is
`cache_read / (input + cache_read + cache_write)`; TTFT is `result.ttft_ms`.

### A.1 Hooks and isolation

| Run | Flags | Hook events seen | Notes |
|---|---|---|---|
| default | no isolation flags | `hook_started`/`hook_response` ×2 each for SessionStart, UserPromptSubmit, Stop | the fleet's user-level hooks; the flock stub no-ops without `FLOCK_*` |
| safe | `--safe-mode` | none | `--mcp-config` is ignored too (the added server never appeared in `system/init`) |
| iso + benign | profile iso, `--settings` with one Stop hook writing a file | only the benign Stop hook | claude.ai connectors still listed as `pending` without `--strict-mcp-config` |
| isolated dir | `CLAUDE_CONFIG_DIR=<empty dir>` | — | `Not logged in · Please run /login`, exit 1 |

### A.2 Event catalogue (all runs)

Count, `type[/subtype]`, top-level fields besides `type`, `session_id`, `uuid`:

```text
  121 assistant                       message, parent_tool_use_id, request_id, timestamp, error, is_api_error_message,
                                      aborted, thinking_display, thinking_duration_ms, tool_use_meta, wire_tool_inputs,
                                      user_message_uuid(s)
   38 user                            message, parent_tool_use_id, timestamp, isReplay, isSynthetic, tool_result_meta,
                                      tool_use_result
   15 command_lifecycle               command_uuid, state ∈ {queued, started, completed, cancelled}
   68 system/init                     agents, apiKeySource, capabilities, claude_code_version, cwd, mcp_servers,
                                      memory_paths, messaging_socket_path, model, output_style, permissionMode, plugins,
                                      skills, slash_commands, tools, …
   94 system/status                   status ∈ {requesting, compacting, null}, compact_result, user_message_uuids
   36 system/thinking_tokens          estimated_tokens, estimated_tokens_delta, user_message_uuid
    2 system/compact_boundary         compact_metadata{trigger, pre_tokens, post_tokens, preserved_segment, …},
                                      logical_parent_uuid
    2 system/permission_denied        tool_name, tool_use_id, decision_reason_type, message
    7 system/hook_started             hook_event, hook_id, hook_name
    7 system/hook_response            hook_event, hook_id, hook_name, exit_code, outcome, output, stdout, stderr
   58 rate_limit_event                rate_limit_info{status, resetsAt, rateLimitType, …}
    2 control_request/can_use_tool    request_id, request{tool_name, display_name, input, tool_use_id, description,
                                      permission_suggestions, requires_user_interaction}
    1 control_response                response{subtype, request_id, response}   (reply to flk's interrupt)
   89 stream_event/message_start      event, api_message_id, ttft_ms, user_message_uuid(s), parent_tool_use_id
  842 stream_event/content_block_delta  delta ∈ {text_delta, thinking_delta, signature_delta, input_json_delta}
  120 stream_event/content_block_start  content_block ∈ {text, thinking, tool_use}
  120 stream_event/content_block_stop
   89 stream_event/message_delta, message_stop
   67 result/success                  result, is_error, stop_reason, terminal_reason, api_error_status, usage, modelUsage,
                                      total_cost_usd, ttft_ms, num_turns, permission_denials, queued_turn_count, …
    1 result/error_during_execution   errors, terminal_reason = aborted_streaming
```

`system/init.capabilities` was `interrupt_receipt_v1, interrupt_cancel_queued_v1,
interrupt_send_now_v1, msg_lifecycle_v1, request_marker_lists_v1,
sdk_mcp_tools_list_changed, sdk_mcp_manifests, mcp_read_resource_v1,
mcp_tool_ui_meta_v1, ui_surface_v1` in every run. `result.total_cost_usd`
is cumulative over the resumed chain (a fork starts from its parent's
recorded cost), so it is not a per-test cost.

### A.3 Base session and hot reload

| Run | in | read | write | out | read % | TTFT ms |
|---|---|---|---|---|---|---|
| base turn 1 (`ls`) | 4 | 5,102 | 5,214 | 87 | 49.4 | 1,114 |
| base turn 2 (two Reads) | 4 | 10,502 | 430 | 271 | 96.0 | 1,620 |
| base turn 3 | 2 | 5,644 | 47 | 3 | 99.1 | 1,222 |
| reload, no compact: turn 1 (Read of a 70 KB file) | 6 | 39,908 | 35,726 | 579 | 52.8 | 1,679 |
| reload, no compact: first turn after `--resume` + `--mcp-config` | 2 | 40,921 | 470 | 4 | 98.9 | 1,681 |
| reload, compact: turn 1 | 6 | 63,360 | 12,198 | 499 | 83.8 | 2,397 |
| reload, compact: `/compact` (result.usage) | 0 | 0 | 0 | 0 | — | — |
| reload, compact: `/compact` (modelUsage delta) | 13,945 | 28,647 | 250 | 1,383 | — | — |
| reload, compact: first turn after `--resume` + `--mcp-config` | 2 | 0 | 7,815 | 4 | 0.0 | 1,140 |

`compact_metadata`: `trigger: manual, pre_tokens: 40032, post_tokens: 1237,
duration_ms: 7140`. The init diff after the reload was exactly
`mcp_servers += flkstub (connected, dynamic)` and `tools +=
mcp__flkstub__approve, mcp__flkstub__ask_user` (plus a new
`messaging_socket_path`).

### A.4 Flags present in the binaries

`strings` over both the 2.1.296 and the 2.1.287 binary find
`--resume-session-at <message id>`, `--resume-drops-turn`,
`--rewind-files`, `--fork-point` and `--permission-prompt-tool` (including
`=stdio`); none is in `--help`. Errors read: `--resume-session-at requires
--resume`; `No message found with message.uuid of: <id>`. The loader looks
the uuid up among the loaded messages and keeps everything up to it.

### A.5 Turn control: mid-turn input, interrupt, error

Driven with `--replay-user-messages` and flk-chosen input uuids `1111…`
and `2222…`.

- **Mid-turn.** Input 1 at t=0.002 s; `command_lifecycle{1, queued}` 0.49,
  `{1, started}` 0.49; input 2 written at 1.20 after the third delta;
  `{2, queued}` 1.20; `{2, started}` 1.51 (input 1 still running); one
  `result` at 3.98 (`success`, `queued_turn_count: 0`) whose text answers
  input 2; then `{2, completed}`, `{1, completed}`.
- **Interrupt.** `control_request{request_id: int-1, subtype: interrupt}`
  after five deltas; `control_response{success, still_queued: []}` within
  1 ms; a synthetic `user` "[Request interrupted by user]"; `result{subtype:
  error_during_execution, is_error: true, terminal_reason:
  aborted_streaming}`; `{1, cancelled}`; process exit 1 after stdin close.
- **Error.** `--model no-such-model-1299`: `assistant{error:
  model_not_found}`; `result{subtype: success, is_error: true,
  terminal_reason: api_error, api_error_status: 404}`; exit 1; stderr
  `[claude-code:unrecognized_model]`.

### A.6 Permissions and questions

Prompt: run `ls`, then `touch flk-probe.txt`, no `--allowedTools`.

| Run | Path | `touch` outcome | `permission_denials` |
|---|---|---|---|
| P2 | `--permission-prompt-tool mcp__flkstub__approve` (an MCP tool returning `{"behavior":"deny","message":…}`) | denied with flk's message | 1 |
| P3 | `--permission-prompt-tool stdio`, driver answers `control_response{behavior: deny}` | denied with flk's message | 1 |
| P6 | no prompt tool | denied by the harness ("… need[s] permission") | 1 |
| P4 | MCP `ask_user` tool | answered "blue" | — |
| P5 | `--tools default --permission-prompt-tool stdio` | `AskUserQuestion` listed; answered via `updatedInput.answers` | — |
| P7 | `--tools default`, no prompt tool | `AskUserQuestion` absent; `ToolSearch select:AskUserQuestion` finds nothing | — |

`ls` and `cat` were never put to the prompt tool (read-only commands are
allowed without asking).

### A.7 The cache matrix

Base F: three turns (Read a 300-line file; `ls` then Read, sequentially; a
one-word reply), 19.4k tokens of context. Fork prompt "Reply with exactly:
ok" unless noted. Input-equivalent cost = `in + 0.1 × read + 2 × write`
(1-hour writes), in tokens.

| Test | What | in | read | write | out | read % | TTFT ms | input-eq |
|---|---|---|---|---|---|---|---|---|
| 1 | latest (`--resume F --fork-session`) | 2 | 19,376 | 64 | 4 | 99.7 | 2,384 | 2,068 |
| 2 | end of turn 1 (`--resume-session-at`) | 2 | 18,913 | 108 | 4 | 99.4 | 1,853 | 2,109 |
| 3 | between the two tool calls of turn 2 | 2 | 19,053 | 135 | 117 | 99.3 | 2,949 | 2,177 |
| 4 | base G, end of turn 1, immediately | 2 | 18,913 | 112 | 4 | 99.4 | 1,741 | 2,117 |
| 4 | same point after 6 min | 2 | 19,025 | 0 | 4 | 100.0 | 1,421 | 1,905 |
| 4 | same point 65 min after the 6-min read | pending (#1305), A.7.1 | | | | | | |
| 5a | point of test 2 + an MCP server added | 2 | 18,996 | 336 | 4 | 98.3 | 1,606 | 2,574 |
| 5b | point of test 2 + `--tools Bash` (was `Bash,Read`) | 2 | 19,021 | 0 | 4 | 100.0 | 1,172 | 1,904 |
| 5c | 41k session forked with its MCP server removed | 2 | 41,391 | 165 | 4 | 99.6 | 1,619 | 4,471 |
| 5d | control for 5c, server kept | 2 | 41,391 | 41 | 4 | 99.9 | 1,877 | 4,223 |
| 5e | 5b, asked to use Read | 4 | 37,966 | 353 | 230 | 99.1 | 1,752 | 4,507 |
| 5g | point of test 2 + MCP added, asked to call it | 4 | 38,283 | 559 | 80 | 98.6 | 1,163 | 4,950 |
| 6 | day boundary | pending (#1305), A.7.2 | | | | | | |
| 7a | segment: fork at the first message after `/compact` | 2 | 5,195 | 2,266 | 4 | 69.6 | 683 | 5,054 |
| 7b | slice of F's turn 1 (28 kB of text) as the first prompt of a fresh session | 2 | 5,195 | 13,808 | 6 | 27.3 | 1,588 | 28,138 |
| 7c | `--resume-session-at` a preserved pre-compaction uuid | 2 | 5,195 | 2,153 | 3 | 70.7 | 1,139 | 4,828 |
| 7d | byte-prefix copy ending before `compact_boundary`, `--resume <copy> --fork-session` | 2 | 40,845 | 170 | 3 | 99.6 | 1,149 | 4,426 |
| 8a | 10 concurrent forks at test 3's point (each) | 2 | 19,188 | 0 | 87–129 | 100.0 | 1,428–2,984 | 1,921 |
| 8b | 10 concurrent fresh sessions, identical new 28 kB prompt (each) | 2 | 5,195 | 13,808 | 6 | 27.3 | 1,143–2,163 | 28,138 |
| 8c | the same prompt once more after 8b | 2 | 19,003 | 0 | 6 | 100.0 | 1,251 | 1,902 |

Notes:

- The 5,195-token read in 7a, 7b, 7c and 8b is the system prompt and tool
  block shared by every session with the same flags; a fresh session G whose
  first turn matched F's byte for byte read 24,979 and wrote 0 (cross-session
  prefix reuse).
- 7c's transcript holds `compact_boundary`, the summary, the preserved
  assistant message and the new turn only: the pre-compaction messages are
  not reachable through `--resume-session-at`.
- 5e: the model called `Read`; the harness answered `<tool_use_error>Error:
  No such tool available: Read. Read is disabled for this session, in
  subagents as well as here.</tool_use_error>`.

#### A.7.1 Test 4, 65 minutes

Pending: not run when this record was written; tracked as #1305, which
must be done before this record leaves Proposed.

#### A.7.2 Test 6, day boundary

`CLAUDE_CODE_OVERRIDE_DATE=2026-10-14` on a fork at test 2's point read
99.4% (18,913 / 116) but did not change the date: no date-change
attachment appeared and the model answered 2026-10-13, so the override did
not take effect on this path and the row proves nothing about a day
boundary. A real run straddling local midnight (base at 23:56, fork at
00:02) is pending: tracked as #1305, before this record leaves Proposed.

### A.8 Bytes

Over every probe run (1,864,109 bytes of stdout JSON):
`user` tool results 41.7%, partial deltas 28.8%, other events 11.1%,
`system/init` 10.6% (repeated at every turn), `assistant` 7.8%.

### A.9 Budget

Sum over all `result.usage` objects at the time of writing: input 176,
cache read 1,204,445, cache write 365,494, output 6,860; total 1,576,975
tokens (76% cache reads), under the 2M ceiling the brief set.

### A.10 Codex and opencode

Captured with `codex exec --json --skip-git-repo-check -s read-only` (exit 0),
`codex app-server generate-json-schema` and `generate-ts` (exit 0; 651
definitions), `opencode run --format json` (exit 0) and `opencode serve` on a
loopback port (`GET /doc`: OpenAPI 3.1, 162 paths; `GET /event` SSE; one
`POST /session` and one `POST /session/{id}/message`).

| Concept | Codex | opencode |
|---|---|---|
| session init | `thread/started{thread}` (exec: `thread.started{thread_id}`) | `session.created`, `POST /session` → `Session` |
| text, streaming | `item/agentMessage/delta`; `item/completed{agentMessage}` | `message.part.delta`; `message.part.updated{text}` |
| reasoning | `item/reasoning/textDelta`, `summaryTextDelta` | `ReasoningPart` |
| tool call / result | `item/started` / `item/completed` with `commandExecution`, `fileChange`, `mcpToolCall`; `…/outputDelta` | `ToolPart{callID, tool, state: pending → running → completed / error}` |
| turn end, usage | `turn/completed{status: completed / interrupted / failed}`; `thread/tokenUsage/updated{inputTokens, cachedInputTokens, cacheWriteInputTokens, outputTokens, reasoningOutputTokens}` | `step-finish{tokens{input, output, reasoning, cache{read, write}}, cost}`; `session.idle` |
| errors | `error{willRetry}`, `Turn.error{codexErrorInfo}` | `session.error`, `AssistantMessage.error` (`APIError`, `ContextOverflowError`, …) |
| approval | server request `item/commandExecution/requestApproval`, `item/fileChange/requestApproval`; reply `{decision: accept / acceptForSession / decline / cancel}` | `permission.asked`; `POST /permission/{id}/reply{once / always / reject}` |
| user question | `item/tool/requestUserInput{questions}`, `mcpServer/elicitation/request` | `question.asked{questions[{options, multiple, custom}]}`; `POST /question/{id}/reply` |
| interrupt | `turn/interrupt`; `turn/steer` injects mid-turn | `POST /session/{id}/abort` |
| resume | `thread/resume` | `run -s <id>`; persistent sessions |
| fork | `thread/fork{lastTurnId}` (turn granularity) | `POST /session/{id}/fork{messageID}` |
| compaction | `thread/compact/start`, `thread/compacted` | `POST /session/{id}/summarize`, `session.compacted` |

Not verified live: reasoning events, approvals, questions, errors and
interrupts on both (read from the schemas only), and any live Codex
`app-server` session; #1309 tracks those runs.
