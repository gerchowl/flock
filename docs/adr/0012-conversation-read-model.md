# ADR 0012 — The conversation read model: canonical entries and a derived index

- Status: Proposed
- Date: 2026-08-11
- Issues: split out of ADR-0011 after three review rounds concentrated here.
  Depends on #150 only for remote serving.
- Decision owner: operator; data-model round performed the Codex mapping and the
  per-agent audit cited below.
- Companion: **ADR-0011** owns surfaces, transports, authorization, and the
  write model. This ADR owns what the GUI reads and where it comes from.

## Context

ADR-0011 decides that the GUI renders a conversation rather than a character
grid. That conversation has to come from somewhere, and the somewhere is agent
transcript files plus flock's own event log.

**flock barely reads what it already has.** Claude appends a JSONL transcript
per session. `agent_resume.rs:204` resolves a Claude session-id to a path safely
(rejects `/`, `\`, control chars, oversize; base is hardcoded
`~/.claude/projects/`) — but only for existence checks (`app/worktrees.rs:334`).
That guard covers **Claude-by-id only**; every other agent's resolution is this
ADR's problem. The hook lifts just the last assistant text and the recap
sentinel (`cli/hook.rs`).

**A transcript is mostly not conversation.** Measured over **66 transcripts /
97,711 entries**: only **62%** are `user` or `assistant`. The other **37%** are
session-scoped records —

| type | count | | type | count |
| --- | ---: | --- | --- | ---: |
| `attachment` | 5,296 | | `system` | 3,080 |
| `last-prompt` | 4,905 | | `pr-link` | 2,307 |
| `mode` | 4,894 | | `file-history-snapshot` | 1,407 |
| `ai-title` | 4,788 | | `file-history-delta` | 769 |
| `permission-mode` | 4,444 | | `worktree-state` | 587 |
| `queue-operation` | 3,124 | | `relocated` | 395 |

This measurement is the reason for Decision 1: a turns-only model would push a
third of every transcript into an escape hatch, which is not a compatibility
cushion but the common case.

**Transcript content is untrusted.** It contains whatever the agent read — web
pages, files, MCP output — plus whatever the operator pasted, including
credentials. ADR-0011's threat model applies; this ADR adds the ingest and
render surfaces.

## Decision

### 1. Entries, not just turns

```
TranscriptEntry = Turn | SessionEvent

Turn { role, timestamp, model_id?, usage?, stop_reason?, blocks[] }

Block = Text | Reasoning{ summary?, content?, opaque? }
      | ToolCall{ id, name, input, status? }
      | ToolResult{ call_id, ok, output }
      | Attachment{ mime, inline?, ref?, description? }
      | Task{ tool_call_id, agent_type?, description?, transcript_ref }
      | Unknown{ schema_version, raw }

SessionEvent = ModeChange | PermissionMode | Title | Compaction
             | FileHistory | Queued | SessionMeta | TurnContext
             | Unknown{ schema_version, raw }
```

Every addition is evidence-driven:

- **`SessionEvent`** renders in a timeline rail, not as blocks inside turns.
  Types the GUI does not acknowledge are **dropped at ingest and the dropped set
  is declared**; silently bucketing them is what made a turns-only model
  dishonest.
- **`model_id`, `usage`, `stop_reason` are first-class.** Claude records them on
  `message.model` / `message.usage`; Codex records the model on a *separate*
  `TurnContext` record. A conversation GUI that cannot say which model produced
  a turn is not the promised surface, so this must not live in an escape hatch.
- **`ToolCall.id` / `ToolResult.call_id`.** Both formats carry a call id;
  positional inference breaks whenever a turn issues parallel tool calls, which
  is routine.
- **`Attachment` distinguishes inline from referenced.** Codex inlines images
  (`Message.content = InputImage{image_url}`); Claude stores them separately,
  and real tool results carry image lists.
- **`Task` replaces a parent pointer for subagents.** Claude subagents live in a
  *separate* `subagents/agent-*.jsonl` with a `.meta.json` sidecar linked by
  `toolUseId` — a parent field cannot express a file boundary or carry
  `agentType`/`description`. Codex has no subagent concept, so a generic
  `parent?` was a Claude affordance in disguise.
- **`Unknown` is budgeted.** It carries a schema version, and the GUI reports
  the share of an entry that fell through. Above ~5% on a supported agent is
  schema debt to file, not normal rendering.
- **`Turn.role` for tool output.** Claude synthesizes `role: user` for
  `tool_result`; Codex's `FunctionCallOutput` has no role. A ToolResult-only
  `Turn` takes the synthesized role `tool`.

### 2. Rendering is constrained by security, not taste

Every block can carry attacker-controlled bytes — a `tool_result` from a fetched
URL, a file the agent read, MCP output, a pasted `.env`. Blocks render as plain
text or through an allow-listed Markdown subset that strips raw HTML,
`javascript:`, and non-`https:`/`mailto:` URLs. No `innerHTML` /
`dangerouslySetInnerHTML` / `v-html` may touch transcript content. ANSI escapes
render only through a sanitizing, length-capped converter. Richer rendering
requires a follow-up ADR.

This is not defence-in-depth, it is the boundary: ADR-0011 Decision 3 puts the
app on the origin holding the `rpc` bridge, so stored XSS here is RCE against
the write model.

### 3. The Codex mapping, performed

Codex persists `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` with five
top-level record kinds. Mapping:

| Codex | canonical |
| --- | --- |
| `ResponseItem::Message` (InputText/OutputText) | `Turn` + `Text` |
| `ResponseItem::Reasoning` (summary, content) | `Reasoning` |
| `Reasoning.encrypted_content` | `Reasoning.opaque` — round-trips, never renders |
| `FunctionCall` / `LocalShellCall` / `CustomToolCall` / `WebSearchCall` | `ToolCall{id,name,input,status}` |
| `FunctionCallOutput` / `CustomToolCallOutput` | `ToolResult{call_id,…}` in a `role: tool` Turn |
| `Message.content = InputImage{image_url}` | `Attachment{inline}` |
| `SessionMeta`, `TurnContext`, `Compacted`, filtered `EventMsg` | `SessionEvent` |

Two residual mismatches are accepted and named: Codex tool outputs carry no
role (hence the synthesized `tool` role), and `EventMsg::{UserMessage,
AgentMessage, AgentReasoning}` duplicate the `ResponseItem` path, so the adapter
must dedupe. Codex has no subagents and no typed-asks analog.

**Codex is the second implementation that proves generality**, and is scheduled
rather than deferred (Sequencing step 2). Designing against one format produces
that format wearing a trait.

### 4. Capabilities, not tiers

An earlier draft proposed a three-tier support ladder. The audit found **Tier 1
= {claude}, Tier 2 = {} — empty, not "may be empty"**: no shim implements
prompt/reply reporting, and Opencode appears in `cli/hook.rs`'s `Agent` enum
only for session-id forwarding. A tier with no members is marketing, so tiers
are deleted. Adapters declare capabilities:

`has_transcript`, `has_reasoning`, `has_subagents`, `model_identity_per_turn`,
`usage_per_turn`, `attachments{inline|referenced}`, `compaction_events`,
`mode_switches`, `queued_prompts`, `live_tail`, `typed_asks`.

`typed_asks` is deliberately not "structured asks": ADR-0011 Decision 6's
mechanism is a Claude hook with no analog elsewhere.

`IntegrationTarget` (`schema.rs:1049`) covers **nine** agents — `Pi, Omp,
Claude, Codex, Copilot, Kimi, Opencode, Hermes, Qodercli` — while `cli/hook.rs`
reports for two. Codex and Pi are transcript-capable on disk with no reader
written; they are the cheapest second implementations. The GUI codes against
capability bits and falls back to ADR-0011's terminal view when
`has_transcript` is false. **Degradation is partial**: actions stay
agent-agnostic, so an agent with no readable transcript still gets the full GUI
for lifecycle, messaging, and worktrees.

### 5. A derived index, tailed — never the source of truth

The server ingests into a local SQLite index (FTS first; vectors only when
search demonstrably hurts).

- **Ingest is a tail.** Transcripts are append-only — verified by observing
  `/compact` land at line 75 of a 704-line file with all 74 preceding lines
  intact. Track a byte offset per file; read offset→EOF on change; re-ingest
  from zero if the file shrinks or its leading bytes change.
- **Single-writer is an assumption.** Two processes appending to one session
  file interleave partial lines, which the offset guard cannot detect (the file
  only grows, its head is intact, the middle is torn). The tailer validates each
  line as JSON and refuses-and-logs on failure.
- **Ingest paths pass a per-agent allow-list.** `valid_session_path`
  (`agent_resume.rs:274`) checks only non-empty, length, no control chars, and
  `is_absolute()` — a hook-reported `session_ref` of
  `{kind: Path, value: "/Users/…/.ssh/id_rsa"}` passes it. Each agent therefore
  declares a permitted root; candidates are canonicalized, required to have that
  root as a prefix, and rejected if any component is a symlink. Violations are
  dropped with an audit event, never followed.
- **Ingest is bounded**: max line length, max blocks per turn, max JSON depth,
  refuse non-UTF-8. A refused line advances the offset and emits an audit event;
  it never blocks the tailer. Rebuild-from-scratch is no defence against a
  hostile blob that reappears on rebuild.
- **Compaction is an epoch, not a turn.** `isCompactSummary` entries supersede
  prior turns that remain in the file. The GUI renders a compaction boundary and
  collapses superseded turns behind it; showing both flat is duplicative.
- **Subagent files are tailed too**, or their content is silently missing.
- **Joining the two logs needs a conversion, not a coincidence.**
  `event_hub.rs:12` stores `ts_ms: u64` (Unix ms, *server* clock); transcripts
  store ISO-8601 strings like `"2026-08-11T06:48:36.460Z"` written by the
  *agent* CLI. Same-host they align after parsing; cross-host they are subject
  to clock skew and must not be joined blind. The join is what makes "pane went
  Blocked at T" answerable against "this tool call at T", which neither log
  answers alone — and it settles the two-logs drift without making either
  canonical.
- **Search is parameterized.** Client terms are quoted-and-escaped so no FTS5
  operator is honoured from input; column filters (`project_key`, `pane_id`,
  `role`) are separate SQL predicates. The client never composes SQL or FTS.
- **Never authoritative.** The agent owns those files; rebuild is always valid,
  and the index holds no state that cannot be reconstructed.
- **The DB is not the surface.** Decision 1's model is the API surface; SQLite
  is how the server answers quickly. Exposing the schema would re-couple the GUI
  to storage exactly as it is currently coupled to a character grid.
- **No cross-host replication** — ADR-0009 decided this for the event log on
  measurement, and transcripts are larger. Cross-host reads ride the held-SSH
  relay on demand.
- **Watch only active panes'** transcripts, resolved via `session.json`
  (`persist/snapshot.rs:12`), not the whole tree.

### 6. Reads are authorization-scoped, not merely identity-gated

Identity-gating is right for writes and wrong for reads: the point of a phone
client is that a tailnet peer sees *some* fleet state, not every `.env` fragment
an agent grepped. Each configured identity carries an optional `projects`
allow-list (unset = all). Filtering happens **at query time**
(`WHERE project_key IN (…)`), never in the presentation layer, and scoped
identities cannot issue cross-project `MATCH` — otherwise match counts and
highlighted snippets leak exactly what the filter hides.

## Sequencing

1. **Canonical model + Claude adapter + index.** Server-side; independent of
   ADR-0011's TS-type generation.
2. **Codex adapter** — the generality proof, not a later nice-to-have.
3. **Transcript API** (pagination, tail, search) with Decision 6 scoping —
   needed the moment a networked shell exists.
4. **Cross-host reads** over the ADR-0009 relay.

## Alternatives considered

**A `Turn`-only model with an `Opaque` escape hatch.** Rejected on measurement:
37% of 97,711 real entries are session-scoped, so the hatch becomes the common
case and load-bearing data (model identity, usage) disappears into it.

**A three-tier support ladder.** Rejected: the audit found Tier 2 empty.
Capability bits describe reality; tiers described a hope.

**DB as source of truth.** Rejected: the agent owns the transcript files; we can
only ever hold a derived index.

**Replicate transcripts across hosts.** Rejected per ADR-0009's measured finding
for the smaller event log.

**Ingest-time redaction.** Rejected as a non-goal — the source is unredacted,
and partial redaction in the index is false comfort that invites treating the
index as safe.

## Consequences

- **The conversation view is Claude-only at launch.** One agent has a readable
  transcript through flock. Codex and Pi are transcript-capable on disk with no
  reader; every other agent falls back to ADR-0011's terminal view.
- **We couple to undocumented, unstable formats.** Neither Claude's nor Codex's
  transcript schema carries a compatibility contract. `Unknown` blocks with a
  reported fall-through budget, plus rebuildable indexes, are the mitigation; a
  schema change is a when, not an if.
- **Retention has a stated default**: the ingested copy is discarded once the
  source transcript is gone, plus a 30-day grace. Storage is a `0o600` SQLite
  file under the platform data dir, excluded from sync and backup surfaces by
  default, with `--pane` and `--project` purge commands, both audit-logged.
  Encryption at rest is deferred, with a stated preference for filesystem-level
  encryption over SQLCipher.
- **The index outlives its source.** Claude's transcripts age out (~6 weeks
  observed); the ingested copy does not until the rule above fires. That is
  durable history and a secret-retention liability at once, which is why the
  default is stated rather than emergent.
- **Read-side audit logging**: transcript reads and searches are logged with
  identity, scope, and result count, alongside ingest refusals (path outside
  root, oversized line, symlink target). Append-only, complementing ADR-0011's
  write-side audit.
- **Transcripts may contain secrets** — `.env` fragments, tokens, private
  source. They inherit the identity gate and Decision 6 scoping.
- **adr-matrix**: Proposed trips no gate. On flip to Accepted, add a
  FEATURE-MATRIX.md row citing ADR-0012.
