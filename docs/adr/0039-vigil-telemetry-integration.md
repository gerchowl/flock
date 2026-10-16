# ADR-0039: flk's logs, metrics and traces go through vig-os/vigil as per-role OTLP/JSON Lines files, carry ids and never content, and are read back only by the flk server

- Status: Proposed
- Date: 2026-10-16
- Issues: [#1614](https://github.com/gerchowl/flock/issues/1614) (this record), [#1300](https://github.com/gerchowl/flock/issues/1300) (adoption, gated on a vigil release), [#1323](https://github.com/gerchowl/flock/issues/1323) (superseded by the owner's vigil decision), [#1619](https://github.com/gerchowl/flock/issues/1619) (G13, the GUI views)
- Builds on: ADR-0038 §4a and §11 item 5 (the owner chose vigil on 2026-10-16; the GUI's telemetry views); ADR-0036 §4 and §5 (session streams, wire scopes, `cmd_id`); ADR-0010 (`flk report`); ADR-0005 and ADR-0028 (the durable event log, which stays state, not telemetry); ADR-0014 and ADR-0035 (operator-only verbs); ADR-0026 (mesh)
- Implemented: nothing. vig-os/vigil is pre-alpha on 2026-10-16: no release, and its `src/` holds only a version constant. Every implementation slice below waits for the vigil release that ships the API it uses.

## Summary

flk keeps one logging facade (`src/logging.rs`). Behind it, vigil replaces today's hand-rolled rotating JSON writer and adds spans and metrics. Each process role writes its own OTLP/JSON Lines files under the data dir, one file per signal. Telemetry carries ids, enums, numbers, fixed message templates, typed paths, and masked text of flk's own errors only: never message bodies, prompts, tool or harness output, comment text or tokens. That is a schema change from today's facade, which logs free-text errors and raw paths. It is done first, on today's writer (V1a). The flk server is the only reader the GUI uses (ADR-0038 §4a). `flk report`, `peers logs`, `tail_session_logs` and the log-budget gate are migrated in the release that switches the format, not left reading a dead file. Spans correlate with the transcript through ids the session stream already has, so the stored session stream does not change. Cross-node trace propagation waits for the next mesh protocol version.

## Context

Measured in the tree on 2026-10-16 (dev at 8296d8a2):

- **Logs.** `init_file_logging` (`src/logging.rs`) is the only subscriber: one `tracing_subscriber::fmt().json()` layer, `FLOCK_LOG` as the filter (default `flk=info,flock=info`), and a size-rotating writer (5 MiB, 2 rotated files, no age limit) that is safe within one process only. That is why each role has its own file: `flock-server.log`, `flock-client.log`, `flock.log` (CLI and remote), `flock-relay.log`, and `flock-mcp-<pid>.log`, with dead-pid files pruned. Every event goes through a one-line facade function with a stable `event` name (for example `api.request.start`), a `subsystem`, an `outcome` and typed fields. The `no-raw-trace-fields` gate confines `?`/`%` formatting to `logging.rs`.
- **Spans and ids.** There are none: no `#[instrument]`, no span macros, no trace or turn id. The correlators that exist are the API `request_id`, the mesh `correlation_id`, and `MessageKey { origin_node, message_id }`.
- **Metrics.** There are none. Nothing counts queue depth, delivery latency, stalls or wire subscribers, and nothing is exported.
- **Readers.**
  - `flk report` (ADR-0010) reads the four fixed log files client-side and scrubs them with an allowlist (`src/report/redact.rs`).
  - `peers logs` ships `LogLine` over SSH, using `tail_session_logs` with `SESSION_LOG_FILES`.
  - The pre-push `log-budget` hook runs the external `guardrails-log-budget` over a flat JSONL sample (`log-budgets.toml`: `event_field = "event"`).
  - Tests use the log files as fixtures.
- **vigil's documented design** (README; not yet code):
  - `vigil::init(service)` writes OTLP/JSON Lines per the OTel file-exporter spec: one `LogsData`, `MetricsData` or `TracesData` per line, one signal per file, 64-bit integers as decimal strings, hex ids.
  - Size rotation and age retention.
  - Multi-process safety on one file.
  - An opt-in `audit` feature: a hash-chained, Merkle-rooted, ed25519-checkpointed trail of OTLP log records, separate from telemetry.
  - Milestones: 0.1.0 logs, 0.2.0 metrics and traces, 0.3.0 audit.

## Decision

### 1. What flk emits

**One facade, three signals.** `src/logging.rs` stays the only module that names telemetry fields. A new event, span or instrument is a facade function there, so the redaction rule (§4) is enforced in one file, and the existing gate extends to it.

**Logs.** Every facade event becomes an OTLP `LogRecord`:

| Today's field | OTLP |
|---|---|
| `event` | the `event.name` attribute (same string; renames are a breaking change for readers, §6) |
| `subsystem`, `outcome` | attributes `flk.subsystem`, `flk.outcome` |
| level | `severityNumber` / `severityText` |
| `target` | instrumentation scope name |
| typed fields | attributes under `flk.` (for example `flk.request_id`, `flk.method`) |
| the message | `body`: the facade's fixed template, never interpolated with free text (§4) |

The event names and their meaning carry over unchanged. **The fields do not: this is a schema change.** Today's facade logs values that §4 forbids:
- **free-text errors:** about 105 facade functions take `err: &str` and log it verbatim (for example `client.switch.dial_failed` in `src/logging.rs`, which masks credentials but nothing else), plus 4 sites with `error = %err`;
- **raw paths:** 15 fields are `path = %path.display()`, a path anywhere on disk, including repo and worktree paths that name projects and users;
- **`ssh_opts`:** `peers` events log the joined ssh options, which can include identity-file paths, host names and `ProxyCommand` strings.

V1a (§7) rewrites them all into the typed values of §4, on today's writer, before the switch.

**Spans.** Named `flk.<area>.<op>` and opened only through facade functions:

| Span | From → to | Attributes (beyond §1's ids) |
|---|---|---|
| `flk.turn` | a structured agent's turn start → its `result` (ADR-0036 §3) | `flk.stream.seq_first`, `flk.stream.seq_last`, `flk.turn.outcome` |
| `flk.tool_call` | tool use → tool result, child of `flk.turn` | `flk.tool.name`, `flk.tool.outcome`, sizes |
| `flk.queue.deliver` | queue item accepted → delivered or dropped (ADR-0036 S5) | `flk.queue.item_id`, `flk.delivery.mode`, `flk.outcome` |
| `flk.question` | question opened → closed (ADR-0036 §2a, ADR-0034) | `flk.question.id`, `flk.question.close_reason` |
| `flk.cmd` | wire `cmd` received → `ack` sent (ADR-0036 §5) | `flk.cmd.id`, `flk.cmd.verb`, `flk.outcome`, `flk.error.code` |
| `flk.api.request` | socket API request → response | `flk.request_id`, `flk.method`, `flk.outcome` |
| `flk.mesh.hop` | envelope received → forwarded or delivered on this node | `flk.mesh.origin_node`, `flk.mesh.message_id`, `flk.mesh.hops_left` |

**Bounded-cardinality names.** `flk.method` and `flk.cmd.verb` take flk's own `Method` enum (`src/api/schema.rs`), and anything else becomes `unknown`. `flk.tool.name` is one of a fixed enum (the harness's built-in tools and flk's own MCP tools), and anything else becomes `other`. A harness or an MCP server can therefore never put a free string into a metric attribute or a span name.

**Correlation ids** are attributes on every span and log record that has them: `flk.agent.id`, `flk.session.id`, `flk.node.id`, `flk.cmd.id`, `flk.request_id`. They get Rust newtypes where they are plain strings today (session id, `cmd_id`, node id) in the slice that first emits them, so a field can't take the wrong id.

**Transcript correlation without touching the session stream.** ADR-0036's stored session lines are the harness's events, kept losslessly, and they get no trace field. A `flk.turn` span records the stream seq range it covers. The server answers "the trace for this turn" with a `telemetry/traces` filter on `(agent id, session id, seq)` (ADR-0038 §4a). A turn or tool card offers "trace" when that lookup finds a span. This changes ADR-0038 in two places, both edited when this record is accepted:
- §4a's wording "when a trace id is attached" becomes a server-side lookup;
- the §5 `telemetry/traces` row (added by #1618) gains the filters `session id` and `seq` beside trace id, agent id and time range.

**Across nodes.** Each node writes its own traces. A `flk.mesh.hop` span carries the envelope's `MessageKey`, so a reader joins hops across nodes by message key. W3C `traceparent` propagation in the mesh envelope changes the mesh protocol, whose VERSION stays 5. It is deferred to the next protocol version and filed as a follow-up.

**Metrics.** These instruments are named now and emitted by the slice that builds the thing they measure:

| Instrument | Kind | Attributes | Emitted from |
|---|---|---|---|
| `flk.queue.depth` | gauge | `flk.agent.id` | ADR-0036 S5 |
| `flk.delivery.latency` | histogram (ms) | `flk.delivery.mode`, `flk.outcome` | S5, and msg delivery (ADR-0008) |
| `flk.agent.stalls` | counter | `flk.agent.id` | ADR-0036 S4 (`stall_ms`) |
| `flk.wire.subscribers` | gauge | `flk.stream.kind` (never the full stream id) | ADR-0036 S7 |
| `flk.api.requests` | counter | `flk.method`, `flk.outcome` | the socket API |

`AgentId` embeds a host name. Telemetry files are written on the host that produced them (§2), but host names do leave that host in two ways:
- `peers logs` ships log records over SSH to the operator's own peers, as it does today;
- an external exporter, if the operator configures one (§2), sends whatever it reads.

`flk report` keeps its allowlist, which drops agent ids and host names before anything is shared.

### 2. Where the OTLP goes

- **Files, per role and signal:** `<data dir>/telemetry/<role>/{logs,metrics,traces}.jsonl`, plus rotated files. `<data dir>` is `session::data_dir()`, so a named session keeps its own.
- **Roles:** `server` (including the handoff importer, which is the new server process), `client`, `cli` (today's `flock.log`, including `remote`), `relay`, and `mcp-<pid>`. Dead-pid `mcp-*` directories are pruned as dead-pid logs are today.
- **One writer per file stays the rule,** even though vigil documents multi-process safety. The per-role split already exists, keeps rotation simple, and doesn't depend on an unverified property.
- **A total disk cap.** `[telemetry] max_total_bytes` (default 512 MiB) bounds every role's files together, `mcp-<pid>` directories included. When the total passes the cap, the oldest rotated files across all roles are deleted first. Active files are never deleted, and the per-signal limits of §3 still apply within it. The `audit` trail is outside this cap (§5).
- **Emission never blocks a hot path.** Records from the wire, delivery, queue and API paths go into a bounded in-memory channel, drained by a writer thread. When the channel is full, records are dropped and counted (`flk.telemetry.dropped`, surfaced in `flk report` and on the `node` tile). Telemetry never waits on disk inside a request. V0 checks whether vigil's writer already behaves this way; if not, flk wraps it. Audit is the one synchronous exception (§5).
- **No network export in flk.** flk writes files only. An operator who wants Loki, Tempo, Prometheus or any OTLP backend points a collector's file receiver at these files. The GUI and `flk report` never need one.
- **The GUI reads through the server.** It uses `telemetry/traces`, `telemetry/logs` and `audit` as ADR-0038 §4a and §5 define them, decoded into generated types. A client never reads the files.

### 3. Retention

- **Per-signal limits** under `[telemetry]`, each a size cap per file, a rotated-file count and an age limit. Defaults:
  - logs: 16 MiB × 4 files, 14 days;
  - traces: 32 MiB × 4, 7 days;
  - metrics: 8 MiB × 2, 30 days.
  - Today's 5 MiB × 2 logs default is raised, because spans and metrics no longer share a file with logs, and because G13's views read history. The exact numbers are owner decision 2.
- **Independent of state.** Telemetry rotation never touches ADR-0036 §4's session streams, queues or blobs, or ADR-0005/ADR-0028's durable event log. Those are state, with their own rules (S16; the event log's 32 MiB × 4). A span whose seq range points at session lines that retention has since removed still renders. The trace tile shows "transcript no longer stored", as ADR-0038 §3 does for an `agent/<id>` gap.
- **The `audit` trail is not telemetry** and is never rotated away by these limits. It has its own segment retention in §5.

### 4. Redaction: ids only

- **Never in telemetry:** message bodies, prompts, harness or tool output, comment text, artifact content, file contents, environment values, client tokens or channel tokens (#1614 item 4).
- **Allowed attribute values:** id newtypes, enums, numbers, booleans, sizes, durations, the facade's fixed templates, flk's own `ErrorCode` (`src/api/error_code.rs`), and the typed errors and paths below.
- **Errors: `TelemetryError`, replacing `err: &str`.** Every facade function that logs a failure takes a `TelemetryError`, built in one of two ways:
  - `TelemetryError::own(&e)`, for failures of flk's own I/O, OS calls, protocol and config. It carries `flk.error.class` (an enum: `io`, `spawn`, `ssh`, `protocol`, `timeout`, `config`, `auth`, `other`), `flk.error.code` (the `ErrorCode`, when there is one) and `flk.error.message`. That message is the error's text after the same masking `flk report` applies (`mask_credentials`, then `Scrubber`'s home and user masking), clamped to 512 bytes.
  - `TelemetryError::external(class, len)`, for failures whose text comes from an agent, a harness, a tool or a remote body (a harness's stderr, a tool result, an HTTP body). It carries the class and the byte length, and never the text.
  - **Diagnostic cost, owned:** `src/report/redact.rs` calls `err` the single most diagnostic field in a bug report. flk's own failures keep a masked message, so most reports lose nothing. Failures inside external output lose their text in telemetry. The full text stays where it already lives: in the session stream (ADR-0036 §4), or in the harness's own output, which the operator can open. That loss is accepted, because that text is exactly the content #1614 item 4 excludes.
- **Paths** are emitted as `flk.path.kind` (an enum: `state`, `config`, `socket`, `repo`, `worktree`, `other`) plus the path relative to its root for `state`, `config` and `socket`. Repo and worktree paths are emitted as the flk worktree or repo id where one exists, and otherwise as the kind alone.
- **`ssh_opts`** is emitted as the list of option **names** (for example `ConnectTimeout,ControlMaster`); values are dropped.
- **Enforcement:**
  - the facade signatures take those types, not `&str`;
  - the `no-raw-trace-fields` gate extends to span and metric macros;
  - a **canary test** asserts that the canary never appears in the written files. The canary is planted:
    - in every string-typed argument of every facade function, including `TelemetryError::external`'s source and any path;
    - in a fixture's message body, question prompt, tool input, tool output and comment text, which are driven through a message delivery, a question, a tool call and an artifact comment;
    - in an environment variable's value and in a client token.
- **`flk report`'s allowlist scrubber stays** as defence in depth. It is rewritten against OTLP attribute keys in the switch slice (§6).

### 5. Audit

- **What is recorded:** every verb that ADR-0014 or ADR-0035 makes operator-only. Stated as a rule: every request that would be refused with `operator_only` for a non-operator caller, plus `flk client token add|rm` and owner changes. Each record has the actor (operator, client-token name or agent id), the action, the object's id, before and after as ids or states (never content), and the outcome.
- **Signing key:** a dedicated audit key in the operator's data dir, mode 0600, not the mesh `NodeIdentity`, so that rotating one doesn't rotate the other. Key loss starts a new chain with a recorded break. It never silently continues.
- **Verification:**
  - the server verifies the chain when it serves the `audit` stream (ADR-0038 §5: `operate` scope only) and reports "verified up to segment N" or "broken link at segment M";
  - `flk audit verify` does the same offline;
  - a broken chain is surfaced, never repaired.
- **Synchronous, and fail-closed.** vigil's audit is lossless and fsynced per record, so each audited request pays one fsync. Only operator-only verbs are audited, they are rare, and none is on a hot path, so that cost is accepted.
  - If a record cannot be written (for example a full disk), the verb is refused with `audit_unavailable` before it acts. Reads are never audited and never blocked.
  - **Reserved headroom**, so a full disk can't lock the operator out of the verbs that free space: the trail keeps a pre-allocated reserve (default 1 MiB, thousands of records). When the disk is full, audited verbs write into the reserve, and `flk report` and the `node` tile warn. Only once the reserve is used up does the verb fail closed.
- **On or off, and fail-closed or fail-open:** owner decision 1.
- **Retention:** whole sealed segments only, by age (default 180 days). The chain records each removal, so a gap is visible rather than a false "verified".

### 6. Readers to migrate, in the switch release

The switch from the fmt writer to vigil happens in one release. Every reader moves with it:

| Reader | Change |
|---|---|
| `flk report` (ADR-0010) | reads `telemetry/*/logs.jsonl`, decodes OTLP `LogRecord`s, and keeps its client-side composition (it works with the server dead); the redaction allowlist is rewritten for OTLP keys |
| `peers logs` / `LogLine` / `tail_session_logs` | decode OTLP into the unchanged `LogLine` wire type, so older peers keep working over SSH |
| `log-budget` hook | `scripts/capture_log_sample.sh` flattens OTLP records to today's `{event: …}` sample shape, so `guardrails-log-budget` and `log-budgets.toml` stay unchanged |
| tests that use `flock-*.log` as fixtures | move to a shared test helper that reads the OTLP files |
| docs (README, configuration docs, CHANGELOG) | new paths, `[telemetry]` keys, and the `FLOCK_LOG` filter (kept as the filter variable) |

There is no dual-write period. Until the switch, the format is today's. After it, today's files are no longer written, and a release note says so.

### 7. Slices

Each waits for the vigil release that ships what it uses, and codes against that release's API, never the README. V1a needs no vigil and can start once this record is accepted.

**Packaging.** vigil's first crates.io release is planned for 0.4.0. Until then, V0 to V3 take vigil as a **git dependency pinned to a release tag's commit**, behind the `vigil` cargo feature. This affects the Nix build: the flake's cargo vendoring needs an output hash for each git dependency (`cargoLock.outputHashes` or the vendoring tool's equivalent), updated with every pin bump, and `cargo-deny` must allow that git source explicitly. When 0.4.0 is on crates.io, the dependency moves there and the git allowance is removed.

| # | Slice | Needs |
|---|---|---|
| V0 | #1300 step 1: spike `vigil::init` behind a `vigil` cargo feature; confirm the file layout, rotation options and per-role init; report deviations from this record | vigil 0.1.0 |
| V1a | The schema rewrite on today's writer, needing no vigil: `TelemetryError` replaces about 105 `err: &str` sites and the 4 `error = %err` sites; path kinds replace the 15 `path.display()` fields; `ssh_opts` becomes option names; the name clamps of §1; the canary test of §4; and `redact.rs`'s allowlist updated to match. Split into sub-PRs by subsystem if it passes ~400 hand-written lines. | — |
| V1b | The switch: logs through vigil, `[telemetry]` retention and the total cap, the non-blocking channel, and every §6 reader | V0, V1a |
| V2 | Spans: `flk.api.request`, `flk.cmd`, `flk.question`, `flk.mesh.hop`; id newtypes | V1b, vigil 0.2.0 |
| V3 | Spans and metrics for structured agents: `flk.turn`, `flk.tool_call`, `flk.queue.deliver`, the §1 instruments | V2, ADR-0036 S4/S5 |
| V4 | Audit: signing key, recorded verbs, reserved headroom, `flk audit verify` | V1b, vigil 0.3.0, owner decision 1 |
| G13 | The GUI views (#1619) | ADR-0036 S7, V2, and V4 for the audit tile |

### 8. Owner decisions

| # | Question | Recommendation |
|---|---|---|
| 1 | Audit on by default once V4 ships, or opt-in? And when a record can't be written: fail-closed (refuse the verb) or fail-open (run it and record a gap)? | **On by default, fail-closed with reserved headroom (§5).** Operator-only verbs are rare, and a trail that is off, or silently skips records, is worthless when it is needed. Costs: one fsync per audited verb; one more key to back up; and on a disk that stays full past the reserve, operator-only verbs are refused until space is freed by other means (reads and non-audited verbs keep working). Fail-open is the rejected alternative: it keeps verbs available on a full disk, but its "verified" status could then cover a trail with holes that only an explicit gap record reveals. |
| 2 | The retention defaults in §3 | As written. The owner may prefer longer trace retention for G13. |
| 3 | Defer cross-node `traceparent` to the next mesh protocol version | **Yes.** Joining hops by `MessageKey` covers the GUI's waterfall per node now, with no protocol change. |

## Failure modes and known limits

- **vigil slips or changes its API.** Every slice is gated on a release, and V0 reports any deviation before V1 starts. Until then, flk keeps today's writer, and nothing in this record has shipped.
- **Disk full.** For audit, see §5 (fail-closed after the reserve). For telemetry, a write failure must not fail the operation it describes. vigil's writer error is counted, and flk surfaces it once per process in `flk report` and on the `node` tile. It is never retried in a loop.
- **A process killed mid-line** leaves a torn last line. Readers skip an undecodable last line and count it. They never fail the whole file.
- **Clock skew across nodes** misorders a joined cross-node waterfall. Hops are ordered by `MessageKey` and per-node time, never by cross-node time alone.
- **Metrics with agent-id attributes** grow with the number of agents. Retention bounds the files, and the external exporter is the operator's choice (§1).

## Alternatives considered

- **Grafana stack (#1323).** The owner rejected it on 2026-10-16 (ADR-0038 §11 item 5): another service to run, and the GUI would depend on it.
- **Dual-write old and new formats during migration.** Rejected: two sinks double the redaction surface and the disk use, and readers would never be forced to move.
- **A trace id in every stored session line.** Rejected: it changes ADR-0036's lossless store for a lookup the server can answer from the span's seq range.
- **Shared files across processes, trusting vigil's multi-process claim.** Rejected for now: the per-role split costs nothing and does not depend on an unverified property.

## Consequences

- The facade in `src/logging.rs` becomes the single place that defines flk's telemetry schema, and the redaction rule is testable there.
- Bug reports lose the text of failures inside external output (§4). flk's own failures keep a masked message.
- Audited, operator-only verbs can be refused on a disk that stays full past the reserve (§5, owner decision 1).
- `flock-*.log` paths disappear in the switch release. Scripts that tail them must move to `telemetry/<role>/logs.jsonl`, and the release note must say so.
- ADR-0038 §4a's trace affordance becomes a server-side lookup. G13 depends on V2, not only on a vigil release.
