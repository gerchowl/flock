# ADR 0036 — An agent can run as a structured session: flk owns its process and its one input queue, translates its typed event stream into flk's state, questions and waits, serves every frontend one wire contract, and forks it at any message boundary

- Status: Accepted (2026-10-13, owner decisions recorded); amended
  2026-10-14 with the S0 probe findings (#1324, "S0 findings for the
  architect" says where each landed), and with the A.13 controls, the A.16
  login results and the owner's narrowing of decision 1 (#1404); amended
  2026-10-16 (#1606, ADR-0038): the Vite client build (§7), the `synced`
  frame and the `gap` frame's `snapshot_to` (§4, §5), and the owner's
  decision that flk's log, metric and trace output goes through vig-os/vigil,
  which resolves #1323 so it no longer gates S7 (§5, decision 6, S7);
  transport liveness and recovery are specified in §5a
- Amendment accepted by the architect, 2026-10-16 (#1546, #1641): §4/§5/§7
  define transport recovery, snapshot bootstrap and `synced` before S7a.
  Numeric parameters in §5a are proposals with rationale for review; no
  transport implementation or deployment decision is claimed.
- Implemented: the S0 probes (#1356); the slices are tracked in #1307.
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
  #1300 for unknown-event logging (§8); reserves the extension points that
  the follow-up spike #1310 designs (§12). Extends `agent.fork` /
  `agent.lineage` (`flock_agent_fork`, `flock_agent_lineage`) rather than
  adding verbs. Designs the permission presets of #1321 (P25, §10b).
- Follow-ups: #1307 (slice tracking), #1305 (the cache tests; test 4 is
  measured in A.7.1, test 6 in A.7.2), #1308
  (TUI parity gaps P16, P17), #1309 (live Codex and opencode runs), #1323
  (the Grafana Labs stack versus our own for the stream and history, §5;
  resolved 2026-10-16 in favour of vig-os/vigil for telemetry, #1614).
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

The owner then answered this record's questions on 2026-10-13; the answers
are in "Owner decisions (2026-10-13)" and the body follows them. This record
decides the design and the slices. It changes no code.

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
  date. A fork across a day boundary therefore keeps its cache: forked
  across local midnight, it read 98.1% from cache, and the harness appended
  a date-change reminder after the new prompt while the recorded date
  stayed as it was (A.7.2). The model need not act on that reminder
  (A.7.2, caveats). (A.7)
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
- **E16 The harness opens a messaging socket of its own, and
  `--restricted` closes it.** Every `-p` process opens a socket under
  `/tmp/cc-socks/` and reports it as `system/init.messaging_socket_path`.
  On macOS it runs unauthenticated `user` lines from any process of the
  session's uid as turns, and every child process of the session gets its
  path and token. `--restricted` (or `CLAUDE_CODE_RESTRICTED=1`) binds no
  socket and sets neither variable, while `--settings` hooks,
  `--mcp-config` servers and the stdio prompt keep working. flk launches
  with it and never uses the socket (§1, owner decision 5). (A.2, A.11)
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
--restricted --settings <flk file> --mcp-config <flk file> --session-id
<uuid>` (or `--resume <id>`), started as the `flk-agent` user (§1a) with that user's
private config dir for the agent's profile as `CLAUDE_CONFIG_DIR`, and the
compiled context of ADR-0032 passed through `--append-system-prompt-file`
and `--add-dir`. The server owns stdin and stdout exclusively.

- **Isolation and login (E5, E6, ADR-0035 §6.3 point 1).** A structured
  agent never runs as the operator's uid. Its `CLAUDE_CONFIG_DIR` is a
  private dir per profile in the `flk-agent` user's `HOME`
  (`~flk-agent/.flk/claude/<profile>`). The login is a `claude setup-token`
  token only (owner decision 1, narrowed 2026-10-14): g-fleet renders it
  from sops as a 0600 file owned by `flk-agent` (g-fleet#476), and flk
  passes it into the session's environment as `CLAUDE_CODE_OAUTH_TOKEN`
  and never logs it. `claude /login` is not used for agents: as a macOS
  user with no GUI session it cannot save its login (no keychain, and no
  plaintext fallback on 2.1.287), while the token path works for that user
  (A.16). The operator's config dir and keychain are never read or copied:
  sharing them would share a rotating refresh token between two dirs (E6). Inside that
  dir the session is still isolated by flags: `--setting-sources local` (no
  user-level hooks, permissions or plugins), `--strict-mcp-config` (no
  claude.ai connectors arriving late and shifting the tool set) and flk's
  own `--settings` and `--mcp-config`, so nothing that lands in the dir by
  accident changes the session. `--restricted` closes the harness's
  messaging socket (see "No second input door" below); it also reads no
  user settings, turns off auto memory and refuses cloud sessions (A.11),
  which this isolation wants anyway. Because user-level permission rules are
  dropped, flk passes the agent's permission policy explicitly, as the
  agent's permission preset (§10b). The fleet
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
  control request is open. S0 observed only one open at a time: no later
  call of the same message ran until the open one was answered, a
  concurrency-safe `Read` included (A.13, §2b). flk still does not rely on
  one open request at a time, because the probed shapes are few. It
  tracks open control
  requests by `request_id`, and the agent stays `waiting_on_human` until
  every one of them is answered. Working is `system/status: requesting` or
  an open `message_start`. A stall is an accepted or started input with no stdout event for
  `[sessions] stall_ms` while no control request and no tool call is open;
  a long-running tool is not a stall (its `tool_use` is open). A stall is
  reported as `agent.session_stalled{since}` and leaves the phase as it is
  (ADR-0028 §1 derivation is unchanged); it is an observation, not a timeout
  that kills anything.
- **No second input door (E16, owner decision 5).** flk owns the session's
  stdin, so every input, from a human or from another agent, is an item of
  the one queue of §3. flk has no "messaging socket" concept: it never
  writes to the harness's socket, never advertises its path to agents, and
  agent-to-agent messages travel as `msg.send` into the queue. The
  harness's own socket is closed by the launch profile: with `--restricted`
  no socket is bound, `system/init` carries no `messaging_socket_path`, and
  neither `CLAUDE_CODE_MESSAGING_SOCKET` nor `CLAUDE_CODE_MESSAGING_TOKEN`
  reaches the agent's tool processes (A.11). Without it, the socket would
  take unauthenticated `user` lines from any `flk-agent` process, so any
  structured agent could start turns in any other one past the queue
  (A.11). S3 diffs the `system/init` of the restricted profile against the
  same profile without `--restricted` (tools, MCP servers, skills, slash
  commands, capabilities, permission mode) and records the result before
  it ships; S0 found `tools`, `slash_commands` and `capabilities`
  unchanged (A.11). S3 also refuses to run a session whose `system/init`
  reports a `messaging_socket_path` (see "Failure modes").
- **Hot reload.** On a config change (MCP server, skill, settings) the
  server waits for idle (no started input, no open control request), closes
  stdin, waits for exit, and respawns with `--resume <id>` and the new
  config. The first `system/init` of the new process is diffed against the
  last one of the old (tools, MCP servers, skills, plugins, slash commands,
  model, permission mode) and recorded as `agent.session_reloaded{diff}`;
  the diff is also queued to the agent as a `reload_notice` item (§3) so that it knows
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
  **Account policy (owner decisions, #1795).** File-only
  `[security] agent_account = "flk-agent" | "none"` defaults to `"flk-agent"`.
  Recognition is independent of `structured.run_as`: opting into operator-mode
  launch does not disable it. With recognition enabled, a pending or failed
  directory lookup classifies socket callers as `Unknown`, including callers
  with the server UID: reads remain available but operator mutations, including
  server stop, are refused. A resolved dedicated UID distinct from the server
  UID permits the server UID to proceed to the existing ancestry checks; a
  resolved absent account does too. A dedicated UID equal to the server UID
  is a §1a misconfiguration, never operator authority. Report
  `agent_account_uid_collision` once in the log and retain the typed diagnostic
  in server status until restart. Each server owns its account cache and
  collision-reporting state; its app and API listener share the same policy.

  Explicit `agent_account = "none"` declares that no dedicated account is used.
  The server never starts the dedicated-account NSS lookup, and callers with
  its own UID immediately pass the UID boundary. Lease, pane, birth and relay
  checks still apply: known same-UID agents remain non-operator. Other or missing
  UIDs remain unknown/read-only. This opt-in avoids the default directory-service
  availability cost but gives up recognizing anonymous/detached callers as
  `flk-agent` by UID. It cannot provide a dedicated-account isolation boundary;
  same-UID callers without lease/pane ancestry remain cooperative under #1177.
  Changing the setting requires a server restart; reload keeps the active policy
  and reports that requirement.

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

**Scoped session MCP requests (#1734).** Every structured-session MCP API request carries a server-issued credential scoped to one lease reservation generation. The server verifies it against current kernel peer identity and the live bound lease root, and derives only an agent principal. Failed or revoked session authentication never falls back to operator ancestry classification. This applies independently to every connection, queued request and channel attachment; lease/root loss refuses subsequent admission. Already admitted work never acquires operator authority. Parent-change exit is defense in depth. Unscoped arbitrary same-UID programs retain the explicitly cooperative #1177 limitation; dedicated-account policy remains mandatory unless the operator opts out.

Slice A adds the scoped local request envelope and server admission. Slice B of #1734 supplies mandatory generated MCP credentials and immutable transport propagation; public structured starts remain gated until both land. Legacy unscoped CLI/MCP requests retain their existing policy. Credentials are not operator/client tokens and never appear in logs, errors or status. Failed scoped decoding or authentication cannot be retried through legacy dispatch. Revocation precedes cleanup and lease removal; scoped subscriptions close on revoked or unreadable identity. Operations admitted before root loss may finish with agent authority only. The scoped verifier requires a living kernel process, not merely a matching birth time: zombies cannot authorize admission. The local envelope is `{"request": <Request>, "credential": <LeaseCredential>}`; malformed scoped envelopes return `invalid_request`, authentication failures return `not_authorized`, and neither path retries as a legacy request.

Slice B of #1734 provisions each generation credential only in the generated private MCP configuration's child environment (`FLOCK_MCP_LEASE_CREDENTIAL`), alongside an explicit `mcp serve --structured` mode. It never enters Claude's launch environment or process arguments. Before reading stdin, the MCP child captures that mode, the absolute socket endpoint and the credential into immutable client state. Calls, compatibility probes, subscriptions and reconnects use that same state. Missing, malformed or inconsistent configuration fails startup; authentication failure never selects unscoped transport. Ordinary unscoped MCP remains compatible. Public structured starts still require the registry/start slice (#1439).

Scoped verification never waits for the lease registry lock: contention has a typed refusal and no snapshot fallback. Each admission performs at most sixteen kernel process-fact reads, refusing ancestry beyond that budget. This is a bounded supported ancestry depth, not permission to trust partial evidence. Credential comparison uses opaque accumulation (`core::hint::black_box`) to prevent optimizer replacement with an early-exit comparison. Encoded credential length is public format information. Legacy unscoped malformed requests retain serde diagnosis; scoped schema failures keep their fixed, secret-free error. JSON syntax diagnostics contain no field values.

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
| `control_request{can_use_tool, tool_name: AskUserQuestion}` | a question record (§2a), `source: session`, options from `input.questions`; answered at once if the policy allows, otherwise held or deferred by the System-One check (§2a); routed per ADR-0034 §2 |
| `control_request{can_use_tool}` (any other tool) | `hook.decide{point: permission.request}` (ADR-0034 §1); `pass` answers allow, `deny` answers deny with the reason; `defer` holds the request open and makes it a question |
| `control_request{other subtype}` | answered with an error and logged as an unknown shape (§8) |
| flk's own `deny` answer to a `can_use_tool`; `system/permission_denied`, `result.permission_denials` | `agent.permission_denied{tool, reason}`, once per `tool_use_id`. A stdio `deny` that flk sends emits no `system/permission_denied` event (A.12, A.17), so for those the fact comes from flk's own answer; `system/permission_denied` and `result.permission_denials` add only the denials the harness made itself. Carve-out: the `deny` that carries a question deferral (§2a) is not a permission denial. flk records the `tool_use_id` of every deferral, and neither its answer nor that id's entry in `result.permission_denials` emits anything here |
| `rate_limit_event{status: rejected}` | `Attention::Blocked{ProviderLimit, retry_after_ms}` from `resetsAt` |
| `rate_limit_event{status: allowed or allowed_warning}` | clear provider-limit attention; a warning does not block |
| `rate_limit_event{unknown status}` | ignore the status, preserving the last known observation (ADR-0028 §3a) |
| `control_cancel_request{request_id}` | close that incoming permission/question request only |
| `result` | close open permission/question requests; queue items `done`; `Turn::Idle`; usage recorded; outcome per §1 |
| process exit without `result` | close open permission/question requests; `Lifecycle` unchanged, `Attention::Blocked{crashed}` |
| anything unknown | §8 |

S4 integration boundary (architect ruling, #1427). One pure reducer per
`agent_id` and launch generation consumes S1 events and explicit host facts.
Its `StateSink` publishes `AgentState` snapshots, immediate phase transitions,
all open needs-human request ids/reasons, and one stall observation per silent
interval. #1439 connects this seam to the registry and agent.get/list; S7-min
#1746 connects it to agent/<id> and fleet-lite. These immediate facts are not
settled supervisor events. S4 emits no `agent.phase_changed`, `agent.needs_human`
or `agent.session_stalled` EventHub envelopes: `AgentPhaseEvent` and mesh v5
stay unchanged. #1804 owns the mesh-versioned pane-free event contract.
No placement is empty or fabricated, and list/side-pane wiring remains #1439.

S3a-3 registry amendment (#1439, owner-approved split). #1841 owns pure
records, runtime-tagged persistence and the S4 `StateSink` consumer. Each
registered agent owns a reducer and a generation token. Admission rejects
conflicting agent ids, names and native session ids without replacing another
record. Removal and state delivery require the current generation, so late
worker output cannot alter a replacement. The registry stores the complete S4
publication, including phase edges, needs-human facts and the next observation
deadline; it emits no EventHub envelopes (#1804).

Session persistence stores `runtime: structured` identity/name/harness/session,
cwd and requested model alongside `runtime: pty` pane records. Older snapshots
without structured records remain valid; older pane records default to PTY.
Cold restore retains metadata only: lifecycle is offline, observations are
unknown, and no clock ages, outstanding questions, deadlines, process ids or
lease proofs survive. An agent whose process is gone stays `Lifecycle::Offline`:
it remains resumable by explicit launch and is never resurrected automatically.
Under ADR-0028 §3a, structured records deserialize individually. Unknown runtime
or harness records, invalid entries and duplicates are skipped with a warning,
without rejecting any PTY workspace. The first supported id/name/session wins.
Skipped entries remain raw and in order across saves, including additional
fields, so rollback cannot erase newer records. Explicit removal also discards
retained entries sharing the removed id or session, preventing a shadowed
duplicate from becoming active after restart. Known-runtime shadows also match
by name; a future-runtime record sharing only a name has a separate identity
and survives removal (#1858). Explicit admission refuses any retained name.
Removal compacts retained slots.
Known records also retain their original extra fields. An unknown collection shape remains raw
and inactive; new admission refuses until its shape is understood. Warnings
contain an index and reason, never raw payloads. The registry exposes a retained
entry count for diagnostics; structured status/doctor routing remains #1843.
Unknown pane runtime tags also survive restore/save and disable automatic agent
resume for that pane only; its hibernation request and shell placement survive,
and other panes retain normal resume behavior. A registry-only session remains
persistable even when it contains only inactive raw entries and no terminal
workspaces.

**Retained-record lifetime (corrected owner decision, #1853).** DEAD classification
is version-independent: an element is **DEAD** only when it is not a JSON object,
or its canonical digest equals a record already kept, live or retained (an exact
duplicate). Canonical JSON here sorts object keys and preserves JSON value types.
Everything else this build cannot use is **FORWARD**, never automatically pruned:
supported-tag parse failures (including future field shapes), unknown runtime or
harness values, empty/odd identity strings, and non-identical conflicting records
sharing an agent ID, name or session ID. Current-build parsing/validation failure
is never evidence that another version cannot use an object (ADR-0028 §3a).
On save, prune DEAD records strictly older than `RETAINED_DEAD_MAX_AGE` (30 days)
and then enforce `RETAINED_DEAD_MAX` (256), oldest first. Equal timestamps use
original array order. The injected clock determines age; a backward clock never
ages a record early. Missing stamps receive the next capture/save time, not an
invented historical age. Every capture runs stamp-only preparation on its snapshot copy
with an injected clock; undo/test captures never prune or mutate live state.
Only actual save/export boundaries prune the live registry. A failed wall clock
or an unknown sidecar collection format logs a warning and refuses automatic
retention maintenance, preserving data.

`first_seen_ms` is Unix milliseconds in the additive `structured_retention`
sidecar field, never a property inserted into preserved record JSON. Each entry
binds a timestamp to the serialized slot and a SHA-256 digest of the record's
JSON representation. Mismatched or invalid sidecar entries are logged and kept
raw; matching entries preserve extra fields. Recognized stamps precede opaque
entries so stale duplicates cannot replace the active timestamp. Cold restore,
compaction, subsequent saves and live handoff carry these stamps. Older binaries
may drop this new sidecar on save; missing stamps then restart conservatively on
the next new-binary save. Retention timestamps never supply runtime observations,
lease evidence or authority.

A non-array `structured_agents` collection remains raw and inactive and refuses
admission with typed `RegistryError::UnsupportedCollectionShape`, naming the
session-state repair guide. Never discard the whole session because of a bad
entry. The operator list/purge commands and doctor counts/hints are the separate
second PR of #1853; they are not enabled by this policy/persistence slice.
Both the temporary
snapshot file before rename and its parent directory after rename are fsynced.
A directory sync failure after successful replacement is logged as a durability
warning and the save reports success because the new file is already installed.
Handoff restores structured metadata offline; this slice owns no workers to
transfer.

#1842 owns runtime workers, actual lease ownership and observation scheduling.
The app owns `structured::workers::Workers` outside pure `AppState`. Each launch
reserves its native session before running the admission-supplied, bounded
profile factory on an owning thread, then binds the registered cwd/model.
Preparation is cancellable at its boundaries; an in-progress factory must finish
within its own bound. One blocked factory cannot block another agent or the app.
A new launch advances the observation generation, without modifying durable
metadata. Live/unconfirmed workers refuse replacement. Cold restore remains
offline and never starts a worker automatically.

Ready/completion share a bounded two-message channel. Runtime stream queues are
bounded separately; an app pass drains at most four records per worker and
rearms its existing event wake when it reaches that budget. Every stream record
enters S4 only via `Received::state_input`, so durable permission-book closes
clear attention without waiting for a result (#1846/#1849). Lifecycle and due
observations are explicit host facts. Both TUI and headless schedulers service
owner wakes and include S4's exact next observation boundary. No periodic
structured-agent timer, kill-on-silence policy or EventHub emission is added.

Ordinary stop latches cancellation without waiting; polling joins only finished threads. A terminal report
becomes visible only after queued stream facts drain. Confirmed cleanup closes
open requests and sets Offline; unconfirmed ownership instead retains authority
and Unknown lifecycle, refuses restart, and requires explicit recovery. Reports
retain actual exit status/outcome or typed preparation/worker failure; results
remain turn facts rather than process-success evidence. Finished threads are joined
before replacement or pruning. Shutdown and the pre-handoff step attempt to stop
all workers, drain durable permission closes, release confirmed leases, and join
within the stated bound. Handoff deliberately stops structured sessions in this
slice (full single-writer transfer is S7-rest). Records remain Offline and resume
only through an explicit start using their stored session id, never automatically.
A shared 10-second graceful deadline is followed by identity-checked child-tree
KILL and a bounded 10-second cleanup/join interval. Deadline overruns report
`structured_worker_timeout`; unconfirmed cleanup reports
`structured_worker_cleanup_failed`. Worker diagnostics never veto handoff: kill,
report the typed codes and proceed once the real handoff preconditions hold.
An additive, optional handoff-manifest list carries uncertain agent identities,
restored as Unknown without lease or process authority; confirmed cleanup stays
Offline. Cold disk restore still defaults to Offline. Unknown identities never
grant permission to signal. An unjoined worker remains owned and fenced, and
later owner polls can finish joining it. Worker panics report the distinct
`structured_worker_panicked` failure. These diagnostics require human inspection
before retrying an uncertain session. Shutdown runs once per worker set and caches its typed result.
Both TUI and headless quit always save the session before returning a shutdown
error. Drop runs bounded teardown only if it has not already been attempted;
after success or failure it joins only finished threads, without another wait.
A newly admitted worker resets that cached result. The caller must mark session persistence
dirty when durable metadata changes.
#1843 owns the public `StructuredAgentInfo`, additive `structured_agents` list,
get/status/stop routes and `runtime_unsupported` refusals. Existing PTY
`AgentInfo` placement fields remain mandatory and unchanged; structured records
have no fabricated placement. Mesh VERSION stays 5; remote directory rows
remain #1380. Public structured start stays unreachable until #1843 and #1734
have landed. The default remains the agent user (#1177); the owner-approved
MVP exception permits explicit operator-mode opt-in only. #1843 supplies the
public admission wiring and isolated manual dogfood runbook for that exception.

Accepted-input silence (#1903): successful transport admission of a user
stdin envelope supplies `Input::Accepted { at, at_ms }` through
`Received::state_input`, using monotonic and Unix clocks captured at admission.
It arms the same caller stall policy before any started/requesting/message_start
observation, without inventing a working turn. Later stdout resets the silence
anchor normally; started evidence establishes working, and top-level result or
exit retires the pending interval. Control requests and open tools suppress
stall reporting exactly as for started turns. The transport serializes its host
`flk_input_accepted` stream record before later stdout observations, even for a
fast response. This is an internal stream fact, not an EventHub event kind or
mesh frame. Its monotonic stamp belongs only to the current launch and cannot
be reconstructed on replay; a raw host record without that stamp is skipped.
Admission is not proof of a successful pipe write or provider turn. An unavailable
acceptance observer requests process shutdown as well as pipe cancellation.
Ordinary I/O cancellation does not record stop intent or change the observed exit
outcome. Transport failure remains an explicit worker failure, and output backpressure delays
notification rather than dropping it. No queue/scheduler feed is needed.

The caller supplies monotonic observation time and its configured `stall_ms`;
the reducer never starts a timer, polls a process or kills/retries a turn.
Every stdout line, including an unknown line, proves progress but an unknown
event never changes the axes. A host-sent answer closes only its incoming
`request_id`; stdout control responses acknowledge host-initiated controls
and cannot close incoming permission requests. A CLI `control_cancel_request`
closes only its named request; a top-level result or process exit closes all
open permission/question requests, including an interrupted or errored turn.
A tool remains open past its
argument `content_block_stop`, until its matching `tool_result` or a top-level
`result`. No stall is reported while any tool is open, however long it runs.
Subagent results never end the parent turn. A late result ends a
stalled turn normally; the observation itself leaves the phase unchanged.
Unavailable background and registered-wait evidence stays `unknown`, so idle
alone cannot manufacture `finished`. Provider reset timestamps are Unix seconds
converted to an optional duration against receive-time Unix milliseconds;
missing, null, incompatible or overflowing reset metadata gives no duration.
Lifecycle comes explicitly from the registry and retains ADR-0028 precedence.
While attention is set, public `turn` is `unknown` per ADR-0028 §1; the
underlying turn boundary is retained for when attention clears. A known provider
limit outranks crashed attention while lifecycle remains live; a registry-owned
offline/hibernated/gone lifecycle outranks both.

Phase derivation is ADR-0028 §1's unchanged; only the inputs differ. A
question answered while its control request is still open becomes the
`control_response` (`behavior: allow`, `updatedInput` with `answers`), so the
one answer verb of §8a works for structured agents too. Registered waits
(ADR-0028 §5) wake a structured agent by queueing a `wake` item (§3) instead
of typing into a pane.

### 2a. Questions do not block, except honest blockers (#1301)

A question is a record, not a state the turn waits in, unless the agent has
nothing else to do.

- **Record and stable id.** Every ask becomes a question record in the
  agent's stream: `question.asked { question_id, source: session | mcp |
  recap | message, ask_point: message_uuid, prompt, options, multi_select,
  allow_other }`. `ask_point` is the boundary the question was asked at: for
  an `AskUserQuestion` or `flock_question_ask` call, the tool-result message
  that closes it (§9's cut rules allow a fork there); for a question the E9
  parsers (#1038, #1243) find in a turn's final text or `※ recap … Next: …`
  line, the final `assistant` message of that turn. Later states are
  appended as `question.answered { question_id, by, answer, via: in_place |
  fork | record }`, `question.forked { question_id, fork_agent }` and
  `reply.delivered { question_id, item_id }`. The ADR-0028 §8a kinds
  `agent.question_opened` / `agent.question_closed` are emitted to the
  global log as today, so `flk question list` and the TUI and web questions
  views read one source.
- **The id is a hash of the question, so it is stable across turns.**
  `question_id` is `q-` plus the first 12 hex digits of SHA-256 over the
  canonical JSON (sorted keys, no whitespace) of `{agent, prompt, options,
  multi_select}`, with `prompt` and each option trimmed and their internal
  whitespace collapsed. It contains no time, counter or ask point. The same
  question asked again by the same agent, in a later turn or after a
  reload, gets the same id and the same record (an `ask_count` is bumped,
  nothing new is opened). A re-ask of an answered question is answered at
  once from the record (`via: record`); an agent that wants a new answer
  asks a different question.
- **A fork is another agent, so its questions get their own ids.** `agent`
  stays in the hash so that an id names one record on the whole server,
  which the one answer verb (`question.answer { question_id }`, ADR-0028
  §8a) relies on: the same words asked in a fork open a new record with a
  new id. That costs the prompt cache nothing. A fork inherits its
  parent's transcript prefix byte for byte, so the parent's question ids
  and their rendered text are in the fork's prefix as they were, and the
  fork's own asks come after the fork point, where the two transcripts no
  longer share a prefix anyway. A question fork (`answers: question_id`,
  below) names the parent's id explicitly and never needs to re-ask it.
- **Byte-stable rendering.** Every text flk puts into the agent's context
  about a question is rendered from a fixed template with the id and the
  question's own text only: no timestamps, no counters, no ask count, no
  key order that can vary. The same question therefore yields the same
  bytes in every turn and resend of the agent, so a prefix that contains
  it stays a prompt-cache hit (E10), and a fork of the parent at or after
  the ask shares those bytes with it.
- **Hold or continue: the System-One check (owner decision 3a).** When the
  question policy (ADR-0034 §2) cannot answer an ask at once (a routed
  architect answering within `[questions] hold_ms`, as ADR-0034 §3 does),
  the server asks System One whether this is an honest blocker. It goes
  through flk's existing `[systemone]` client (the advisory client of
  `flk delegate`, `src/cli/delegate_s1.rs`, with its timeout and outage
  breaker) as one `choice` question, `hold` ("nothing in the open work can
  proceed without this answer") or `continue` ("some open work does not
  depend on it"), over a bounded state of:
  1. the last turn or turns of the agent (final `assistant` text and the
     tool calls, newest first, cut to `[questions] s1_state_bytes`, default
     4 KiB, the size flk's System One calls already send);
  2. the question record (prompt, options);
  3. the agent's open tasks and work packages and their dependency graph:
     its `TodoWrite` list (P15), its goal and its open sub-goals (ADR-0031),
     and the open delegates and forks it waits on, with their `depends on`
     edges.

  A `strong` `hold` holds the control request open (or, on the MCP path of
  §2b, keeps the tool call open): the turn blocks, the agent is
  `waiting_on_human`, and the answer, whenever it comes, is the tool
  result. Anything else (`continue`, `lean`, `uncertain`, System One not
  configured, timed out or broken) does not hold. Not holding is the
  default because a wrongly held turn costs the agent its turn, and a
  wrongly continued one costs only a later reply. `flock_question_ask
  { blocking: true }` (§2b) always holds and skips the check;
  `blocking: false` never holds and skips it too. Each check is recorded on
  the question (`hold_check { verdict, p, band }`) so that its calls can be
  rated against what the agent then did.
- **The placeholder result.** When the ask is not held, the tool call ends
  at once with the result text

  ```text
  <question-id> submitted for user feedback - the response will be relayed to you
  ```

  with `<question-id>` the stable id. On the MCP path (§2b) that is the
  tool's result. On stdio the harness only offers `allow` (with
  `updatedInput`) or `deny` (with a message) as the answer to an
  `AskUserQuestion` control request, and the carrier is `deny` with the
  text as its message (S0, A.12). The model then sees an `is_error` tool
  result whose content is the text byte for byte, goes on with the rest of
  its work in the same turn, and uses the reply when it comes (4 of 4
  runs). `allow` with the text as every answer kept the model working too,
  but the harness frames it as "The user answered: …", so it tells the
  model that the user replied, which is what this rule excludes. The §2
  carve-out applies: flk records the `tool_use_id` of every deferral, and
  neither that `deny` nor its entry in `result.permission_denials` is a
  permission denial. The turn continues, and the agent is `working` or
  `finished`, never `waiting_on_human` for a question it did not hold on.
  This is ADR-0034's `defer` verdict applied to a structured session.
- **Permissions always block.** Allowing a tool call later is not possible,
  so a `can_use_tool` for any tool other than `AskUserQuestion` stays open
  until `hook.decide` answers or the operator does. Besides a held question,
  that is the only `waiting_on_human` a structured agent has.
- **Answer in place.** `question.answer { question_id, answer }` on a held
  question answers its open control request or tool call. On a deferred
  question it queues one `reply` item (`anchor: question_id`, `delivery:
  at_idle`) whose body is the fixed template `Answer to <question-id>:
  <answer>`, and records `question.answered{via: in_place}`.
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

### 2b. Two question channels: stdio first, an MCP tool for richer asks (owner decision 3)

Both channels end in the same question record of §2a and the same answer
verbs; they differ in what the agent can say and in what can block them.

- **stdio (`--permission-prompt-tool stdio`) is the primary path.** It
  carries every permission request and the harness's own
  `AskUserQuestion` (E7: it is the only path that keeps that tool), needs no
  extra process, and keeps the server the only owner of the session's I/O.
- **The MCP tool is the richer path.** flk's MCP server, which every
  structured agent has through flk's `--mcp-config`, serves
  `flock_question_ask` (ADR-0034 §3) with the fields `AskUserQuestion`
  lacks: `blocking: true | false` (absent means the §2a check decides),
  `context` (markdown), `refs` (blob refs and file paths), `waits_on`
  (task, work package or issue ids, which feed the §2a check),
  `recipient` (`operator | architect | auto`, routed per ADR-0034 §2) and
  `urgency`. It is listed in the launch profile's `--allowedTools`, so its
  call never goes through a permission prompt. The compiled context
  (ADR-0032) tells the agent to use it when an ask needs any of those
  fields, and `AskUserQuestion` for a plain choice.
- **When stdio can block, and what flk does about it.** The owner's concern
  is right in three cases:
  1. *An open permission request.* It blocks the turn by design (§2a), and
     S0 observed that it also blocks everything after it in the same
     message (A.13): while a `can_use_tool` was open, no later tool call of
     that message ran, whether it prompted (a second `touch`), asked
     (`AskUserQuestion`) or was pre-allowed (an MCP tool without a
     read-only hint, so `flock_question_ask` too). Only one control request
     was ever open, and a pre-allowed call ordered before the prompting one
     ran first. A concurrency-safe `Read` after the prompting call did not
     run either until the permission was answered (A.13). Separately, with
     no permission open, a pre-allowed MCP call still waited for a slow
     pre-allowed Bash call ordered before it (A.13): an unfinished
     non-concurrency-safe call holds later ones too. So a question that
     must not wait behind a permission has to be asked before the call
     that may prompt, or in a separate message, and the compiled context
     (ADR-0032) tells the agent so. The MCP tool still never prompts
     itself, but it gives no way past a prompt that is already open in the
     same message.
  2. *flk not reading stdout.* A control request waits for flk. The stdout
     pump (S3) is its own task that never awaits a client, a System One
     call or a disk flush to read the next line; questions are answered or
     deferred within `[questions] hold_ms` plus the System One timeout, and
     a pump that falls behind is reported as `agent.session_stalled` (§1).
  3. *The stdio prompt path disappears.* `--permission-prompt-tool stdio`
     is hidden (A.4). The capability probe of §9 checks it too. If it is
     gone, the launch profile switches to `--permission-prompt-tool
     mcp__flock__permission_prompt` (E8: an MCP permission tool works),
     `AskUserQuestion` is lost with it (E7), and `flock_question_ask` is the
     only question path.

### 3. One server-owned input queue per agent

Every input to a structured agent is an item in that agent's queue, which is
an event-log stream (§4) named `queue/<agent>`. Humans and agents feed the
same queue (owner decision 5): there is no second input path.

```text
{ item_id (uuid; becomes the stream-json message uuid and command_uuid),
  producer: human | agent:<id> | channel:<binding>/<user> | system:<source> | fork:<agent>,
  kind: prompt | message | reply | delegate_result | wake | reload_notice | finding
        | command (a harness slash command such as /compact)
        | continuation (the prompt that follows a command, §3a),
  anchor?: { item_id | correlation_id | question_id | fork_point },
  priority: normal | urgent,          ordering only: which waiting item goes first
  delivery: at_idle | steer | interrupt,   timing only: when the item is written
  state: queued | held | stashed | delivered | done | cancelled | dropped | failed,
  (queued → delivered → done | cancelled; queued → held | stashed | dropped;
   held | stashed → queued; queued → failed with a typed refusal;
   delivered → held when the process dies without a result)
  stash?: name,                       which stash a stashed item is in
  edit_lease?: { client, until_ms },  human producers only; skipped while it holds
  batch_id?: uuid,                    the message the item was written in
  relevant_now?: { p, band },         the last relevance check (below)
  body, attachments: [blob refs] }
```

- **One gate.** Each item passes `queue.gate` once: `send` (deliver per
  `delivery`), `hold` (until a human releases it), `stash` (keep, never
  deliver unless popped) or `drop`. The gate is the policy point;
  ADR-0035's rows and ADR-0034's question policy plug in here.
- **Delivery and priority are independent fields.** `delivery` alone says
  *when* an item is written (E4, A.14). `at_idle`: only after the current
  `result`. `steer`: at once, as a plain write; the harness reports it
  `queued` and folds it into the running turn at the next model step, so it
  waits out a running tool (7.25 s behind an 8 s `sleep` in A.14 "plain"),
  and it can change what the running turn answers (in that run the turn's
  one `result` answered the steered item, and the reply the first input
  asked for was never written). `interrupt`: flk writes the item and sends
  `control_request{interrupt, send_now: true, message_uuid}`, the
  harness's own send-now; the running turn stops at once, a tool that is
  mid-run included (it is rejected and its side effects so far stay), the
  turn ends `error_during_execution` with `terminal_reason: aborted_tools`
  (or `aborted_streaming` while text streams, A.5), its inputs are
  `cancelled`, and the item starts as a new turn (A.14 "send_now").
  `priority` alone says *in which order* items that are waiting
  for the same delivery point are written: all `urgent` items before all
  `normal` ones, each group in queue order. Priority never changes the
  delivery mode, and no value of one implies a value of the other: an
  `urgent` `at_idle` item is written first after the next `result`, never
  into the running turn. Defaults (owner decisions 4 and 5): every item,
  a human prompt included, is `at_idle` + `normal`; `steer` is what
  "send now" sets; `interrupt` is explicit only. An idle agent with an
  empty queue takes an `at_idle` item at once, so a prompt typed at an idle
  agent is not delayed.
- **One batch per turn, FIFO across producers (owner decisions 4 and 5).**
  Queue order is arrival order until someone moves an item. At idle the
  server writes one batch, waits for that batch's `result`, then writes the
  next. The batch is built from the deliverable items (`queued`, `at_idle`,
  gated `send`, no live `edit_lease`, and for a `continuation` its anchor's
  `result` seen), `urgent` first, then in queue order:
  1. The head goes.
  2. If the head is a human item, every human item directly behind it goes
     with it, up to the first item that is not human (the bulk submit at
     idle). A human item open in an editor is skipped: it stays `queued`
     and the run continues past it.
  3. An agent, channel, system or fork item goes alone: agent messages and
     human messages are never in one batch, and two agent messages are
     never in one batch, unless the relatedness check below joins them.
  4. A `command` item always goes alone, since a slash command cannot share
     its turn.

  A batch is written as one stream-json `user` message whose content holds
  one text block per item, each rendered from a fixed template that names
  its producer (and, for a reply, its question id), under a new `batch_id`
  that every item in it records; a one-item batch uses the `item_id` as its
  uuid, as before. `command_lifecycle` for that uuid moves all its items
  together. Up to `[queue] max_batch` (default 8) items go in one batch.

  **Command exception (architect decision for #1802, 2026-10-17).** A harness
  slash-command item is sent **verbatim**, as the sole text block of its own
  isolated turn, without a producer label and never batched with another item.
  The label would break command parsing. Attribution remains in the queue item
  and its audit/lifecycle records. Only an operator may create command items:
  the operator composer or `flk agent send` authenticated as operator/token
  principal. Text from agents, MCP or relayed peers that starts with `/` remains
  ordinary input, wrapped in the normal producer-labelled template, and cannot
  become a harness command. Neither a slash prefix nor a claimed producer in
  request JSON grants operator authority.
- **Relatedness joins (owner decision 5).** After rules 1 to 4, when
  System One is configured (§2a's `[systemone]` client), the server asks
  it, in one call, one `noul` per candidate among the next
  `[queue] related_window` (default 4) deliverable items that the batch
  does not already hold: "is this item about the same matter as the head, so that one turn
  should answer both?". Candidates answered `strong` yes join the batch, in
  queue order. Anything else, or no System One, leaves the batch as rule 1
  to 4 built it. The check is recorded on the batch so it can be rated.
- **Relevance to the running turn (owner decision 4).** While a turn runs,
  at each model step that ends in a tool call and at most every
  `[queue] relevance_every_ms` (default 20 s), the server asks System One,
  in one call, one `noul` per queued human or agent item: "would this item
  change or inform what the agent is doing right now?". The state is the
  running turn's last `assistant` text and tool calls plus the items, cut to
  `[questions] s1_state_bytes`. The answer is written to the item's
  `relevant_now`. `[queue] relevance = off | suggest | force` (default
  `suggest`): `suggest` lets clients mark the item and offer "send now";
  `force` also sends `strong` items now, as `send_now` does. An item that
  is being edited is never forced.
- **Queue operations (owner decision 4).** All on the socket API and as
  `cmd` verbs (§5), scope `send`, for items still `queued`, `held` or
  `stashed`:
  - `queue.update { item_id, body?, attachments?, priority?, delivery? }`
    edits an item.
  - `queue.edit { item_id, open }` takes or releases an edit lease for one
    client (`[queue] edit_lease_ms`, default 10 min, renewed while the
    editor is open; a lease that lapses releases itself), so a closed tab
    never holds an item forever.
  - `queue.move { item_id, before? | after? | to?: head | tail }` reorders.
  - `queue.delete { item_ids }` drops items (state `dropped`, `by`
    recorded).
  - `queue.stash { item_ids, stash? }` moves items into a named stash
    (default `default`); an agent has any number of stashes, each keeping
    its items' order. `queue.pop { stash, item_ids?, to?: head | tail }`
    moves a whole stash, or some of its items, back to `queued` at the
    head or tail (default tail). `queue.clear { stash }` drops a stash's
    items. `queue.stashes {}` lists stash names with counts and first
    lines.
  - `queue.send_now { item_ids, interrupt? }` is the forced send: the items
    get `delivery: steer` (or `interrupt` when `interrupt: true`) and are
    written at once, in the order given, as one batch. Without `interrupt`
    the batch folds into the running turn at its next model step, with
    the costs of `steer` above; with it, the running turn stops through the
    harness's send-now.
  - `queue.take_back { item_id }` withdraws a `steer` item that the
    harness still reports `queued` (below).
- **The harness's own queue (S0, A.14).** A written item stays `queued` in
  flk until its `command_lifecycle{started}` (E3); in between it sits in
  the harness's queue. The harness has a take-back there and no edit:
  - *Take-back.* `control_request{cancel_async_message, message_uuid}`
    withdraws an input that is still `queued` (`cancelled: true`, then
    `command_lifecycle{cancelled}`) and answers `cancelled: false`, doing
    nothing, once it has `started`. `queue.take_back` wraps it. On
    `cancelled: true` the item stays `queued` in flk's queue with
    `delivery: at_idle` and is editable again, and the harness's
    `cancelled` for that uuid does not cancel the item; it is written again
    later in a new message (a new `batch_id`), never under the uuid the
    harness cancelled, since whether the harness accepts that uuid again
    was not probed. On `cancelled: false` it is refused as too late and the
    item runs. Items written in one batch share its uuid, so a take-back
    of one of them withdraws the whole batch and every item in it returns
    to `queued`.
  - *Edit.* No control subtype edits a queued input, and an unknown subtype
    is answered with an `error` naming it. flk's queue is the only edit
    surface: an item is editable until it is written; once written, only a
    successful take-back makes it editable again.
  - *Send-now* is `interrupt{send_now: true, message_uuid}`. It is an
    interrupt, so flk uses it for `interrupt` delivery (above); a `steer`
    write needs no control request. Its response says
    `stopped`, `delivering` (the input was already going) or
    `nothing_waiting`. `interrupt{cancel_queued: true}` cancels every
    input in the harness's queue and interrupts the running turn too, so
    flk never uses it for a take-back.
- **Acknowledgement (E3).** `command_lifecycle` drives the item state, so
  "delivered" means the harness started it, not that bytes were written.
- **Pending turns (architect revised decision, #1896 review).** Only a real
  result consumed by the queue owner or runtime exit/error ends its reservation.
  Harness cancellation keeps the items cancelled while waiting for the result;
  operator take-back (§3 above) is a separate operation, not implemented by this
  worker slice. Indeterminate loss retains the S3a writer guard. Status reports
  the batch ID, original reservation time and an explicit cause: `awaiting_result`,
  `turn_end_unknown` or `cancelled_awaiting_result`. A refused uncertain offer
  writes no stdin and restores its original queued items instead of holding them
  as `input_write_failed`. A real result or runtime exit/restart recovers the
  pending state; restart holds unresolved items `session_unconfirmed`, never
  automatically resending them. No queue timeout is inferred from pending age.
  Stall observation belongs to the session state/supervision policy.
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

- **Input framing (architect decision, #1885 review).** Every non-command
  item uses a single header line `From <producer> (item <item_id>):` with
  reply metadata and `Attachments (blob refs): <JSON refs>` on that line.
  Producer/attachment values are JSON-encoded so embedded newlines remain
  escaped, including U+2028, U+2029 and U+0085. Attachment strings must be
  content-addressed `blob:<sha256>` refs: exactly 64 lowercase hex digits
  after `blob:`. Reply `question_id` must be a canonical lowercase hyphenated
  UUID. The next line is `<<<flk-item <nonce>`, followed
  by the literal body and a final line `flk-item <nonce>>>>`. Here `<nonce>`
  denotes 128 random bits encoded as 32 lowercase hex digits. Generate a
  fresh nonce per rendered item and regenerate if its hex string occurs
  anywhere in the body. Sender text, including fake labels, attachment lines
  or delimiters, remains inside that item's block. Attachments and reply
  metadata never enter the body block. The operator-only native command
  exception in §3 remains verbatim and isolated. Admission and replacement
  validate anchors and refs, refusing `invalid_request` before writing.
  Rendering validates again: an invalid head becomes durably `failed`
  with its typed refusal, allowing subsequent items through. A valid prefix
  is delivered before a later malformed item is failed.
- **Trigger.** The turn end is the `result` event (E1). An armed self-compact
  (`flock_self_compact`, `flk pane arm-self-compact`) queues a `command`
  item with body `/compact`, `delivery: at_idle`, `priority: urgent`. The §3
  command exception applies to direct command items: operator authorization
  is required, the body is sent verbatim alone, and attribution stays in
  queue/audit records. The agent-initiated self-compact authorization path
  belongs to S5c (#1879); this admission slice does not implement or grant it.
  An agent's recap or supervisor text cannot manufacture that authority. By
  §3, `at_idle` means it is never written into a running turn, and `urgent`
  means it is the first item written after that `result`, ahead of any
  `normal` `at_idle` item. Background shells, held waits and subagents do not matter, because
  nothing reads a pane status.
- **Continuation.** The post-compact continuation (the goal re-inject and
  status of #998, or a supervisor's prompt) is a `continuation` item
  (`delivery: at_idle`, `priority: urgent`) with `anchor: { item_id: <the
  command item> }`; it
  becomes deliverable when the compaction's own `result` arrives
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
  file. When the file passes `[sessions] queue_rewrite_bytes`, serialize the
  rewrite with appends. Let H be the last committed seq before the rewrite
  and N the number of snapshot lines. Write a `queue_checkpoint` header
  carrying `high_water: H + N`, followed by full-state snapshot lines with
  seqs H+1 through H+N, to a temporary file; fsync it, atomically rename it
  over `queue.jsonl`, and fsync the containing directory before publishing.
  The header is storage metadata, not an `ev`, and consumes no seq. Recover
  the next seq at startup from `max(header.high_water, appended event seqs)+1`.
  Thus a zero-line rewrite persists H in the header and the next append is
  H+1 even after restart. Readers must not treat a checkpoint header as a
  queue item. No separate counter file can race the atomic replacement.
  A retained checkpoint and subsequent changes are one consistent replay cut.
  A subscriber needing the checkpoint gets
  `gap { from: cursor + 1, to: H, snapshot_to: H + N }`, followed inline by
  its snapshot lines and subsequent retained changes. For N>0, H is also
  `first_snapshot_seq - 1`; for N=0 there is no first line and `to` is still
  H, the last seq before rewrite. `snapshot_to` is the only completion
  marker. On an empty snapshot (`snapshot_to == to`), clear that stream's
  entities and advance its client cursor to `to` immediately; wait for no
  `ev`. An initial empty stream uses `from:1, to:0, snapshot_to:0` (an empty
  range, not a lost event). Checkpoint publication gives active subscribers
  the same gap/snapshot batch, so removals cannot hide behind upserts.
  Retain the latest checkpoint through the next atomic replacement; replay
  must not interleave its lines with a newer checkpoint or concurrent appends.
  Other materialized snapshot streams persist their seq high-water with their
  checkpoint by the same atomic rule. Wire journal, queue and checkpoint
  writes obey §5a's single-writer handoff rule, including seq allocation and
  rename-rewrite; process overlap never grants simultaneous write authority.
  `agent/<id>` and `events` have no
  entity snapshot: a gap there reports lost history without `snapshot_to`.
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
- **Not in this cut** (owner decision 10): sealed zstd segments, SQLite for
  hot state, a DuckDB query layer, and a thinking retention policy.

### 5. The client wire contract

**Architecture (owner decision 6).** A structured agent runs `claude -p`, so
there is no terminal and nothing to render on the server: flk ships no
terminal frames, cell grids or repaints for it, and none of the xterm
bridge's machinery applies. What travels is the session's JSONL messages
(§4) and flk's state about it (§2, §3), live as they happen, plus a pull for
history from any cursor. A client is a renderer of those messages and that
state; it holds no session logic and computes nothing the server has not
sent. Between fleet nodes the mesh (ADR-0026) carries the same two things,
state and messages, and nothing else: a client on node A that follows or
commands an agent on node B talks to A's server only, A gets B's state and
messages over the mesh, and a `cmd` for B's agent travels as a mesh message.
No client hops into or out of another node's server.

That mesh message does not exist yet. Mesh VERSION 5 envelopes have two
kinds (ADR-0026; `Kind` in `src/mesh/store.rs`): `message`, mail for an
agent's inbox that is delivered as text, and `receipt`, a delivery outcome
sent back. Neither runs a verb: a `cmd` sent as `message` would reach B's
agent as text, not as a `queue.update` or `question.answer` with its scope
check and `ack`. A new `command` kind cannot be added within VERSION 5:
`Kind` is a closed enum, so a VERSION 5 node fails to decode an envelope of
a kind it does not know, and the mesh hello requires identical versions
(ADR-0026, "Rollout"). It needs a VERSION bump with ADR-0026's
coordinated upgrade, tracked with its design (how B authorises a principal
that A authenticated, how the `ack` comes back) in #1380. Until then a
`cmd` whose agent is on another node is refused with
`unsupported_for_agent` before anything is sent. The one exception is
opening a real terminal to a session, which is a `pty` agent's pane (the
`terminal/<pane>` stream) served by the node that runs it, as today.

The stream and its history stay the JSONL store of §4 and the endpoint
below. The open item of whether they should run on the Grafana Labs stack
(#1323) was resolved by the owner on 2026-10-16 (ADR-0038 §11 item 5):
flk's log, metric and trace output goes through vig-os/vigil's OTLP/JSON
Lines files and its optional signed audit trail, not through Grafana and not
through a separate pipeline. The integration is #1614. #1323 no longer gates
S7; #1546 (liveness and recovery on unstable connections) does.

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
  sub     { sub_id, stream, after? }          fresh sub_id on this connection;
                                            stream ∈ agent/<id>, agent/<id>/live,
                                              queue/<id>, events, terminal/<pane>,
                                              fleet, inbox, artifacts, work
  pong    { nonce }                          echo the server ping nonce
  unsub   { sub_id }                         closes id; drop late frames for it
  cmd     { cmd_id, verb, args, resume_only } cmd_id is the idempotency key
server → client
  welcome { protocol, server_version, capabilities, principal,
            server_epoch, idempotency_window_ms }
  ping    { nonce }                          application heartbeat (§5a)
  ev      { sub_id, stream, seq, ts_ms, ev }  ev is the typed event (§6)
  synced  { sub_id, seq }                    replay barrier on every seq stream
  subfail { sub_id, code, message }          closes id; drop late frames (§5a)
  gap     { sub_id, from, to, snapshot_to? }  inline checkpoint (§4)
  lagged  { sub_id, skipped_to }              closes id; fresh sub, no unsub (§5a)
  ack     { cmd_id, ok, result? | error{ code, message, exit, retry } }
```

Agent replay preserves the receive-time `known`/`shape` provenance of §5a/§8.
Legacy records without it produce `provenance_missing` history gaps while later
records continue. Replay never reclassifies using the current harness version.

**S7-min fleet and permission contracts (#1746).** The `fleet` snapshot stream
lists structured agents as `FleetAgentRow { id, label, state: AgentState,
activity?, waiting_on_human: string[] }`. `AgentState` is the shared S4 object,
including axes, phase and sources, not a bare phase. Its per-field `or_unknown`
reading follows ADR-0028 §3a: an unknown newer phase becomes `unknown` while the
other axes survive; it must not discard the fleet delta. Snapshot rows use §5a's
existing inline gap/checkpoint path. Fleet `ev` payloads are the closed tagged
`FleetDelta` union: `{ type:"fleet.upsert", row: FleetAgentRow }` or
`{ type:"fleet.remove", id }`, rather than session `WireEvent` payloads.

S7-min slice 6a (#1832) supplies only the provider core: structured registry
identity/label, S4 state, current activity and S6 waiting IDs project into these
rows. Its exclusive-writer store atomically persists rows and high-water with
file fsync, rename and directory fsync before returning changes or checkpoint
cuts. Snapshot reads allocate dense positions for their complete upserts; empty
snapshots preserve the existing head, including across restart. Invalid row
entries are skipped, warned and retained raw, while invalid cursor metadata
refuses loading rather than resetting it. Any uncertain write requires reopening
the store before another publication. Slice 6b on #1832 wires the provider into
subscriptions and serializes each checkpoint batch with live deltas; 6a has no
product caller or transport. Before serving after cold boot, 6b refreshes the
registry/S4/S6 provider cut; persisted observation state grants no process lease
or resume authority. Local file locking does not implement S7-rest's single-writer
handoff protocol.

`cmd agent.decide` takes `AgentDecide { agent, request_id, decision, message? }`,
where decision is `allow_once`, `allow_always` or `deny`. Only a token-authenticated
client principal may call it. The existing ack envelope echoes `cmd_id`; success
returns `AgentDecideResult { request_id, decision }`, echoing the applied answer.
Decision-specific failure names are `permission_request_unknown`,
`permission_request_closed` and `not_authorized`, with retry class `never` in the
existing error envelope. Their API error-code registration and command behavior
are deferred to S6 (#1429); this types slice names the contracts only.

- **Cursors and bootstrap (architect decisions iii/iv).** Every stream except
  `agent/<id>/live` has seqs, including `terminal/<pane>` and `events`.
  `sub` with `after` replays from the last applied cursor, then goes live.
  A first `sub` without `after` on a snapshot stream (`fleet`, `inbox`,
  `queue/<id>`, `artifacts`, `work`, `terminal/<pane>`) gets an inline checkpoint
  through the same path as recovery: `gap { from:1, to:first_snapshot_seq-1,
  snapshot_to:last_snapshot_seq }`, or §4's empty-snapshot form. No separate
  bootstrap fetch or client reducer exists. If no checkpoint exists yet,
  create and persist one under §4's serialization rule before serving it.
  Streams without entity snapshots replay retained history from the beginning
  when `after` is absent, reporting any missing prefix as an ordinary gap.
  Send `synced { sub_id, seq }` exactly once on **every seq stream** per
  subscription, after replay through a captured head, including all announced
  snapshot lines. The barrier's seq is that head (zero only for a genuinely
  new empty stream); all later frames are live. `lagged` closes the old ID;
  re-subscription with a fresh ID gets its own barrier without an `unsub`.
  Empty-checkpoint completion advances to `to` before `synced`; a nonzero
  high-water never becomes zero merely because no items remain. Delivery is
  at-least-once; dedupe on `(stream, seq)` and persist a cursor only after
  applying its event or empty checkpoint. Persist a pending
  `snapshot_to` and unconfirmed-entity set with the cursor across disconnects.
  Resume unfinished snapshots before declaring Current. A new gap supersedes
  an old resync marker and marks all entities unconfirmed again.
  `agent/<id>/live` has no cursor or `synced`; discard in-flight partial blocks
  after a drop and render from the next complete persisted `assistant` event.
- **Commands.** `cmd` verbs are the socket API's own (`queue.add`,
  `queue.update`, `queue.gate`, `agent.interrupt`, `question.answer`,
  `agent.fork`, `agent.reload`, …), with the same params types. The server
  keeps `cmd_id`s for `[wire] idempotency_window_ms` (default 24 h); a
  repeat returns the first `ack`, subject to the durable recovery rules in
  §5a. Errors use ADR-0029 §4's envelope and
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
  `operate`-scoped token through `flk client open`, which runs over the
  unix socket (so `caller_is_local_operator()` and the uid check of §1a
  apply), mints a one-time code, and opens the client in a new browser tab or
  window; the client
  exchanges the code for its session token once. The code contains 256 bits of
  randomness encoded as 64 lowercase hex characters, is bound to that token,
  exists only in server memory, is single
  use, and expires at age 60 seconds. `flk client open --print` prints the
  loopback URL rather than launching the browser. The code is carried only in
  the URL fragment (`#code=…`), never the query, so it is absent from HTTP
  request targets and Referer. The browser consumes that fragment and sends
  `POST /v1/client/exchange` with JSON `{ "code": "…" }`; the loopback-only,
  same-origin endpoint returns `{ "token": "…" }` exactly once, with
  `Cache-Control: no-store`. Used, expired and unknown codes get one uniform
  401. There is no global failed-exchange gate: every caller reaches the
  loopback listener locally, and local processes can forge Host/Origin.
  Such a gate would let any local process, including an agent, lock out the
  operator. Guessing a 256-bit, single-use code with a 60-second lifetime is
  infeasible. Keep the store bounded at 128 live codes and request bodies at
  1024 bytes. Cross-origin and non-loopback requests are refused, and neither
  codes nor tokens enter diagnostic logs.
  The opener's URL argument is an explicit credential-in-argv exception:
  `open`/`xdg-open` have no stdin or environment URL channel. The exchange code
  is visible through `ps` for the opener's lifetime. Single use, the 60-second
  TTL and fragment-only transport bound this known exposure; process logging
  masks the code. This exception does not permit long-lived tokens in argv.
  The token then travels in the unchanged normal `hello` frame.
  The served client derives its server and transport from its own location
  (same-origin `ws://<host>/v1/ws`); the bundle router serves no config script.
  `token add` prints the bearer only to terminal stdout unless `--json` or
  `--show-token` explicitly permits redirected secret output. Without that
  consent it refuses before issuance, so no undisclosed credential is created.
  Tokens follow the channel-token model of ADR-0028 §8
  (`src/channels.rs`): 32 random bytes as 64 hex digits, one file per client
  under `<data dir>/client-tokens/<name>.token`, mode 0600 in a 0700 dir,
  compared as SHA-256 digests in constant time, never logged, revoked by
  deleting the file. Each token carries scopes `read`, `send`, `answer`,
  `operate` (`operate` is what the operator's own client holds). Only the
  local operator (`caller_is_local_operator()` after §1a's uid check) can
  issue or revoke one (`flk client token add|rm|list`; else
  `operator_only`, exit 13), and verbs that are operator-only under ADR-0014's
  authority boundary (and ADR-0035's owner rules) stay operator-only
  whatever the token says unless it is `operate`-scoped. Stated plainly: an
  `operate`-scoped bearer token *is* the operator's identity for ADR-0014's
  operator-only verbs, from any address, and once `--allow-non-loopback`
  is on that includes other machines (owner decision 9). Token files live in
  the operator's data dir, which `flk-agent` cannot read (ADR-0035 §6.3), so
  structured agents cannot pick up a client token. `pty` agents that still
  run as the operator's uid until O19a can; that limit is #1177's.

### 5a. Transport liveness and recovery (#1546, #1641)

**Amendment accepted by the architect, 2026-10-16.** The snapshot/bootstrap,
all-seq-stream `synced`, empty-checkpoint cursor, persisted high-water and
S7 no-compression rules are decided. The values below are **proposed
parameters for review**, not measured performance claims. Keep WebSocket as
one transport for the `-p` surface and GUI; recovery is required in S7, including
local use, and its types precede G3 in S7a. No deployment is authorized here.

| Parameter | Proposed value | Rationale |
| --- | --- | --- |
| Connect deadline | 10 s per attempt, from attempt start through name resolution, TCP/TLS and HTTP upgrade | Bounds a black-holed route without waiting for operating-system timeouts; allows mobile connection setup. |
| Handshake deadline | 5 s from WebSocket open to authenticated `welcome`; server closes if valid `hello` is absent within 5 s | Hello is local validation, not model work. Neither side can hold an unauthenticated socket indefinitely. |
| Handoff deadline | 10 s from handoff request, across producer quiescence, claim settlement, flush and readiness for exclusive transfer | This is local coordination, not model work. Bound a stuck producer or flush without releasing ownership; fail and continue serving from the old owner rather than half-transfer. |
| Heartbeat interval | 15 s from `welcome`, independent of other traffic | Detects suspended or half-open peers even during long quiet turns; four round trips/minute per connected client. |
| Liveness window | 45 s | Three heartbeat intervals tolerate brief scheduling delays while bounding stale display and server resource retention. |
| Subscription progress deadline | 30 s without a frame advancing that subscription while awaiting synced or snapshot completion | Large replays may take longer overall; unrelated traffic and heartbeats cannot hide a stalled stream. Only that stream re-subscribes. |
| Command acknowledgement deadline | 30 s from each send | Marks that command uncertain while keeping the connection up; resend only after the next reconnect. Long work acknowledges admission and publishes completion separately. |
| Reconnect backoff | base 500 ms, factor 2, cap 30 s, equal jitter in [base/2, base] after applying cap | Avoids fleet reconnect storms and zero-delay loops, while the first retry is quick. |
| Stable connection before resetting backoff | 60 s continuously authenticated and live/read-only | An open socket followed by immediate failure must not reset storm protection. |
| Resume retention | Entire session JSONL until explicit S16 retention; latest checkpoint plus later changes for materialized streams | No arbitrary time-based replay loss. Removed history uses gap, never a silent jump. |
| Command idempotency window | 24 h from first admission (existing §5 default) | Covers sleep/offline recovery; does not promise indefinite replay safety. |

These are failure ceilings for a single operation, not evidence of completion.
Use monotonic clocks while running. Suspend/foreground invalidates the old
connection instead of interpreting a frozen clock as continued liveness. This
uses ADR-0037's principle that missing evidence fails rather than hangs; that
ADR's test-binary ceilings do not set runtime timeouts. No condition is made
true by expiry, a retry, a heartbeat or elapsed time.

**Heartbeat direction and shape.** Use application JSON
`{"t":"ping","nonce":"<opaque unique nonce>"}` server → client and
`{"t":"pong","nonce":"<same nonce>"}` client → server. A bounded nonce
identifies one outstanding ping; no shared wall clock is required. Retain the
few outstanding nonces within the 45 s window, not an unbounded history. The
client responds at once, without waiting for rendering, a model or a command.
Application frames are chosen over WS control ping because browser JavaScript
cannot observe control ping/pong to drive the same visible lifecycle
([WebSockets Standard](https://websockets.spec.whatwg.org/#ping-and-pong-frames)). Native
clients use the same JSON contract. Control ping/pong may be handled by the WS
library but does not replace these checks. Invalid/replayed nonces do not renew
server liveness. The server closes and releases per-client subscriptions when
no valid pong has arrived for 45 s (initial baseline: `welcome`). The client
enters Stale when no valid server frame, including ping, has arrived for 45 s,
marks retained content stale, holds its outbox, closes and enters Reconnecting.
Prioritize ping/pong ahead of queued replay/data messages in both directions.
The writer sends bounded messages with bounded buffering, checks the priority
queue between writes, and never enqueues an entire replay ahead of a heartbeat.
A blocked socket write remains subject to the liveness failure ceiling; priority
cannot cure a genuinely unusable link. This prevents a healthy slow replay from
causing false Stale merely because the heartbeat waited behind its data backlog.
Discard every frame from the abandoned socket, including late pings; send no
pong for them and let no old callback advance a cursor or renew liveness.
A stream's independent progress deadline still applies during traffic.

**Reconnect and failure scope.** Connect/handshake expiry, protocol failure,
socket loss or heartbeat failure ends the connection attempt and enters
Reconnecting, preserving displayed state and applied cursors. Subscription
progress and command-ack expiry never tear down a healthy connection.
One attempt is active at a time. Backoff attempt n waits a uniformly sampled
value in `[min(30s,500ms*2^n)/2, min(30s,500ms*2^n)]`, n starting at zero;
use saturating arithmetic. A network-change or foreground event cancels the
backoff and triggers one immediate attempt; coalesce repeated signals while an
attempt is active. Foreground first closes a possibly suspended old socket.
Retries of connection setup continue under the cap until user disconnects;
each attempt ends at its deadline, with the latest failure visible and a
user disconnect action always available. This deliberate background reconnect
policy does not retry a condition assertion or declare a command successful.
WS close **4401** means auth refused during hello; **4403** means token revoked
mid-session. Both enter NeedsToken, holding the outbox without reconnecting until
a new token arrives. These codes differ from protocol failures (visible error,
capped reconnect) and network loss (reconnect). A verified socket whose
post-registration token recheck misses the handshake deadline or loses its store
worker closes with **1001**, preserving the credential for reconnect. A failed
welcome send drops the transport without attempting an auth close; it cannot
establish revocation. Only a credential that no longer authenticates, a changed verified principal,
or a revocation signal justifies **4403** after registration.
Frames for a closed subscription ID are silently dropped. A frame for an ID
never opened on the current connection is a protocol error, not a closed-ID case.
Protocol errors close with a visible diagnosis and capped reconnect (a server
upgrade can cure them). A newer client follows §5's ReadOnly capabilities and
holds commands; it can keep reading and detecting stale transport.

**Session replay provenance (S7-min slice 3, #1829).** New session-store
`StreamRecord`s add optional `known` (boolean) and `shape` (the original
`ShapeKey` string). The receive-time adapter sets `known`; unknown events retain
the original sender's harness/version-scoped `shape`. Known records keep their
stored blob projection. Readers ignore additive envelope fields under ADR-0028
§3a. Replay never invokes the current parser to reconstruct classification or
shape. A legacy record without classification, or an unknown without its shape,
yields a typed history gap with reason `provenance_missing`; later records still
replay. It is neither a whole-replay refusal nor an entity snapshot. A missing
retained prefix uses the same gap path with reason `retained_prefix`.

The registry's runtime stream owner registers bounded continuation and captures
its durable head under one publication lock. An owned file cut pages through that
head before exactly one `synced`; subsequent publications cannot fall between
registration and the barrier. Capture and registration are one operation over
the locked book; constructing the owned cut afterwards cannot open a hole.
Replay retains unread page bytes. Runtime-only, source-fenced byte checkpoints
let a consumer resume its applied cursor after lag without rescanning the prefix;
they are neither new wire fields nor durable cursor authority. Lag terminates
unfinished replay on its next page. Terminal permission audits use this same source,
even when runtime notification delivery is cancelled. Partial deltas consume no
durable cursor and do not enter this source. The S4 reader remains singular.

**Replay and resync.** Reconnect authenticates again, then sends each seq
stream's `sub { after:last_applied_seq }`, preserving any unfinished resync
marker. Retained events are replayed in order before `synced`, and mutations
concurrent with replay follow the captured barrier without a hole. `lagged`
closes its old ID, like `subfail`: the server sends nothing more on that ID and
the client silently drops late frames for it. Re-subscribe with a fresh ID from
the applied cursor, never `skipped_to`; no `unsub` is needed for the closed ID.
A snapshot gap uses §4's inline checkpoint; no snapshot gap may omit `snapshot_to`.
A history gap on `agent/<id>` or `events` leaves an explicit missing-range marker
and continues from `to+1`, without implying complete history.

`terminal/<pane>` is a **snapshot stream**, not history-only. Its snapshot
contains the complete terminal frame and rendering state for that pane; keep its
seq high-water and checkpoint under §4's single-writer atomic checkpoint rule.
Initial subscription and a gap both use
`gap { from:cursor+1, to:H, snapshot_to:H+1 }` followed by one complete frame
at H+1 (initial `from` is 1). Subsequent deltas follow it; a missing/removed pane
gets `subfail { code:"unknown_stream", ... }`, not a fake empty terminal.
Do not clear stale visual state until the complete frame has been applied;
a delta cannot reconstruct a missing base. As for other snapshots, synced
follows its completion and never substitutes for the frame. Cursors and
identity survive reconnect; neither a new socket nor a new server epoch resets
seqs. A cursor ahead of the restored stream head is refused as `cursor_ahead`,
not clamped or treated as caught up. On ordinary reconnect, re-subscribe only
the cursorless `agent/<id>/live` stream without `after`. Terminal recovery always uses its
full-frame snapshot path; on cursor_ahead it clears that stream's state/cursor
and bootstraps without `after` as specified below, never starts from a delta.

**Subscription identity (architect decision, 2026-10-17).** Every `sub`
uses a fresh `sub_id`, unique for the lifetime of that connection. IDs are
never reused on that connection, including re-subscriptions of the same stream
after `lagged` or the progress deadline. `unsub`, `subfail` and `lagged` each
close the named ID; the server sends nothing more on a closed ID. Clients
silently drop late frames for any closed ID, without advancing a cursor or
renewing progress. After `lagged`, re-subscribe with a fresh ID without an
`unsub`. After `subfail`, re-subscribe only where its code calls for it, using a
fresh ID without `unsub`; otherwise stop until the code table's condition changes.
A progress deadline first sends `unsub` to close the stalled ID. A frame for an
ID never opened on the current connection is a protocol error. The server refuses
a `sub` that reuses either a live or closed ID with typed
`subfail { code:"sub_id_reused", ... }`. Preserve stream cursors and resync
markers across replacement while allocating a new subscription identity.

**Subscription refusal and progress.** `subfail { sub_id, code, message }`
closes only the named subscription ID, which receives no further server frames;
clients drop late frames for it. It supplies these typed codes (S7a defines
the wire enum; implementation registers any new API error codes where needed):

| Code | Meaning and client action |
| --- | --- |
| `cursor_ahead` | Stored cursor exceeds the stream head. Clear only that stream's entities, cursor and resync marker, then re-subscribe with a fresh ID without `after` through bootstrap; do not silently clamp. |
| `operator_only` | Subscription requires the operator principal. Show refusal and stop that subscription until the principal changes; other streams stay up. |
| `unknown_stream` | Unknown stream kind or removed subject. Show unavailable and stop that subscription until the user selects a valid stream. |
| `scope` | Token lacks read scope for the stream. Show refusal and stop that subscription until authorization changes. |
| `sub_id_reused` | ID was already used on this connection, live or closed. Refuse the registration; treat this as a client allocation error and use a fresh ID for any new subscription. |

Start the 30 s progress deadline at sub registration, or when a new gap starts
resync of a Current stream. Reset it only on a frame advancing that sub: a newly
applied seq, an advancing gap/cursor or snapshot boundary, or its synced barrier.
Duplicates, repeated identical gaps, unrelated streams and heartbeat frames do
not reset it. Expiry sends unsub, then a new sub with a fresh ID from the applied
cursor and stored resync marker; only that stream returns to Replaying/Resyncing.
Keep the connection and other streams up and show the affected stream's
recovery state.
A replay that keeps advancing can exceed 30 s in total. No timeout marks Current.

**Commands across a drop.** Persist the client's outbox before sending, keeping
principal, unique `cmd_id`, exact verb/args and first-send time. A reconnect may
resend an unresolved command only under the same principal, with the same id
and identical payload, within the advertised window. Do not change ids on retry.
Set `resume_only:false` on the first send and `true` on every resend after
uncertain delivery; exclude this delivery flag from the payload digest. A
resume-only request whose key is absent, expired or lost is refused as uncertain
before dispatch, even if the server restarted with an empty journal. This trades
a possibly unexecuted original for explicit inspection, never a duplicate effect.
There is a dead-socket race: the original `resume_only:false` command may be
buffered on the old connection while the new connection's resume-only lookup
finds no claim and refuses it. The original may later arrive and execute once.
Show **unknown outcome**, not cancelled or definitely unexecuted; inspection may
later reveal its effect. Do not create another command ID to force execution.
On an ack deadline, mark only the command uncertain and keep the connection up.
A late ack on that still-current socket resolves it normally; resend with
`resume_only:true` only after the next reconnect and within the window.
The server persists an idempotency journal at `<data dir>/wire/commands.jsonl`,
keyed by `(principal, cmd_id)` plus a canonical payload digest and first-admission
expiry. Claim a key durably before dispatch; concurrent duplicates join that
claim. Reuse with different payload is refused (`invalid_params`), never executed.
Persist the outcome before sending ack; completed duplicates return the first
ack. Integrate the claim/outcome with durable queue/question mutations so a
restart cannot apply them twice. For nontransactional effects, a recovered pending
claim is uncertain: do not dispatch again. Report `session_unconfirmed` with
CLI `UNCONFIRMED`; expose the existing subject state for inspection. A duplicate
never creates a second execution, including after restart or handoff; an
unresolved original is not falsely presented as a completed first ack.
`session_unconfirmed` keeps its registered `retry:later` class. Retrying is
harmless only with the same principal/id/payload and `resume_only:true`: an
absent/expired claim still refuses and cannot dispatch. The class is not license
to resend as a first send or create a new ID; the outbox/expiry rules still apply.

Automatic resend is allowed for the listed §5 verbs only through this durable
idempotency path, not based on an assumed harmless verb. Permission/question
answers retain stable question/request IDs; apply answer validity, principal
scope and first-answer-wins atomically with the claim. A lost ack cannot answer
twice, and a late answer to an already closed prompt returns the stored result
or the normal stale-request refusal, never targets a newer prompt. While offline
hold answers/commands in the outbox; do not imply that they reached the agent.

After the window, or if elapsed age cannot be established after a client restart,
an unresolved command becomes **unknown outcome** and is not resent automatically,
even with the old key. Do not generate a new key as a workaround. Inspect queue,
question or agent state and require an explicit user decision about any new
command. A changed principal drops the old outbox with a visible explanation.
Persisted token files and journal survive normal restart/handoff; `welcome`
reports the effective `idempotency_window_ms` and a fresh random `server_epoch`
for diagnosis, not as a reason to bypass dedupe. Lost/replaced journal state must
refuse `resume_only` commands rather than assume an empty cache permits execution.
A revoked token stays revoked; no auth state is inherited from a dead socket.

**Single-writer handoff.** As in ADR-0026's ownership-transfer rule, ownership
of `wire/commands.jsonl`, every queue/session log and checkpoint, **ADR-0005's
`event-log.jsonl` (including the `events` stream's seq allocator)**, and all
other wire-stream seq allocators transfers before the new server loads these
files or accepts a connection/command. Hold an exclusive data-directory writer
lock/fence through all writes. Store the owner generation with that lock; every
append, seq allocation and rename-rewrite checks it before committing, including
late callbacks. A process overlap never permits two writer generations.

Handoff has the proposed **10 s monotonic deadline** through the ownership
commit; preparation and every precondition share that deadline, rather than
receiving new windows at each stage.
The old server retains ownership and continues serving reads while temporarily
refusing new command admission/claims with `restart_pending`. It prepares a
reversible quiescence barrier, drains or durably records uncertain in-flight
claims, and flushes producers within that one deadline; none of these steps may
wait indefinitely. Do not close existing wire sockets or release ownership until
all readiness evidence has arrived. At the final fenced transfer, close the old
`/v1/ws` sockets, stop all old writes, and transfer ownership before the new
server loads/replays files and accepts. Check deadline/cancellation before closing
old sockets too: a failed final commit must not needlessly drop those clients.
No old callback may write afterward.
Crash recovery acquires exclusive ownership before loading as the new owner.

If a producer, flush or ownership-readiness step remains stuck at the deadline,
**fail the handoff** with the existing `handoff_failed` outcome. Cancel the
transfer request and discard its late readiness callbacks; keep the old lock,
owner generation and serving endpoint, resume admission and reversible producers,
and leave the new server unable to load as owner or accept. No counter, journal
or checkpoint ownership has been half-transferred. The final ownership commit
must atomically check deadline/cancellation before transferring; a timed-out
request can never later release the old fence. A blocked producer remains the
old owner's responsibility and may fail its own operation; it cannot force
handoff or hold the handoff caller open. Once ownership has committed, report
the committed outcome, never turn an acknowledgement delay into a rollback claim.
This closes the overlap contract gap identified in ADR-0005; it does not claim
that the existing implementation already enforces these rules.

**Revocation on an open socket.** Re-check the current token file/digest and
scopes on **every cmd**, before claiming an idempotency key, returning a cached
ack or dispatching a verb; cached hello authorization is insufficient. Serialize
this authorization/admission boundary with operator revocation, so a command
cannot be newly admitted after revocation. Each admission or management operation
explicitly unlocks its owned file description at scope exit, including completion
or cancellation of lock-helper transfers. An inherited pre-exec descriptor in a
child cannot extend that critical section. On detection of a removed/replaced
token, return an error ack for the triggering cmd with `code:"auth_revoked"`
before closing **all sockets for that token with 4403**; clients enter NeedsToken.
Order that ack before the close on the triggering socket, without waiting for
client acknowledgement or relaxing the write/liveness deadline. If the network
loses the ack, the close still signals revocation. A still-valid token whose
scopes have shrunk instead gets an error ack with `code:"scope"`; do not claim,
return a cached result or dispatch an unauthorized verb, and keep the socket
open for allowed operations. S7a defines these typed error codes and the
implementation registers new API codes where needed. Check hello against the
file too, with 4401 on refusal. This command-time check
is chosen because it does not rely on portable filesystem-watch delivery. Its
limit is explicit: deletion alone does not proactively close a read-only idle
socket; the next cmd or reconnect detects it. Read scopes are checked at sub
registration (`subfail scope`), not continuously on existing subscriptions;
shrinking read scope alone does not terminate those subscriptions.
Revocation prevents subsequent admission, not cancellation of an already admitted command; report its outcome
through the journal and existing state rather than implying it was rolled back.

**Compression and secrets (architect decision, round 2).**
**`permessage-deflate` is NOT negotiated in S7.** Decline the extension at
upgrade for every client; a browser or webview GUI gets no deflate at all, and
all data messages, including hello, have RSV1 unset. Browsers/webviews may
compress every client message after negotiation, including hello's token, with
no per-message application switch. Disabling compression avoids an oracle on
mixed secret and attacker-influenced content through observable lengths;
no-context-takeover alone does not remove that risk (see
[RFC 7692 security considerations](https://www.rfc-editor.org/rfc/rfc7692.html#section-8)).
A later opt-in needs a separately reviewed **native-client-only** rule decided
at upgrade time, with uncompressed hello and secret messages enforced; it is
not an S7 setting or a post-hello capability toggle. Log neither bearer values
nor raw secret payloads in transport diagnostics.

**Boundaries.** Mobile background suspension closes or invalidates the connection;
foreground resumes with cursors and the same bounded outbox rules. Push wakeups
and a relay remain #1402, outside this amendment. Read recovery describes the
client-to-serving-node hop; it does not assert that an unavailable remote node
is current. Preserve origin/remote-health diagnostics and stale remote state;
a successful local ping cannot certify a remote source. Cross-node command
routing remains refused until #1380; mesh VERSION stays **5**. QUIC/WebTransport
and SSE/long-poll are not selected: they add transport/auth/resume paths and do
not remove the need for application cursors, snapshots or command dedupe. Revisit
only with a filed proposal and evidence WebSocket recovery fails a requirement.
S7 has not shipped: `ping`, `pong`, `synced`, `subfail`, `snapshot_to` and
welcome recovery metadata are additions to the initial `WIRE_JSON_VERSION` contract, not a bump
of an implemented/released wire. The framed protocol version is unchanged.

**Implementation acceptance, not tests executed by this docs PR.** S7a defines
the fake-server/fault-script types; S7 and G3 must preserve a failing baseline
for each case below, then pass with the contract implemented. Use supplied clocks,
controlled socket faults and bounded receives, not sleeps or live models.

| Fault or scenario | Required evidence |
| --- | --- |
| Black-holed connect / missing hello / missing welcome | Each peer ends the appropriate 10 s or 5 s stage; no unbounded task or unauthenticated subscription remains. |
| Half-open socket, silent live peer, missing pong | Stale and close within 45 s on the affected side; subscriptions released, outbox held, no success inferred. Active pings cannot hide a replay that never reaches synced. |
| Backoff and foreground storm | Supplied randomness proves jitter bounds, cap, stable-reset rule and one active attempt; coalesced network changes do not create parallel sockets. User disconnect cancels attempt and timer. |
| Mid-turn drop, duplicate delivery, delayed old-socket callback | Resume from applied cursor; rendered persisted transcript contains each event once in order. Discard old-socket frames; complete event repairs abandoned live deltas. |
| Reordered replay/barrier | A fault script delivering synced before its announced snapshot end or out-of-order seq is rejected, never marked Current. Every seq stream, specifically terminal and events, receives exactly one valid barrier. |
| First subscription and empty state | fleet, inbox and queue bootstrap all use the inline gap/snapshot path; empty from=1,to=0,snapshot_to=0 completes without waiting. No client fetch or guessed end marker. |
| Zero-line queue rewrite and restart | Append through H, empty the folded queue, rewrite with header high_water=H, restart, subscribe and process empty gap: cursor becomes H; next item is H+1 and is applied rather than deduped away. Crash before/after rename preserves one complete checkpoint. |
| Disconnect during snapshot / lagged / another rewrite | Stored marker and applied cursor resume remaining lines. Lagged closes the old ID; re-subscribe with a fresh ID without unsub and drop late old-ID frames. A newer checkpoint marks all entities unconfirmed again. Concurrent mutations are present after replay; removed entities disappear exactly at snapshot completion. |
| Lost ack / duplicate cmd / restart at claim, effect or outcome | Same principal/key/payload yields one effect and first stored ack, or explicit uncertain pending outcome. All listed verbs, including fork/reload/interrupt, prove no automatic second dispatch. Different payload never executes. |
| Permission prompt across drop | Prompt survives replay, stored answer retries once under the same request/key, closed prompt cannot receive a second answer or an answer intended for a newer prompt. |
| Expired key / clock uncertainty / changed principal / revoked token | No automatic resend or new-id workaround; unknown outcome is visible, old-principal outbox drops, revoked token enters NeedsToken. |
| Restart, handoff, missing journal or ahead cursor | Token scopes and stream seqs persist; no seq reset or duplicate side effect. Missing recovery state fails closed; ahead cursor is refused. |
| Compression disabled | Offer permessage-deflate from browser, webview and native fixtures: every S7 upgrade declines it, all messages including hello have RSV1 unset, and no secret appears in diagnostics. |
| Single-writer handoff overlap | Hold the old process alive with queued append/rewrite and command callbacks, including event-log.jsonl writes. New loading/acceptance requires exclusive ownership; every write checks the lock's owner generation. Race rewrite with append and duplicate cmd across transfer: no overwritten append, repeated seq or double effect. |
| Stuck handoff producer | Keep a producer alive but unable to quiesce (repeat for a stalled flush/readiness step). At the supplied-clock 10 s deadline, handoff fails with handoff_failed: old ownership/generation/serving remain, admission resumes, new owner loads/accepts nothing. Release the delayed callback afterward: it cannot transfer the fence or mint a new-owner seq. No hang or half-transfer. |
| Open-session revocation | Authenticate two sockets with one token, revoke it, then send a new cmd on one and a duplicate on the other. Neither claims, returns cached result or dispatches; the triggering cmd gets auth_revoked error ack before both sockets close 4403 and enter NeedsToken. Rejected hello closes 4401. An already admitted command is not claimed cancelled. |
| Scope shrink | Reduce a still-valid token's scope after hello; a newly unauthorized cmd and cached-result lookup both get scope error ack without admission/result disclosure. Socket remains open and an allowed operation still succeeds. |
| Slow replay heartbeat priority | Queue replay in both directions on a constrained healthy link; ping/pong bypass waiting data and prevent false Stale from backlog. An actually blocked writer or missing pong still fails within the liveness ceiling. |
| Terminal snapshot recovery | Lose delta history, then receive gap with snapshot_to and a full frame followed by deltas and synced. The old display remains stale until the full frame applies; a delta or early synced cannot clear it. Removed pane refuses unknown_stream. |
| subfail cursor_ahead | A restored head below the saved cursor refuses; client clears only that stream's entities/cursor/marker and bootstraps on a fresh ID without after. Other streams continue. |
| Closed, unknown or reused subscription ID | Unsub/subfail/lagged close the ID. Client silently drops late frames for a closed ID, but treats a never-opened ID as a protocol error. Server refuses a sub reusing a live or closed ID with sub_id_reused. |
| subfail operator_only | Non-operator subscription refuses and stops until principal changes; no reconnect loop or blocked sibling stream. |
| subfail unknown_stream | Unknown kind/removed subject refuses and stops until a valid selection; no fake empty success. |
| subfail scope | Insufficient token scope refuses only that sub until authorization changes; sibling streams remain live. |
| Per-stream progress deadline | Replay exceeds 30 s overall but progresses within each window and succeeds. A stalled sub expires despite duplicate/unrelated/heartbeat traffic: unsub the stalled ID, then re-subscribe with a fresh ID from its applied cursor, retaining resync marker and the connection. |
| Ack timeout and delayed original | Missing ack marks only that cmd uncertain; no same-socket resend or teardown. A late current-socket ack resolves it. After reconnect, deliver resume-only lookup before buffered original: show unknown outcome while the original may later execute once; never manufacture a new ID. Verify retry:later remains harmless with resume_only. |
| Mobile suspend and remote outage | Foreground invalidates old socket and resumes. Local heartbeat does not clear remote stale state; cross-node cmd still refuses under VERSION 5. |

### 6. One schema, generated TypeScript

The Rust types are the schema: the session event enum (§8), the wire
messages of §5 and every `cmd` params and result type. They derive `ts_rs::TS`
behind a `ts` cargo feature, and a test (`wire_ts_types_are_current`)
renders them and compares with the checked-in `web/client/src/gen/*.ts`; a
difference fails the test and therefore CI, and `just gen-ts` rewrites the
files. No hand-written TS type may describe a wire shape.

S8a (#1592) starts with the socket API's `AgentInfo`, `FleetAgentInfo`,
`WorkspaceInfo` and `PaneInfo` records, their transitive dependencies, the
question types and the Claude adapter's `SessionEvent`. The adapter's custom
serializer emits an untagged union; its generated stored form is
`SessionEvent = KnownSessionEvent | JsonValue`. Clients narrow `KnownSessionEvent`
on literal `type` and catalogue discriminators, including nested stream and
control tags. Switching on a nested tag narrows its payload, but TypeScript does
not correlate that narrowing back to the containing envelope. Absent-only subtypes are `subtype?: never`. The event catalogue
supplies these literals to ts-rs; no TypeScript wire declarations are maintained
by hand. Extra JSON fields stay flattened as an index signature. Unknown events
reach clients only through §8's `UnknownEvent` wrapper and use §8's rendering.

S7a (#1604) defines separate `ClientFrame` and `ServerFrame` enums tagged by
`t`, with unknown transport tags rejected as protocol errors and additive fields
ignored. `WireEvent = UnknownEvent | KnownSessionEvent | JsonValue`: the known-only
projection is the S8a catalogue type defined by the adapter’s `events!` macro. Agent `ev` payloads reuse known S8a events; an unknown, incompatible or
client-version-ineligible event uses `{type:"unknown", raw, shape}`. `raw` is
the exact original line as a string (including non-JSON diagnostics); `shape`
is the sender's §8 canonical key, never recomputed with the client version.
This wrapper reconciles the adapter's lossless raw serializer with §8's
frontend rendering contract. Other stream payloads and cmd args/results stay
explicitly opaque JSON until S8b #1677. No new payload interpretation lives in
S7a. The server supplies the wrapper before sending, not the frontend reducer.

The initial `WIRE_JSON_VERSION` is 1. Wire streams are strings using §5's paths;
unknown paths decode so the server can return `subfail unknown_stream`. The
`welcome` principal is the token name and capabilities are operation strings;
a newer client's read-only welcome advertises no command capability. `ev.seq`
is omitted only for `agent/<id>/live`; its timestamp is still present. Ack
success/error fields are exclusive; successful void commands may omit result.
Ack error codes are open strings covering the full ADR-0029 registry and future
codes; transport additions have documented constants rather than a closed enum.
Fixture scripts require initial `given`, separate caller `client` actions,
state `observe` assertions and final bounded `quiet`. `expect.absent` requires
omission, `expect_connect` asserts reconnect timing, and `send.socket:previous`
injects abandoned-generation callbacks without granting them authority.
S7a's scripts and validators are test-only until S7 consumes the contracts, so
there is no endpoint or default-binary behavior change.

Absent metadata and explicit null remain distinct: `Option<Option<T>>` is
exported as `?: T | null`. Typed 64-bit durations, timestamps and counters use
JSON numbers, with exact JavaScript integer precision only below 2^53, documented
on each field. Opaque identities and turn cursors already use strings. The default
build excludes ts-rs; `just check` and CI compile and test the `ts` feature.
`just gen-ts` is an explicit, ignored nextest generator, while the drift test
only reads files, so a normal test run cannot repair stale generated output.
The generator normalises trailing whitespace, and checks the complete TS file
inventory as well as contents. ts-rs reports unsupported custom deserializers;
these affect lenient input reading, while the emitted types describe serialization.
The stored `SessionEvent` and wire `WireEvent` are distinct contracts.
The adapter's `Unknown.raw` retains the exact line; its `shape` is Rust metadata
until the server constructs §5/§8's explicit
`UnknownEvent = {type:"unknown", raw:string, shape:string}` wrapper.
A bare stored unknown is never an agent-stream wire payload. The wire alias
remains `UnknownEvent | KnownSessionEvent | JsonValue`; `JsonValue` supports
non-agent streams and does not remove the agent-stream wrapper requirement. The existing serde_json
serializer preserves JSON token spelling and interior whitespace, trims surrounding whitespace, and
encodes non-JSON diagnostics as a JSON string.

### 7. The Bun/TypeScript reference client

- **Layout.** `web/client/` is a Bun workspace: `src/gen/` (generated, §6),
  `src/wire.ts` (connect, hello, subscribe from cursor, reconnect with the
  last applied seq, heartbeat/stale detection, capped jittered reconnect and
  the bounded durable outbox rules of §5a), `src/views/` (agent transcript,
  queue, questions), `src/unknown.ts` (§8). The views use Solid (owner
  decision 8): its fine-grained reactivity updates one text node or one
  queue row per streamed event without re-rendering a transcript, which is
  the load of P1 and P9. Solid's JSX transform runs through
  `vite-plugin-solid` (`babel-preset-solid`), pinned in `bun.lock` like
  every other dependency.
- **Build.** Vite with `vite-plugin-solid` produces static assets that the
  `web` feature embeds with `rust-embed`, as it embeds the terminal bridge's
  assets today. Amended 2026-10-16 (owner decision, ADR-0038 §6, #1606): Vite
  replaced `bun build` because it gives hot reload in development and is the
  standard Tauri v2 front-end setup. Bun stays the package manager and test
  runner. CI runs `bun install --frozen-lockfile`, `bun test` and the Vite
  build; the nix package builds the assets in a fixed-output step pinned by
  `bun.lock`.
- **Release.** The assets ship inside the `flk` binary of a build with the
  `web` feature; there is no separate artefact to version.

### 8. Unknown events: pass through, show, log once, report

- **Lossless parse.** `SessionEvent` is a Rust enum over the known
  `type`/`subtype` pairs; every known variant keeps unknown fields in a
  flattened `extra` map, and an unparseable or unknown line becomes
  `SessionEvent::Unknown { raw, shape }`, with `raw` kept byte-for-byte. The
  parser never errors past the JSON layer; a line that is not JSON is
  `Unknown` too, with `raw` as a string.
- **S7 wire wrapper.** On S7's `/v1/ws`, unknown, incompatible or
  client-version-ineligible agent events are sent only as
  `UnknownEvent = {type:"unknown", raw:string, shape:string}` inside `ev.ev`.
  `raw` is the exact original line, including non-JSON diagnostics; `shape` is
  the sender's canonical shape key, never recomputed by the client. The server
  constructs this wrapper before sending. It is distinct from the internal
  `SessionEvent::Unknown { raw, shape }` and its lossless raw serializer. The
  legacy API keeps that existing raw JSON behavior; it does not emit this S7
  wrapper. This is the explicit client envelope referenced by §6.
- **Shape key.** `shape = harness + harness_version + type + subtype +
  sorted top-level keys`. A known event whose `extra` is non-empty is also a
  new shape.
- **S1 note (shape identity).** The canonical key also includes `event.type`,
  `request.subtype` and `response.subtype`, alongside harness, observed harness
  version, top-level `type`/`subtype` and sorted top-level keys. These nested
  discriminators distinguish SSE and control envelopes that otherwise share
  the same top-level shape. Missing discriminators are null. This refines the
  key's representation without changing the owner decision.
- **Replay provenance.** The §5 session-store `known`/`shape` metadata fixes
  the sender's original classification and version-scoped key across restarts
  and parser upgrades. A now-known payload originally classified unknown still
  uses its original unknown wrapper. Missing legacy provenance emits the
  `provenance_missing` history gap and replay continues, without recomputation.
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
| P9 | Queued messages shown, editable, cancellable | flk queue (§3), `command_lifecycle` | S5, S5c |
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
| P23 | Harness, model and effort picker, for a new agent and for a switch at idle | per adapter, §10a | S17 |
| P24 | Message view whose whole turns collapse and expand, not only tool output | turn boundaries from `command_lifecycle` and `result` | S18 |
| P25 | Permission preset, picked at start and switched at runtime, beside the P23 picker (#1321) | `system/init.permissionMode`; `set_permission_mode` in place, a reload for `bypassPermissions` (A.15, §10b) | S19 |

P16 and P17 have no source in the stream today and are not promised;
#1308 tracks finding one or deciding against them.

### 10a. The picker (P23) and collapsible turns (P24)

- **Picker.** One picker, in the TUI and the web client, chooses harness,
  model and effort. For a new agent it fills `agent.start`'s `harness`,
  `model` and `effort`. For a running structured agent it is a switch at
  idle: `agent.reload { model?, effort? }` goes through §1's hot reload
  (close stdin at idle, `--resume` with the new `--model` / `--effort`), and
  the init diff records the change. Where an adapter can switch in place,
  it may skip the respawn, but only after a probe confirms the prefix
  behaviour (a model change always misses the cache: caches are per model).
  The model and effort lists come from each adapter, merged with flk's
  model registry (ADR-0021) for aliases and tiers:
  - **Claude.** The CLI has no list verb. The binary carries the SDK control
    messages `supportedModels`, `supportedEffortLevels` and `set_model`
    (A.4 method: `strings`; not probed live). S17 probes them first; until
    they are confirmed the list is flk's registry (ADR-0021) validated by
    `system/init.model` after the switch, the effort levels are the
    `--effort` choices that `claude --help` lists (`low`, `medium`, `high`,
    `xhigh`, `max`; A.4, help text only, no run per level), and a switch is
    always a reload.
  - **Codex.** `codex app-server`'s `model/list` returns `Model{id, model,
    displayName, isDefault, hidden, supportedReasoningEfforts,
    defaultReasoningEffort, …}` (read from the generated schema, #1309);
    `turn/start` takes `effort` per turn, so effort changes need no reload.
  - **opencode.** `opencode serve` lists providers and models at
    `GET /config/providers` and `GET /api/model` (from its OpenAPI document),
    and `/api/session/{sessionID}/model` sets a session's model.
- **Collapsible turns.** The message view groups the stream by turn: one
  turn is the inputs a `result` closes (§3's items plus everything up to and
  including that `result`; steered items fold into the turn they joined,
  E4). Default rule: every turn except the last is collapsed to one line
  (the first input's first line, the tool-call count, the outcome of §1
  and the turn's tokens), the last turn and any turn with an open question
  or a failed outcome are expanded, and a turn the user expanded stays
  expanded for that client. Tool output inside an expanded turn keeps its
  own collapse (P2). The rule is client state, not stream data: the stream
  only carries the boundaries.

### 10b. Permission presets (P25, #1321)

- **A preset** is a named, ranked bundle in flk's config
  (`[permissions.presets.<name>]`): the harness permission mode
  (`--permission-mode`), the tool allow and deny rules, and the
  `hook.decide` policy for `permission.request` (ADR-0034 §1). The rank
  orders presets from least to most permissive, so "escalation" is
  defined. The rules live in flk's `hook.decide` policy, not in
  `--allowedTools`, so changing them needs nothing from the harness: every
  prompting tool call already reaches flk over stdio (§2b).
- **Pick at start.** `agent.start { permission_preset }`; the picker shows
  it beside harness, model and effort (P23), in the TUI and the web client.
- **Switch at runtime.** `agent.set_permissions { preset }`. The rule part
  takes effect at the next `can_use_tool`. The mode part goes through the
  control request `set_permission_mode { mode }` over stdio (S0, A.15): in
  place, at once, with no respawn, and also during a turn, where it
  applies from that turn's next tool call. S0 switched at idle to
  `acceptEdits`, `default` and `plan`, and during a turn to `acceptEdits`
  only; switches to `auto` and `dontAsk`, and any switch during a turn to
  a mode other than `acceptEdits`, were not probed, so S19 probes them
  before it relies on them. The valid modes are `default`,
  `acceptEdits`, `plan`, `auto`, `dontAsk` and `bypassPermissions`; any
  other is refused with `error_code: invalid_mode`, and a refused request
  leaves the mode as it was. `bypassPermissions` is the exception: it is
  refused (`bypass_not_launched`) unless the session was launched with
  `--dangerously-skip-permissions`, so a switch to a preset whose mode is
  `bypassPermissions` is a switch at idle through §1's reload with that
  launch flag (no compaction, so the prefix stays warm, E14). Every other
  switch is in place. A switch away from `bypassPermissions` is expected to
  be in place too, but it was not probed (S0 never launched with the
  flag), so S19 probes it as well. It is
  recorded as `agent.permissions_changed { from, to, by, mechanism:
  in_place | reload }`; `system/init.permissionMode` shows the new mode
  from the next turn on (it is emitted at turn start, so a switch during a
  turn shows only in the next one). A switch to `plan` makes the harness
  write plan files into the session's config dir (`$CLAUDE_CONFIG_DIR/plans/`,
  §1's private dir per profile, A.15), so they stay with the profile after
  the session ends.
- **Escalation is operator-only.** A switch to a higher-ranked preset needs
  the local operator or an `operate`-scoped token (§1a, §5); anything else
  is refused with `operator_only` (exit 13). A switch down needs `send`
  scope. A `flk-agent` caller can switch its own agent down, never up.

- **Per-request session grants (S6, #1429).** `agent.decide` accepts
  `allow_once`, `allow_always` and `deny` only from an authenticated,
  `operate`-scoped token principal. A local UID alone, pane input, an agent
  message or a structured harness's MCP lease never authorizes a decision
  (`not_authorized`, retry never). Identity comes from the authenticated
  dispatcher context, never command arguments. Every decision is recorded
  on `agent/<id>` in ordered open/closed permission audit records, including
  automatic matches, with the originating token subject. Automatic closes also
  reference the originating grant request ID. A raw request already reserved by
  an exact grant carries that trusted reference into replay and counts as
  stdout progress without ever opening S4 needs-human attention.
- **`allow_always` lasts for one live harness runtime.** Its grant is held
  only in memory for that structured agent. Exit, restart and resume into a
  new runtime discard it, even when resuming the same persisted session.
  Grants are never shared between agents, persisted in flk policy, or
  written to a settings file. Persistent presets remain the separately
  operator-edited policy described above (#1799).
- **Match exactly; never forward harness rules.** `allow_always` always
  installs an in-memory grant for only the exact tool name plus canonical
  JSON input (recursively sorted object keys; array order and scalar values
  preserved). Matching performs no Unicode normalization and treats `1` and
  `1.0` as different. A different tool or input prompts again.
  Harness permission suggestions are never interpreted or forwarded, even
  when they appear limited to the requested tool: determining their scope
  would require interpreting the harness rule grammar. The response allows
  the current call with its original input and no `updatedPermissions`.
  When nonempty suggestions are present, the closed audit records
  `ignored_suggestions` with reason `not_forwarded` and the request ID linking
  to the open audit record containing the full original suggestions.
  Absent, null and empty suggestions produce the same exact grant.
  Broader rules belong to the separately operator-edited policy in #1799.
  The `MAX_GRANTS` constant in `src/structured/permissions.rs` bounds distinct
  installed and reserved exact grant keys per runtime; exceeding that bound
  returns `permission_grant_limit` (retry never, policy refusal) without
  closing the request or sending a response. The operator may still allow
  once or deny that request. Token authorization still applies to manual
  decisions after a grant exists.
- **Delivery and audit ordering.** A permission closes for attention only
  after its decision audit is durable, and user input remains gated until
  the control response is fully written. Admission success reserves a
  decision and does not promise eventual pipe delivery. Audit or writer
  failure stops delivery; runtime shutdown writes a reasoned terminal close
  for each outstanding request before clearing the book, without inventing
  an operator decision.
  Harness cancellation closes its named request; top-level result and host
  interruption also produce reasoned closes. A cancelled reservation cannot
  deliver a stale answer. Disk errors are surfaced; failed persistence cannot
  promise a durable close. Unknown and already accepted or closed request IDs are distinct `permission_request_unknown` and
  `permission_request_closed` refusals, both retry never.

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
   keeps `allow_once | allow_always | deny{reason}` and maps down.
   `allow_always` obeys §10b's live-runtime lifetime, token-only authority
   and audit contract. flk always uses exact in-memory grants and never
   forwards harness permission suggestions or sends `updatedPermissions`.
   Future adapters must preserve that scope rather than map an always
   decision to a persistent harness rule.
4. **Fork granularity.** Claude forks at a message uuid; opencode at a
   message id; Codex at a turn (`thread/fork{lastTurnId}`). `agent.fork`
   refuses a mid-turn point on Codex with `unsupported_for_agent`.

Only the server modes qualify (E18): Codex through `codex app-server`
(stdio JSON-RPC), opencode through `opencode serve` (HTTP + SSE). Their
`exec --json` / `run --format json` modes are not used. Codex has `turn/steer`
and opencode a `todo.updated` event that Claude lacks; they map to `steer`
delivery and P15. Adapters for Codex and opencode are slices S14 and S15 and
not part of the first delivery.

### 12. Reserved extension points (#1310)

The spike #1310 (structured agent output as validated HTML fragments, with
anchored comments) follows this ADR. Three points are reserved here so it
can land without reshaping §5, §6 or §8; their design belongs to #1310, not
to this record:

1. **A content-block kind beyond markdown.** The session event model keeps an
   open `content.kind` on rendered text blocks (`markdown` today); a further
   kind (markdown plus allow-listed components, or sanitised HTML) is added
   as a new variant, and a client that does not know it falls back to the
   unknown-event rendering of §8.
2. **Stable per-block ids as comment anchors.** Every rendered block carries
   a stable id (`message uuid` + block index), so a comment can anchor to
   it; a comment is queued back to the agent as an input item through §3
   (`kind` to be named by #1310) and passes the same gate.
3. **The TUI's text fallback.** Every block kind has a plain-text rendering
   that the TUI shows; a kind without one is not admitted.

## Failure modes and known limits

- **Hidden flags move.** `--resume-session-at`, `--permission-prompt-tool
  stdio`, `command_lifecycle` and the control subtypes
  `cancel_async_message` and `set_permission_mode` are undocumented in
  `--help`. The
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
- **`--restricted` and `CLAUDE_CODE_RESTRICTED=1` are the only switches
  found that close the harness's messaging socket (E16, A.11).** The
  launch profile uses the flag. Their effect on the socket is known from
  one harness version (2.1.296) and the changelog line carried in it. If
  a later version binds the socket despite the flag, `system/init` reports a
  `messaging_socket_path` again, and S3 refuses to run that session
  rather than give structured agents a door into each other past the
  queue (§1).
- **`interrupt` delivery stops the running turn (A.14).** A tool that is
  mid-run is rejected, and what it did before that is not undone. A
  `steer` write ("send now" without `interrupt`) does not stop it, but
  waits for the next model step, so a long tool delays it, and it can
  change what the running turn answers (§3).
- **A question in the same message after a prompting call waits for the
  prompt (A.13, §2b).** Not holding a question cannot help it there; the
  compiled context tells agents to ask before, or apart from, a call that
  may prompt.
- **The login token is in the session's environment (owner decision 1).**
  flk never logs it, but whether the harness passes
  `CLAUDE_CODE_OAUTH_TOKEN` on to the agent's own tool processes is
  untested; A.11 saw it pass its `CLAUDE_CODE_MESSAGING_TOKEN` on to them.
- **System One is advisory.** When it is not configured, times out or its
  breaker is open, questions are not held (§2a), batches are not joined and
  no item is marked relevant (§3): the defaults, never a stall.
- **The structured view needs a client.** A structured agent has no terminal
  to attach to; `terminal/<pane>` is for `pty` agents only.

## Owner decisions (2026-10-13)

The owner answered the open questions of the proposed record on 2026-10-13.
Each item names the question, the answer, and where the body follows it.
"As recommended" means the recommendation of the proposed record, which the
body already carried. An "S0 note" line (2026-10-14, #1324) records what
the S0 probes found that bears on a decision. The notes record facts; they
change no decision. An "Owner decision, 2026-10-14" line records a later
decision by the owner.

1. **Isolation and login under `flk-agent` (#1177, ADR-0035 §6.3).** As
   recommended: `claude /login` once per profile as `flk-agent` into a
   private config dir where a browser is available, `claude setup-token`
   on headless hosts, never a copy of the operator's credentials (E6), with
   the isolation flags of §1 on top (E5). Still unprobed: whether
   `/login` and its keychain item work for a macOS user with no GUI login
   session; probe slice S0 checks it, and if they do not, `setup-token` is
   the macOS path too. (§1)
   *S0 note:* as a macOS user with no GUI session and no keychain,
   `claude auth login` failed to save its login and wrote no plaintext
   file (2.1.287); a `setup-token` token in `CLAUDE_CODE_OAUTH_TOKEN`
   worked for the same user (A.16).
   *Owner decision, 2026-10-14 (#1404):* agent users log in with
   `setup-token` only. g-fleet renders it from sops as a 0600 file owned by
   `flk-agent` (g-fleet#476); flk passes it into the session env and never
   logs it. `/login` is kept for GUI operators only. Rejected alternative:
   unlocking a keychain at boot. (§1, S3)
2. **Build on `--resume-session-at`.** As recommended: yes, behind the §9
   capability probe, with the rehydrate path as the measured fallback
   (E11). (§9)
3. **Question and permission channel.** Both, not stdio alone: stdio is
   primary, and an MCP tool (`flock_question_ask`) is the richer path, used
   for asks that need more than a choice and whenever stdio would block. The
   owner's concern that stdio can itself block is answered case by case in
   §2b: an open permission request (S0 measures whether it holds a
   question behind it; if so the MCP tool is the way to ask then), flk not
   reading (the pump never waits on anything), and the hidden flag
   disappearing (the launch profile falls back to an MCP permission tool).
   (§2b)
   *S0 note:* in A.13 an open permission held every later call of the
   same message, a pre-allowed MCP call included, while a call ordered
   before the prompting one ran first. A concurrency-safe `Read` behind
   the open permission was held too, and with no permission open a slow
   non-concurrency-safe call still held a later one. (§2b case 1, A.13)
   - 3a. **Non-blocking questions (#1301).** As recommended (questions are
     deferred, permission requests block), plus three additions now in
     §2a: an honest-blocker question, where the agent has nothing else to
     do, holds, and a System-One check decides hold or continue from the
     last turn or turns, the question, and the open tasks and work packages
     with their dependency graph; a question that is not held ends its tool
     call with the result `<question-id> submitted for user feedback - the
     response will be relayed to you`; and each question has a stable id,
     a hash of the question, rendered in fixed bytes so the prompt cache
     keeps hitting across turns and forks. (§2a)
     *S0 note:* both carriers kept the model working, and `allow` is
     framed as "The user answered", so the not-held result travels as a
     `deny` (A.12). A question asked in the same message after a call that
     prompts was not asked until that prompt was answered (A.13, §2b). The
     id is stable per agent; a fork's prefix carries its parent's ids
     unchanged (§2a, #1324).
4. **Delivery and the queue.** Not as recommended (human `steer` by
   default). The owner wants a full queue UX: queued messages wait for
   idle and stay editable; they can be deleted and reordered; there are
   any number of stashes with pop and clear; at idle the queued human
   messages go as one bulk, except the one being edited; "send now" forces
   one; a System-One check flags queued messages relevant to the running
   work and suggests or forces them. First, probe slice S0 finds how Claude
   Code handles queued messages in `-p` and whether it has a take-back or
   edit surface; if it does, flk wraps it, otherwise flk's queue is the
   surface. The default delivery is now `at_idle` for every producer,
   humans included; `steer` is what "send now" sets. (§3)
   *S0 note:* the harness has a take-back (`cancel_async_message`, for
   inputs still `queued`), which `queue.take_back` wraps, and no edit, so
   flk's queue is the only edit surface. The harness's own send-now
   (`interrupt{send_now}`) is an interrupt: it rejects the tool in flight,
   whose side effects so far persist, and starts the item as a new turn.
   flk uses it for `send_now { interrupt: true }`. A `steer` write, which
   "send now" sets, folds into the running turn at the next model step,
   waits out a running tool, and can change what that turn answers.
   (§3, A.14)
5. **The messaging socket.** No separate concept. flk owns the session's
   input, so agent messages are queued exactly like human ones. The agent
   and human messages are not submitted together: FIFO across both, one
   batch per turn at idle, the next after that turn's `result`, unless
   System One judges them related, and then they go together. E16 is kept
   as a fact about the harness; flk never uses that socket, and S0 records
   what it accepts. The socket is no longer a blocker for shipping. (§1,
   §3, E16, "Failure modes")
   *S0 note:* the socket takes unauthenticated turns from any process of
   the session's uid, and `--restricted` closes it; the launch profile
   passes it, so the known limit is gone. (§1, E16, A.11)
6. **Where the wire endpoint lives.** As recommended: `/v1/ws` in the
   server process, with `web` on in release builds as part of S7. The
   owner clarified the architecture, now stated in §5: with `-p` there is
   no terminal-render shipping; JSONL messages and state go to clients
   live, with a pull for history; the client is a renderer; the mesh
   carries only state and messages between nodes, with no hopping between
   servers except to open a real terminal. Open item: evaluate the Grafana
   Labs stack (Loki, Tempo, Grafana Live) against our own for the stream
   and history, filed as #1323, reporting before S7. (§5) Resolved
   2026-10-16 by the owner (ADR-0038 §11 item 5): log, metric and trace
   output goes through vig-os/vigil, the stream and history stay §4/§5's
   own, and #1323 no longer gates S7 (integration: #1614).
7. **TS generation.** As recommended: `ts-rs`. (§6)
8. **Web client framework.** Not as recommended (none): Solid, for its
   fine-grained reactivity under streaming updates. (§7)
9. **Client tokens and scopes.** As recommended and as written in §5, with
   no loopback trust (§1a). An `operate`-scoped token, minted over the unix
   socket by the local operator, is operator identity from any address once
   `--allow-non-loopback` is on. (§5)
10. **Storage first cut.** As recommended: per-agent JSONL plus the blob
    store; zstd sealing, SQLite and DuckDB later, when a measured view
    needs them. (§4)
11. **Thinking retention.** As recommended: thinking blocks are kept
    lossless (signed, needed for a byte-exact resume); retention is decided
    with storage sealing (S16). (§4)
12. **The TUI's structured view.** As recommended: the server-side renderer
    subscribes to the same typed stream in-process; a client-side TUI is
    not part of this record. (S10)
13. **Coexistence with `[slots]` and the switch paths.** As recommended: no
    change. (§5)
14. **Rehydrated transcripts.** As recommended: accepted as the
    ingest/rehydrate operation the issue allows, copy only, never editing a
    line, the copy deleted when the fork ends. (§4, §9)
15. **Permission presets (#1321), added by the owner in the same session.**
    A preset is picked at start and switched at runtime, with a picker
    beside P23 and escalation for the operator only; probe slice S0 tries
    `set_permission_mode` over stdio. (§10b, P25)
    *S0 note:* it works in place at idle for `acceptEdits`, `default` and
    `plan`, and mid-turn for `acceptEdits`; `auto` and `dontAsk` were not
    switched to. `bypassPermissions` is refused unless the session was
    launched with `--dangerously-skip-permissions`, so it needs a reload
    with that flag. (§10b, A.15)

## Slices

Tracked in #1307. Ordered; each is at most about 400 production lines (tests and doc comments
excluded; TypeScript counts the same as Rust) and lands behind `runtime: structured` or the `web` feature.

| # | Slice | Depends on | Est. lines |
|---|---|---|---|
| S0 | Complete (2026-10-14): the probes landed (#1356); the A.13 controls and token-path re-runs, the concurrency-safe call behind an open permission, and A.16 are recorded (#1404). Probes, recorded in the appendix: the messaging socket of E16 (what it accepts, its owner and mode, a switch to disable it); the placeholder carrier, `deny` versus `allow` with placeholder answers, and that the model keeps working after it (§2a); parallel control requests, an `AskUserQuestion` while a permission is open (§2b); the harness's own queue in `-p`: take-back, edit and send-now behind `interrupt_cancel_queued_v1` / `interrupt_send_now_v1` (§3); `set_permission_mode` over stdio (§10b); `claude /login` and its keychain item for a macOS user without a GUI login (owner decision 1) | — | 0 (probe) |
| S1 | `SessionEvent` types for Claude stream-json: known variants with `extra`, `Unknown{raw, shape}`, shape key; redacted probe fixtures as tests | — | 350 |
| S2 | `session.unknown_shape` persisted kind (skip test per ADR-0028 §3a), once-per-shape registry logging through `tracing` only (#1300), `flk report --unknown-events` | S1 | 250 |
| S3 | Structured runtime: spawn as `flk-agent` with the §1 launch profile (the login from the 0600 `setup-token` file g-fleet renders (g-fleet#476), passed as `CLAUDE_CODE_OAUTH_TOKEN` in the session env and never logged, owner decision 1; `--restricted`; the restricted-versus-unrestricted `system/init` diff; refuse a session that reports `messaging_socket_path`), session lease (`session_busy`), lease-based caller identity and the uid check of §1a, stdout pump into the per-agent stream, blob store, truthful outcome | S0, S1, O19a #1208 | 400 |
| S4 | Pure per-agent state reducer: `StateSource::Session`, turn / attention / limit from events, stall observation and in-process phase/needs-human facts; registry seam for #1439 and S7-min #1746. Pane-free EventHub events deferred to #1804 | S3 | 300 |
| S5 | Input queue: `queue/<agent>` stream, producers, gate, `command_lifecycle` acks, independent `delivery` (`at_idle` / `steer` / `interrupt`) and `priority` ordering, one batch per turn with the FIFO rules 1 to 4 and batch messages, `flk agent send` and msg delivery routed to it | S3 | 400 |
| S5c | Queue UX: `queue.update` / `edit` leases / `move` / `delete`, named stashes with `stash` / `pop` / `clear` / `stashes`, `send_now` (`steer`, or `interrupt` through the harness's `interrupt{send_now}`), `take_back` through `cancel_async_message` (§3, A.14, owner decision 4) | S0, S5 | 300 |
| S5d | System One for the queue: relatedness joins at idle and the relevance check during a turn, `[queue] relevance` (whose `force` sends through S5c's `send_now`), recorded for rating (§3, owner decisions 4 and 5) | S5, S5c | 200 |
| S5b | Turn-end work through the queue: armed self-compact and its continuation on `result`, recap lift, wake of parked waits (§3a, #697) | S5 | 200 |
| S6 | Control requests: `can_use_tool` → `hook.decide` and §8a questions, `AskUserQuestion` answers, `ExitPlanMode` | S4, S5 | 350 |
| S7a | Wire message Rust types with ts derives: hello/welcome recovery metadata, sub/unsub/cmd/ev/ack, subfail, ping/pong, synced on every seq stream, gap with snapshot_to, lagged (§5/§5a); scripted fake-server fixture format, types only with no endpoint. Before S3, as ADR-0038 §10 | #1546 settled | 200 |
| S7 | Wire endpoint implementing S7a, §5a deadlines, heartbeat, durable command journal, resume/checkpoint and single-writer rules, `/v1/blob`, client tokens, `WIRE_JSON_VERSION` gate, `web` on in release builds; refuse remote-agent cmd until #1380 | S3 (S3a #1518/#1439, S3b #1426), S7a | 350 |
| S8 | `ts` feature, `ts-rs` derives, generated `web/client/src/gen`, drift test, `just gen-ts` | S7 | 200 |
| S9a | Bun reference client in Solid: connect, follow an agent from a cursor, render P1–P4, unknown JSON dump, lazy blobs | S8 | 400 (TS) |
| S9b | Bun reference client: P12–P15, P19, P20, queue view with edit, reorder, stashes, send now and relevance marks | S9a, S5c | 400 (TS) |
| S10 | TUI structured view: server-side renderer from the in-process stream; composer with queue display (P9–P11) | S3, S4, S5 | 400 |
| S11 | Hot reload: idle → close → `--resume`, init diff event and notice, staged fleet reload | S5 | 300 |
| S12 | Forks: `at` / `shape`, cut rules, `resume_at` / `rehydrate` / `slice` paths, capability probe, cost guard, warm-then-fan-out, lineage fields | S3 | 400 |
| S13 | Return channel: `finding` items from forks into the parent's queue | S5, S12 | 150 |
| S6b | Non-blocking questions: question records with stable hashed ids and fixed-template rendering, the placeholder result on the `deny` carrier (A.12) with the §2 deny carve-out, `question.answer` / `question.fork` / `question.resolve`, re-ask from the record, exactly one reply (§2a, #1301) | S0, S6, S12 | 350 |
| S6c | The MCP question path: `flock_question_ask`'s new fields (`blocking`, `context`, `refs`, `waits_on`, `recipient`, `urgency`), pre-allowed in the launch profile, the placeholder as its result, the fallback to an MCP permission tool when the capability probe finds `stdio` gone (§2b, owner decision 3) | S6b | 250 |
| S6d | The hold-or-continue System One check: state from the last turns, the question and the open task and work-package graph, hold on `strong` only, recorded for rating (§2a, owner decision 3a) | S6b | 200 |
| S14 | Codex `app-server` adapter onto `SessionEvent` | S4 | 400 |
| S15 | opencode `serve` adapter | S4 | 400 |
| S16 | Storage sealing (zstd segments) and thinking retention | S3 | 250 |
| S17 | Picker (P23): probe Claude's `supportedModels` / `set_model`, per-adapter model and effort lists merged with ADR-0021's registry, `agent.start` fields, `agent.reload{model, effort}` | S11 (S14, S15 for Codex and opencode lists) | 300 |
| S18 | Collapsible turns (P24): turn grouping from `command_lifecycle` and `result`, the default collapse rule, one-line summaries, in the web client and the TUI view | S9a, S10 | 250 |
| S19 | Permission presets (P25, #1321): ranked presets in config, `agent.start{permission_preset}`, `agent.set_permissions` in place through `set_permission_mode`, by reload for `bypassPermissions` (A.15), operator-only escalation, picker beside P23 | S0, S6, S11, S17 | 300 |

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

`claude --help` (2.1.296) lists `--effort <level>  Effort level for the
current session (low, medium, high, xhigh, max)`. That is the only evidence
for the effort levels in §10a: no run per level was made.

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
| 4 | same point 65 min after the 6-min read | 2 | 0 | 19,025 | 4 | 0.0 | 1,723 | 38,052 |
| 5a | point of test 2 + an MCP server added | 2 | 18,996 | 336 | 4 | 98.3 | 1,606 | 2,574 |
| 5b | point of test 2 + `--tools Bash` (was `Bash,Read`) | 2 | 19,021 | 0 | 4 | 100.0 | 1,172 | 1,904 |
| 5c | 41k session forked with its MCP server removed | 2 | 41,391 | 165 | 4 | 99.6 | 1,619 | 4,471 |
| 5d | control for 5c, server kept | 2 | 41,391 | 41 | 4 | 99.9 | 1,877 | 4,223 |
| 5e | 5b, asked to use Read | 4 | 37,966 | 353 | 230 | 99.1 | 1,752 | 4,507 |
| 5g | point of test 2 + MCP added, asked to call it | 4 | 38,283 | 559 | 80 | 98.6 | 1,163 | 4,950 |
| 6 | base H, end of turn 1, 6 min later across local midnight (A.7.2) | 2 | 6,203 | 118 | 4 | 98.1 | 1,159 | 858 |
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

Base G (a fresh session whose first turn matched F's byte for byte) was
created at 18:31:31 and forked at the end of its turn 1 three times with
`--resume G --resume-session-at <uuid> --fork-session`: at 18:31:39, at
18:37:40 and at 19:42:43, 65 minutes after the second fork, the last
request that read that prefix. The rows are in the A.7 table. The third
fork read nothing and wrote the whole prefix again (19,025 tokens, all as
`ephemeral_1h_input_tokens`): the 1-hour TTL holds, a read inside the hour
renews it (the second fork read 100% six minutes after the first wrote),
and an hour without a read is a full miss and a full 1-hour write, 38,052
input-equivalent tokens against 1,905 for the warm read. This is the rule
§9's cost guard applies. Recorded on #1305 on 2026-10-13.

#### A.7.2 Test 6, day boundary

An earlier attempt with `CLAUDE_CODE_OVERRIDE_DATE=2026-10-14` on a fork at
test 2's point read 99.4% (18,913 / 116) but did not change the date: no
date-change attachment appeared and the model answered 2026-10-13. The
override did not take effect on that path, so that row proves nothing about
a day boundary.

The real run straddled local midnight (CEST), Claude Code 2.1.296, Haiku,
profile iso, same driver and flags as A.7.1. Base H (`86b52283`) ran three
turns at 23:56:00–23:56:13 on 2026-10-13: read `big.txt` and give its last
line; `ls` then Read `a.txt`, sequentially; a one-word reply. Its transcript
holds `attachment{type: date, date: 2026-10-13}` in turn 1. H was forked at
the end of its turn 1 (`c4697d6a`) twice with `--resume H
--resume-session-at c4697d6a --fork-session`:

| When | Prompt | in | read | write | out | read % | TTFT ms | input-eq |
|---|---|---|---|---|---|---|---|---|
| 00:02:03, 2026-10-14 | "Reply with exactly: ok" | 2 | 6,203 | 118 | 4 | 98.1 | 1,159 | 858 |
| 00:03:08, 2026-10-14 | "What is today's date? Reply with only YYYY-MM-DD." | 2 | 6,203 | 130 | 481 | 97.9 | 3,170 | 882 |

All writes were `ephemeral_1h_input_tokens`. In both forks Claude Code
appended `attachment{type: date, date: 2026-10-14, changed: true}`, rendered
as a system reminder ("The date has changed. Today's date is now
2026-10-14. …"), after the new user prompt. The original date attachment
in turn 1 was left as it was. The prefix up to the fork point therefore
stays byte-identical across the day boundary and is read from cache. The
writes are only the new prompt plus the date-change reminder. This matches
E13's expectation: a fork across a day boundary keeps its cache. It does
not contradict §9 or A.7.

Caveats:

- H is smaller than F and G (about 6.3k tokens at the fork point, not
  19k): in turn 1 the model read `big.txt` with `tail -n 1` through Bash,
  not with Read. The 6,203 tokens read is the shared system and tool block
  (5,195, see the A.7 notes) plus H's whole turn 1. The prefix past the
  shared block was a hit as well, not just the system prompt.
- The gap from base to fork was 6 minutes, inside the 1-hour TTL (A.7.1).
  This test isolates the date change. It is not a TTL test.
- Model behaviour, not cache: in the second fork Haiku answered
  `2026-10-13` even though the date-change reminder was in its input, and
  added that "nothing in this conversation supports" the change. The
  reminder reaches the model, but Haiku did not act on it here. flk should
  not rely on the model noticing a date change on its own.

Recorded on #1305 on 2026-10-14.

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

## Appendix A, continued — S0 probes (2026-10-14, #1307)

Measured on 2026-10-14 against Claude Code 2.1.296 on a fleet host (macOS,
Apple Silicon), model alias `haiku`. Drivers are committed under
`docs/adr/0036-probes/` (`s0lib.py` is the shared stream-json driver; every
probe below names its script; `README.md` there says how to re-run it).
The raw captures are not committed, as in A (they hold account rate-limit
state), so a figure below can be checked only by re-running its script.

Every run removed every `FLOCK_*` and `CLAUDE_CODE_*` variable plus
`CLAUDECODE`, `CLAUDE_PID` and `CLAUDE_EFFORT` from the child environment,
ran in a fresh temporary working directory holding `a.txt` and `b.txt`
(deleted afterwards), and passed `-p --input-format stream-json
--output-format stream-json --verbose --replay-user-messages
--include-hook-events --setting-sources local --strict-mcp-config
--no-session-persistence --model haiku` plus the flags each section names.
No flk server, pane or sccache server was started or reached. None of the
original runs passed `--permission-mode`. A.13's token-path re-runs include
controls in `auto` (no flag), followed by controls and permission shapes
with `--permission-mode default`. The drivers now pass
`--permission-mode default` (#1404): under the token path, with an empty
config dir, every session started in `auto` (A.13, A.17).

**Isolation caveat.** The runs recorded here set `CLAUDE_CONFIG_DIR` to the
operator's live work profile, for its keychain login (E6). That was not
isolated enough: each session registered `sessions/<pid>.json`, an inbox
key and a socket under it while it ran, left empty `projects/<cwd>/memory/`
and `session-env/<session id>/` directories, and one A.15 run wrote a plan
file into `plans/`. All of it was removed afterwards by the session ids and
working directories in the captures (PR #1356 lists the paths). The
drivers have since been changed to give every session a fresh temporary
`CLAUDE_CONFIG_DIR`, deleted on close, with the login passed in as
`CLAUDE_CODE_OAUTH_TOKEN` from `claude setup-token`. Of the figures
below, only A.13's re-runs were measured that way (2026-10-14); A.11,
A.12, A.14 and A.15 have not been re-run. Nothing measured here depends on the profile: user
settings were already dropped by `--setting-sources local`.

Totals over the
63 `result`s of 38 sessions: input 228, cache read 603,138, cache write
149,881, output 13,667 (766,914 tokens, 79% cache reads).

Code reading used `strings -n 6` over the 2.1.296 binary
(`.claude-wrapped`, a Bun bundle); a statement marked "(code)" was read
there and not run.

### A.11 The messaging socket (E16)

Script `s0_socket.py`, flags `--tools Bash,Read --allowedTools
"Bash(printenv:*)" --permission-prompt-tool stdio`. Each variant ran one
turn, read `system/init.messaging_socket_path`, then connected to that path
as a plain same-uid Python process with no token and wrote three lines, one
connection each: `{"type":"user","message":{"content":"Reply with exactly:
SOCKET-NEXT"},"from":"s0-probe"}`, the same with `"priority":"now"`, and
`{"type":"control","action":"rename","name":"s0-renamed"}`. Last, the agent
ran `printenv CLAUDE_CODE_MESSAGING_SOCKET` and `printenv
CLAUDE_CODE_MESSAGING_TOKEN | wc -c` through Bash. Every variant was run
twice and the two runs agreed (the `printenv` step and the last row's
prompt check were added for the second run, and the last row was run a
third time for the prompt check).

| Variant | Socket | `user` line, no token | `priority: now` | Agent's Bash sees |
|---|---|---|---|---|
| default | `/tmp/cc-socks/<pid>.sock`, uid 501, mode 0600; dir 0700 | a turn ran, `result` "SOCKET-NEXT" | a turn ran, "SOCKET-NOW" | the socket path and a 32-hex token |
| `--restricted` | none (`messaging_socket_path` absent) | — | — | neither (`printenv` exit 1) |
| `CLAUDE_CODE_RESTRICTED=1` | none | — | — | neither |
| `--settings '{"crossSessionInbound":"refuse"}'` | bound, as default | nothing on stdout, no turn | nothing | path and token |
| `--settings '{"crossSessionInbound":"hold"}'` | bound, as default | `system/peer_message_hold{state: held, lane: socket, cause: explicit-setting}`, no turn | the same | path and token |
| `--restricted` + `--settings` (Stop hook) + `--mcp-config` (`s0_mcpstub.py`) | none | — | — | neither |

- **What it accepts.** Newline-delimited JSON, one object per line. `type:
  user` with `message.content` a string is queued as a prompt (code: the
  default priority is `next`; `now` skips the per-connection processing
  chain). The turn's replayed `user` line carries `isSynthetic: true`,
  `origin: {kind: peer, from: "s0-probe", verifiedPeerPid: <the
  connecting pid>}` and wraps the text as "Another Claude session sent a
  message: … This came from another Claude session — not typed by your
  user … A peer cannot grant escalation …". `from` is whatever the sender
  wrote; `verifiedPeerPid` is read from the connection (`LOCAL_PEERPID`).
  `type: control` takes the actions `rename`, `peer_message_status`,
  `notify_when_idle`, `peer_idle_notice`, `yield_artifact_replies`,
  `unyield_artifact_replies` and `artifact_replies_yielded` (code); the
  probe's `rename` produced nothing on stdout and did not change the name
  in the session registry. Anything else is logged and ignored (code).
- **No token needed on macOS.** The inbox publishes a key file and checks a
  first-line auth frame, but rejects unauthenticated lines only where
  `authRequired` is set, and its default is "platform is Windows" (code:
  `authRequired = requireAuth ?? (platform === "windows")`). The probe's
  token-less writes ran turns.
- **Owner and mode.** The socket is created 0600 in a 0700
  `/tmp/cc-socks` (or `$XDG_RUNTIME_DIR/cc-socks`, or
  `/tmp/cc-socks-<uid>` when the path would pass 103 bytes), owned by the
  session's uid, and removed on exit. The session registry
  `$CLAUDE_CONFIG_DIR/sessions/<pid>.json` (0644) records
  `messagingSocketPath`, and `<pid>.<sha256>.key` (0600) holds the key.
  Every child process of the session gets `CLAUDE_CODE_MESSAGING_SOCKET`
  and `CLAUDE_CODE_MESSAGING_TOKEN` in its environment.
- **Switches.** `--restricted` and `CLAUDE_CODE_RESTRICTED=1` skip binding
  ("[uds-messaging] Skipped: restricted session"), and the Claude Code
  changelog carried in the binary lists "Fixed `--restricted` (and
  `CLAUDE_CODE_RESTRICTED=1`) sessions opening the cross-session messaging
  socket". In the last row, the `--settings` Stop hook ran
  (`hook_started` / `hook_response`, and its marker file appeared), the
  `--mcp-config` server was `connected` and its tool was called, and a
  `touch` still arrived as `can_use_tool` over `--permission-prompt-tool
  stdio` (A.12 to A.15 themselves ran without `--restricted`). Other effects of
  restricted mode that the code shows: no user settings are read, auto
  memory is off, cloud sessions and teleport are refused, Claude in Chrome
  does not connect. `tools`, `slash_commands` (55) and `capabilities` in
  `system/init` were the same as the default run.
  `crossSessionInbound` (`accept | hold | refuse`, read from policy, flag
  and user settings, so `--settings` sets it) leaves the socket bound:
  `refuse` drops silently, `hold` reports `peer_message_hold` and, at exit,
  `state: dropped, outcome: discarded`.

### A.12 The deferral carrier: `deny` versus `allow` (§2a)

Script `s0_carrier.py`, flags `--tools Bash,Read,AskUserQuestion
--permission-prompt-tool stdio`. Prompt: two independent tasks, "use the
AskUserQuestion tool to ask me which colour I prefer for the report, red
or blue", then "run `ls` … and count the files", finishing with one line
per task. The driver answered the AskUserQuestion `can_use_tool` with the
§2a text `q-6854666e9377 submitted for user feedback - the response will be
relayed to you` (the id computed as §2a specifies over agent `s0-probe`,
the prompt, the option labels and `multi_select`), carried as
`{behavior: deny, message: <text>}` or as `{behavior: allow,
updatedInput: {…input, answers: {<question>: <text>}}}`. A second turn then
sent the §2a reply `Answer to q-6854666e9377: blue`. Four runs per carrier.

| | `deny` (4 runs) | `allow` (4 runs) |
|---|---|---|
| tool result the model saw | `is_error: true`, content exactly the text | `is_error: false`, `The user answered: "Which colour do you prefer for the report: red or blue?"="q-6854666e9377 submitted for user feedback - the response will be relayed to you". Read the answers carefully — they may request clarification, changes, or that you not proceed — and follow what they actually say.` |
| went on to run `ls` in the same turn | 4/4 | 4/4 |
| re-asked, or any later control request | 0/4 | 0/4 |
| turn end | one `result`, `success`, `end_turn`, `num_turns` 3 (one run 4) | one `result`, `success`, `end_turn`, `num_turns` 3 |
| `result.permission_denials` | 1 entry: `AskUserQuestion` with its `tool_use_id` and input | empty |
| `system/permission_denied` event | none | none |
| final text on task A | "no answer has come back yet … submitted for your feedback" (4/4) | "returned a submission receipt / ID, not red or blue" (4/4) |
| reply turn used "blue" | 4/4 | 4/4 |

Both carriers keep the model working, so by §2a's rule S6b uses `deny`. Its
tool result is the deferral text byte for byte; `allow`'s is the harness's
"The user answered" frame around it.

### A.13 Parallel control requests and an ask while a permission is open (§2b)

Script `s0_parallel.py`, flags `--tools Bash,Read,AskUserQuestion
--permission-prompt-tool stdio` (no `--allowedTools`, so `touch` prompts).
The prompt asked for two tool calls in one response, in a given order. The
driver left the first `can_use_tool` open for 10 s, then answered it
(`deny` for Bash, `allow` with answers for AskUserQuestion). Every run
produced both `tool_use` blocks in one API message (same `message.id`),
complete within 0.37 s of each other. Two runs per shape.

| Shape (order in the message) | First `can_use_tool` | Second, while the first was open | Second after the first was answered |
|---|---|---|---|
| `touch`, then AskUserQuestion | Bash | never in 10 s | AskUserQuestion, 5–6 ms after the answer |
| AskUserQuestion, then `touch` | AskUserQuestion | never in 10 s | Bash, 12–16 ms after |
| `touch`, `touch` | Bash | never in 10 s | Bash, 7–8 ms after |
| `touch`, then a pre-allowed MCP tool (`s0_mcpstub.py` `echo`, in `--allowedTools`) | Bash | — (no prompt); its result arrived 5 ms after the Bash answer, not before | — |
| the MCP tool, then `touch` | Bash, after the MCP result (0.34–0.36 s) | — | — |

What this shows: in a message of non-concurrency-safe calls (Bash
`touch`, AskUserQuestion, and an MCP tool without a read-only hint), only
one `can_use_tool` was ever open, and no call later in the message ran,
prompting or not, until the open one was answered. A pre-allowed MCP call
ordered before the prompting one ran first.

**Re-run on the token path, 2026-10-14.** Every shape below ran on the
token path of the S0 preamble (a fresh `CLAUDE_CONFIG_DIR` per session,
the login passed as `CLAUDE_CODE_OAUTH_TOKEN` from `S0_OAUTH_TOKEN_FILE`),
Claude Code 2.1.296 (from each capture's `system/init`), model `haiku`,
two runs each. All runs exited 0 and no token appeared in the output.
Times are seconds from session start.

*Controls, no permission open.* `s0_parallel.py 2 10 sleep_then_mcp
slowtouch_then_mcp`. Both shapes put the same pre-allowed MCP call behind a
pre-allowed Bash call: `sleep 5` (in `--allowedTools` as `Bash(sleep:*)`),
and `sleep 5 && touch` (also `Bash(touch:*)`), which writes and so is not
concurrency-safe even if `sleep` alone were classed read-only. No
`can_use_tool` was sent. They ran twice: first with no `--permission-mode`,
when `system/init.permissionMode` was `auto` (A.17, #1406; #1307, comment
6165573342), then with `--permission-mode default`. Both calls were in one
API message in every run. `tool_use` → `tool_result`:

| Control | Mode | Run | Bash | MCP `echo` |
|---|---|---|---|---|
| `sleep 5`, then MCP | `auto` | 1 | 2.62 → 8.42 | 2.70 → 8.43 |
| `sleep 5`, then MCP | `auto` | 2 | 1.91 → 7.21 | 1.99 → 7.21 |
| `sleep 5 && touch`, then MCP | `auto` | 1 | 2.25 → 7.55 | 2.35 → 7.56 |
| `sleep 5 && touch`, then MCP | `auto` | 2 | 2.11 → 7.41 | 2.21 → 7.42 |
| `sleep 5`, then MCP | `default` | 1 | 1.980 → 7.256 | 2.077 → 7.260 |
| `sleep 5`, then MCP | `default` | 2 | 2.187 → 7.461 | 2.284 → 7.465 |
| `sleep 5 && touch`, then MCP | `default` | 1 | 2.469 → 7.739 | 2.481 → 7.743 |
| `sleep 5 && touch`, then MCP | `default` | 2 | 2.794 → 8.057 | 2.806 → 8.061 |

In all eight runs the MCP result came only after the slow Bash result, 4 to
5 ms behind it: with no permission open, a later call waits for an unfinished
non-concurrency-safe call before it.

*The shapes of the first table, in `default`.* `s0_parallel.py 2 10
touch_then_ask ask_then_touch touch_then_mcp mcp_then_touch touch_touch`.
The first `can_use_tool` was held open for 10 s, except in `mcp_then_touch`,
where it was answered at once.

| Shape | Run | First `can_use_tool` | While it was open | Answered | Next, after the answer |
|---|---|---|---|---|---|
| `touch`, then AskUserQuestion | 1 | Bash, 1.919 | nothing | 11.929 | AskUserQuestion prompt, 4 ms |
| `touch`, then AskUserQuestion | 2 | Bash, 2.104 | nothing | 12.113 | AskUserQuestion prompt, 4 ms |
| AskUserQuestion, then `touch` | 1 | AskUserQuestion, 2.302 | nothing | 12.310 | Bash prompt, 16 ms |
| AskUserQuestion, then `touch` | 2 | AskUserQuestion, 2.348 | nothing | 12.358 | Bash prompt, 12 ms |
| `touch`, then MCP | 1 | Bash, 3.017 | no MCP result | 13.018 | MCP result, 9 ms |
| `touch`, then MCP | 2 | Bash, 3.449 | no MCP result | 13.520 | MCP result, 11 ms |
| `touch`, `touch` | 1 | Bash, 2.680 | nothing | 12.687 | Bash prompt, 7 ms |
| `touch`, `touch` | 2 | Bash, 2.047 | nothing | 12.057 | Bash prompt, 8 ms |
| MCP, then `touch` | 1 | Bash, 2.381, after the MCP result at 2.082 | — | 2.381 | — |
| MCP, then `touch` | 2 | Bash, 2.679, after the MCP result at 2.517 | — | 2.679 | — |

This repeats the first table under the token path: one `can_use_tool` at a
time, nothing later in the message before the answer, and a pre-allowed call
ordered first ran first.

*A concurrency-safe call behind an open permission.* `s0_parallel.py 2 10
read_behind_permission` asks for a Bash `touch` (which prompts) and then a
`Read` of `a.txt` (which does not prompt in `default`) in one message, and
holds the `touch` permission open for 10 s.

| Run | One message | Bash `can_use_tool` | `Read` `tool_use` | Other prompts while open | Answered (`deny`) | Bash result | `Read` result |
|---|---|---|---|---|---|---|---|
| 1 | yes | 2.230 | 2.470 | none | 12.480 | 12.484 | 12.492 |
| 2 | yes | 2.438 | 2.792 | none | 12.793 | 12.797 | 12.804 |

The `Read` result did not arrive while the permission was open in either run
(`read_result_before_permission_answered: false`). It came 7 to 8 ms after
the Bash result. A first attempt, with no `--permission-mode`, started in
`auto`. Its `touch` raised no `can_use_tool`, so it did not test this case
(A.17).

**Reading.** Nothing in a message ran past an open permission. That held
for a prompting call, an AskUserQuestion, a pre-allowed MCP call without a
read-only hint and a concurrency-safe `Read`. Only one `can_use_tool` was
ever open, and a call ordered before the prompting one ran first. Finding
3's caveat that one open request "may hold only for non-concurrent calls"
is resolved in the strong direction: an open permission held a
concurrency-safe call behind it too. Separately, the controls show that an
unfinished non-concurrency-safe call holds later calls with no permission
open. Not separated: whether the `Read` waited on the permission itself or
on the earlier Bash call in message order. A `Read` behind a slow,
pre-allowed Bash with no permission open was not probed. Also not probed:
an MCP tool with `readOnlyHint` in that position, and a concurrency-safe
call ordered before the prompting one.

### A.14 The harness's own queue in `-p` (§3)

Script `s0_queue.py`, flags `--tools Bash --allowedTools "Bash(sleep:*)"
--permission-prompt-tool stdio`. Input M1 "Run `sleep 8` with Bash, then
reply with exactly: DONE-1" (flk-chosen uuid); one second after the
`sleep` tool call, input M2 "Reply with exactly: M2-ANSWERED" (flk-chosen
uuid), then the control request of each row. Times are seconds from start.
The verbs come from the `-p` control loop in the binary (`print.ts`
handles `interrupt`, `cancel_async_message`, `set_permission_mode`, …).

| Scenario | Control request | Response | Lifecycle and outcome |
|---|---|---|---|
| plain | none | — | M2 `queued` 3.82; the sleep ends 11.07; M2 `started` 11.07; one `result` 12.92, text "M2-ANSWERED" (M1's "DONE-1" is never written); M2 then M1 `completed` |
| takeback | `cancel_async_message{message_uuid: M2}` while M2 is `queued` | `{cancelled: true}` within 1 ms | M2 `cancelled` at once; the sleep runs to its end; one `result` "DONE-1" |
| takeback_late | the same, after M2 reported `started` | `{cancelled: false}` | M2 runs; `result` "M2-ANSWERED" |
| cancel_queued | `interrupt{cancel_queued: true}` | `{still_queued: [], cancelled: [M2]}` in 2 ms | M2 `cancelled`; the running sleep is rejected ("The user doesn't want to proceed with this tool use …"); `result{error_during_execution, terminal_reason: aborted_tools}`; M1 `cancelled` |
| send_now | `interrupt{send_now: true, message_uuid: M2}` | `{still_queued: [M2], send_now: "stopped"}` in 1 ms | the sleep is rejected 1 s into its 8; `result{error_during_execution, aborted_tools}`; M1 `cancelled`; M2 `started` at once; a second `result` "M2-ANSWERED" 0.8 s later |
| edit | `edit_queued_message{message_uuid, message}` (no such verb) | `{subtype: error, error: "Unsupported control request subtype: edit_queued_message"}` | M2 runs unchanged |

- **Take-back** exists: `cancel_async_message` withdraws a `queued` input
  and reports `command_lifecycle{cancelled}`; once the input has
  `started` it answers `cancelled: false` and does nothing.
- **Edit** does not exist. No control subtype in the binary edits a queued
  input (the subtype list read by `strings` has none), and an unknown
  subtype is answered with an `error` response naming it.
- **Send-now is an interrupt.** `send_now` stops the running turn,
  including a tool that is mid-run, cancels the turn's input, and starts
  the named queued input as a new turn. Its `send_now` field is `stopped`,
  `delivering` (the input is already going) or `nothing_waiting` (code).
  It does not fold the input into the running turn: a plain write does
  that (E4), but only at the next model step, so during a long tool the
  write waits for the tool (row "plain": 7.25 s).
- An interrupt during a tool ends `terminal_reason: aborted_tools`
  (A.5's, during streaming, was `aborted_streaming`).

### A.15 `set_permission_mode` over stdio (§10b, P25)

Script `s0_permmode.py`, flags `--tools Bash,Write --allowedTools
"Bash(sleep:*)" --permission-prompt-tool stdio`, started in `default`. Each
step asked for one `Write` of a new file; every `can_use_tool` was denied.
Run twice; the runs agreed.

| Step | Request | Response | That turn's `system/init.permissionMode` | Write prompted | File written |
|---|---|---|---|---|---|
| 1 | — | — | `default` | yes | no |
| 2 | `set_permission_mode{mode: acceptEdits}` at idle | `{mode: acceptEdits}` | `acceptEdits` | no | yes |
| 3 | `{mode: default}` at idle | `{mode: default}` | `default` | yes | no |
| 4 | `{mode: acceptEdits}` during the turn's `sleep 4`, before its Write | `{mode: acceptEdits}` | `default` (emitted at turn start) | no | yes |
| 5a | `{mode: bypassPermissions}` at idle | `error: "Cannot set permission mode to bypassPermissions because the session was not launched with --dangerously-skip-permissions"`, `error_code: bypass_not_launched` | — | — | — |
| 5b | `{mode: plan}` at idle | `{mode: plan}` | — | — | — |
| 5c | `{mode: bogus}` at idle | `error: "Cannot set permission mode: must be one of acceptEdits, auto, bypassPermissions, default, dontAsk, plan"`, `error_code: invalid_mode` | — | — | — |
| 6 | none; the turn is "Reply with exactly: ok" (second run) | — | `plan` | — | — |

The switch is in place and immediate: no respawn, and a switch during a
turn applies to that turn's next tool call. Step 6 shows `plan` stuck and
the refused requests left it unchanged. In the first run step 6 was a
Write, and in `plan` mode that wrote a plan file into
`$CLAUDE_CONFIG_DIR/plans/` (the probe created that directory; it was
removed). The second run's step 6 only reads the mode.

### A.16 `claude /login` for a macOS user with no GUI session (owner decision 1)

Run by the operator on 2026-10-14 (#1307, comments 6165592953 and
6165606960), as a throwaway non-admin macOS user, `s0probe` (uid 502),
entered with `sudo -iu`: a shell with no GUI login session. Claude Code
2.1.287. The runbook is `s0_login_flk_agent.sh`: it records the user's
keychain state, runs `claude auth login` with
`CLAUDE_CONFIG_DIR=$HOME/.flk/claude/s0-login`, checks where the token
landed without printing it, and runs one `-p` turn.

| Check | Result |
|---|---|
| `gui/<uid>` launchd domain | absent |
| default keychain | none (`SecKeychainCopyDomainDefault user: A default keychain could not be found`) |
| `claude auth login` | the browser step completed, then `Login failed: Couldn't save your login. If your Mac's keychain is locked, unlock it and log in again.`, exit 1 |
| keychain item `Claude Code-credentials-<8 hex>` | not found (`security` exit 44) |
| `.credentials.json` in the config dir | not written |
| `claude auth status` | `loggedIn: false` |
| the `-p` turn | exit 1 |

The plaintext fallback read from the 2.1.296 code (below) did not happen on
2.1.287 when the user has no keychain at all. So `/login` cannot persist a
login for a user with no GUI session on that version.

**The token path, same user and shell.** A `claude setup-token` token was
installed as a 0600 file in the user's home. `CLAUDE_CODE_OAUTH_TOKEN=<the
file's content> CLAUDE_CONFIG_DIR=<a fresh temp dir> claude -p --model haiku
--no-session-persistence 'Reply with exactly: ok'` printed `ok` and exited
0. The setup-token path works for a user with no GUI session and no
keychain. Not reported: the repeat from a new shell and after a reboot
that the runbook asks for.

What the code shows (code, not run): the OAuth blob is written with
`security add-generic-password -U -a "$USER" -s "Claude
Code-credentials-<first 8 hex of sha256(CLAUDE_CONFIG_DIR)>"` (the formula
was checked against the work profile's existing item) into the user's
default keychain. If that write fails and the failure is not "locked"
(`security` exit 36) for an item the keychain already holds, the token is
written to `.credentials.json` in the config dir instead (telemetry
`plaintext_fallback_used`). That reading is of 2.1.296; the run above, on
2.1.287 with no default keychain, did not reach it. 2.1.296 was not run as
that user.

### Open S0 items

None. S0 completed on 2026-10-14 (#1404). The A.13 controls, the A.13 shapes
on the token path and the concurrency-safe call behind an open permission
are recorded in A.13. A.16 (`/login` cannot persist a login for a user with
no GUI session, and the token path works) is recorded in A.16, and owner
decision 1 is narrowed. The launch profile's permission mode is a design
question for S3, #1406.

### A.17 Gotchas met while probing

- `--no-session-persistence` keeps the transcript out of the profile but
  still creates `projects/<cwd>/memory/` and `session-env/<session id>/`
  (both empty here). In these runs that was 33 and 23 directories in the
  operator's profile, removed by matching the captured session ids and
  working directories (see the isolation caveat above). Each running
  session also writes `sessions/<pid>.json` and a key file there (A.11).
  These are observations from the profile directory, not from a capture.
- A permission prompt answered `deny` with a message is not a
  `system/permission_denied` event: in A.12 to A.15 none was emitted for
  any stdio deny; only `result.permission_denials` records it. This rests
  on the uncommitted captures of `s0_carrier.py`, `s0_parallel.py` and
  `s0_permmode.py`; re-running them reproduces it.

- **Without `--permission-mode`, the startup mode is not set by the
  flags.** With the same flags, the A.15 runs in the operator's profile
  started in `default`, while every token-path session with an empty
  config dir (the four A.13 controls and the two first
  `read_behind_permission` runs, 2.1.296) started in `auto`. There, a Bash
  `touch` with no allow rule ran with no `can_use_tool` and no
  `permission_denials`. In the code, the startup mode with no CLI mode is
  resolved from settings `defaultMode`, a proactivity level and the
  auto-mode gate `tengu_auto_mode_config` (code). Which of them picked
  `auto` here was not determined. The drivers now pass `--permission-mode
  default`. §1's launch command passes no `--permission-mode` either, and
  presets (§10b), which carry one, come only with S19, so an S3 session's
  startup mode would depend on this default. That is a design question,
  filed as #1406, and not decided here.

### S0 findings for the architect

Each item names the body section whose assumption it changes. This list
records S0's findings and the completed follow-ups; the body was amended
from them on 2026-10-14 (#1324, #1404), and each item ends with where.

1. **§1 "No second input door", E16, "Failure modes" (the socket).** The
   socket accepts unauthenticated `user` lines from any process of the
   session's uid on macOS and runs them as turns (A.11), and the agent's own
   Bash children are handed its path and token. Under §1a every structured
   agent runs as `flk-agent`, so without a switch any structured agent
   could start turns in any other one, past the queue. A supported switch
   exists: `--restricted` (or `CLAUDE_CODE_RESTRICTED=1`) binds no socket
   and sets neither variable, while `--settings` hooks, `--mcp-config`
   servers and the stdio prompt keep working. The launch profile can pass
   `--restricted`, and the "known limit" in "Failure modes" falls away.
   Restricted mode also turns off auto memory, reads no user settings and
   refuses cloud sessions (A.11), which fits §1's isolation, but S3 should
   diff its `system/init` against the unrestricted profile once more.
   *Applied in §1 (launch profile, isolation, "No second input door"),
   E16, "Failure modes" and the S3 row; noted under owner decision 5.*
2. **§2a placeholder result, §2 table.** Both carriers keep the model
   working (8/8), so S6b uses `deny`. The §2 carve-out applies to
   `result.permission_denials` only: no `system/permission_denied` event
   is emitted for a deny flk sends over stdio (A.12, A.17), so the §2 row
   "`system/permission_denied`, `result.permission_denials` →
   `agent.permission_denied`" gets every stdio deny from flk's own answer,
   not from that event. `allow` is framed by the harness as "The user
   answered: …", which §2a's "the model cannot mistake it for the user's
   answer" argued against.
   *Applied in the §2 table, §2a "The placeholder result" and the S6b
   row; noted under owner decision 3a.*
3. **§2b case 1, owner decision 3 (asking while a permission is open).**
   An `AskUserQuestion` later in the same message as a prompting call did
   not arrive while the permission was open, as §2b allowed for. Neither
   did the result of a pre-allowed MCP tool (no read-only hint) ordered
   after it (A.13), so the stated way out, `flock_question_ask`, did not
   escape the block within that message. Only a call ordered before the
   prompting one ran. The completed A.13 controls show that a slow
   non-concurrency-safe call held a later call with no permission open;
   an open permission also held a concurrency-safe `Read` behind it.
   The latter run did not separate waiting on the permission from waiting
   on the earlier Bash call in message order. §2b's advice is therefore
   "ask before, or in a message apart from, a call that may prompt". Only
   one open control request per session was observed in the probed shapes.
   *Applied in §1 "Stall and waiting" (open requests tracked by id), §2b
   case 1 and "Failure modes"; noted under owner decisions 3 and 3a.
   The completed controls and `Read` run (2026-10-14, #1404) are reflected
   in §1, §2b and decision 3's note.*
4. **§3 "The harness's own queue", S5c, owner decision 4.** Take-back
   exists (`cancel_async_message{message_uuid}`, `queued` only), so
   `queue.take_back` can wrap it. Edit does not exist, so flk's queue is the
   only edit surface. The harness's send-now (`interrupt{send_now: true,
   message_uuid}`) is an interrupt that kills the running tool and starts
   the item as a new turn: it matches §3's `delivery: interrupt`, not
   `steer`, and cannot "save a model step" without costing the running
   turn. `steer` stays a plain write, which folds only at the next model
   step (it waits out a running tool, A.14 "plain"). In that row the
   folded item also displaced M1's requested final reply, so a steered item
   can change what the running turn answers. `interrupt{cancel_queued:
   true}` cancels the harness's queued inputs and also interrupts the
   running turn.
   *Applied in §3 (the `steer` and `interrupt` delivery mechanics,
   `queue.take_back`, "The harness's own queue"), "Failure modes" and the
   S5c row; noted under owner decision 4, which is unchanged: "send now"
   still sets `steer`, and the harness's send-now serves
   `send_now { interrupt: true }`.*
5. **§10b, S19 (permission presets).** `set_permission_mode` works over
   stdio in `-p`, in place, at once, and mid-turn (A.15), so the mode
   part of a preset switch is `mechanism: in_place` for every mode except
   `bypassPermissions`, which is refused unless the session was launched
   with `--dangerously-skip-permissions`; a preset that ranks it needs a
   reload with that flag. Valid modes are `acceptEdits`, `auto`,
   `bypassPermissions`, `default`, `dontAsk`, `plan`. A switch to `plan`
   writes plan files into the agent's config dir.
   *Applied in §10b (with the modes and directions S0 did not probe), P25
   and the S19 row; noted under owner decision 15.*
6. **Owner decision 1, §1 (login as `flk-agent`).** The completed A.16 run
   on 2.1.287, as a user without a GUI session or keychain, found that
   `/login` failed and wrote no plaintext `.credentials.json`. The
   `setup-token` path worked with a 0600 token file and a fresh config dir.
   This supersedes the earlier plaintext-fallback hypothesis from reading
   the 2.1.296 code; that version was not run as the user (A.16).
   *The owner narrowed decision 1 to `setup-token` for agent users
   (2026-10-14, #1404); applied in §1 and the S3 row.*
7. **§9 capability probe.** An unsupported control subtype is answered at
   once with `error: "Unsupported control request subtype: <name>"`
   (A.14), so a capability probe can test a control verb such as
   `cancel_async_message` with a harmless request instead of a dry
   resume.
   *Not applied: §9's probe is unchanged; this is an option for S12 to
   weigh when it builds that probe.*


### Amendment: Uncertain spawn records

Owner decision, 2026-10-15 (#1461 review round 2): persist `uncertain=true`
before spawn, then capture and persist the child's birth identity immediately
when the owning supervisor receives the child, before enumerating descendants.
A failed identity capture is explicit; an authoritative wait on our own child
records its reaped status, including an immediate exit. Unknown descendants
still prevent automatic cleanup.

If a dead supervisor left no persisted child birth identity, quarantine the
record with its first timestamp and reason. A sweep reports that directory as
a distinct outcome, never signals a process or deletes the directory, and never
retries its recovery. Later sweeps report the stored quarantine unchanged.
Malformed or unsafe entries are retained and reported individually; they cannot
prevent recovery of other eligible sessions. A namespace-level containment
failure still refuses the whole scan.

Operator cleanup is explicit: inspect the retained directory and associated
processes, establish that none still uses it, then remove the directory by hand.
No automatic PID-only reaping and no cleanup CLI is introduced in this slice;
operator tooling is a follow-up owned by the architect.
