# ADR-0039: flk's logs, metrics and traces go through vig-os/vigil as per-role OTLP/JSON Lines files, carry ids and never content, and are read back only by the flk server

- Status: Proposed
- Date: 2026-10-16
- Issues: [#1614](https://github.com/gerchowl/flock/issues/1614) (this record), [#1300](https://github.com/gerchowl/flock/issues/1300) (adoption, gated on a vigil release), [#1323](https://github.com/gerchowl/flock/issues/1323) (superseded by the owner's vigil decision), [#1619](https://github.com/gerchowl/flock/issues/1619) (G13, the GUI views)
- Builds on: ADR-0038 §4a and §11 item 5 (the owner chose vigil on 2026-10-16; the GUI's telemetry views); ADR-0036 §4 and §5 (session streams, wire scopes, `cmd_id`); ADR-0010 (`flk report`); ADR-0005 and ADR-0028 (the durable event log, which stays state, not telemetry); ADR-0014 and ADR-0035 (operator-only verbs); ADR-0026 (mesh)
- Implemented: nothing. vig-os/vigil is pre-alpha on 2026-10-16: no release, and its `src/` holds only a version constant. Every implementation slice below waits for the vigil release that ships the API it uses.

## Summary

flk keeps one logging facade (`src/logging.rs`). Behind it, vigil replaces today's hand-rolled rotating JSON writer and adds spans and metrics. Each process role writes its own OTLP/JSON Lines files under the data dir, one file per signal. Telemetry carries ids, enums, numbers and fixed message templates only: never message bodies, prompts, tool output, comment text or tokens. The flk server is the only reader the GUI uses (ADR-0038 §4a). `flk report`, `peers logs`, `tail_session_logs` and the log-budget gate are migrated in the release that switches the format, not left reading a dead file. Spans correlate with the transcript through ids the session stream already has, so the stored session stream does not change. Cross-node trace propagation waits for the next mesh protocol version.

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

The event names and their meaning carry over unchanged, so the switch changes the encoding, not the schema.

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

**Correlation ids** are attributes on every span and log record that has them: `flk.agent.id`, `flk.session.id`, `flk.node.id`, `flk.cmd.id`, `flk.request_id`. They get Rust newtypes where they are plain strings today (session id, `cmd_id`, node id) in the slice that first emits them, so a field can't take the wrong id.

**Transcript correlation without touching the session stream.** ADR-0036's stored session lines are the harness's events, kept losslessly, and they get no trace field. A `flk.turn` span records the stream seq range it covers. The server answers "the trace for this turn" with a `telemetry/traces` filter on `(agent id, session id, seq)` (ADR-0038 §4a). A turn or tool card offers "trace" when that lookup finds a span. This replaces ADR-0038 §4a's wording "when a trace id is attached" with a server-side lookup. The ADR-0038 text is updated with this record's acceptance.

**Across nodes.** Each node writes its own traces. A `flk.mesh.hop` span carries the envelope's `MessageKey`, so a reader joins hops across nodes by message key. W3C `traceparent` propagation in the mesh envelope changes the mesh protocol, whose VERSION stays 5. It is deferred to the next protocol version and filed as a follow-up.

**Metrics.** These instruments are named now and emitted by the slice that builds the thing they measure:

| Instrument | Kind | Attributes | Emitted from |
|---|---|---|---|
| `flk.queue.depth` | gauge | `flk.agent.id` | ADR-0036 S5 |
| `flk.delivery.latency` | histogram (ms) | `flk.delivery.mode`, `flk.outcome` | S5, and msg delivery (ADR-0008) |
| `flk.agent.stalls` | counter | `flk.agent.id` | ADR-0036 S4 (`stall_ms`) |
| `flk.wire.subscribers` | gauge | `flk.stream.kind` (never the full stream id) | ADR-0036 S7 |
| `flk.api.requests` | counter | `flk.method`, `flk.outcome` | the socket API |

`AgentId` embeds a host name. Telemetry files stay on the host that wrote them (§2), so agent-id attributes are acceptable there. An external exporter configured by the operator (§2) is the operator's choice to move them.

### 2. Where the OTLP goes

- **Files, per role and signal:** `<data dir>/telemetry/<role>/{logs,metrics,traces}.jsonl`, plus rotated files. `<data dir>` is `session::data_dir()`, so a named session keeps its own.
- **Roles:** `server` (including the handoff importer, which is the new server process), `client`, `cli` (today's `flock.log`, including `remote`), `relay`, and `mcp-<pid>`. Dead-pid `mcp-*` directories are pruned as dead-pid logs are today.
- **One writer per file stays the rule,** even though vigil documents multi-process safety. The per-role split already exists, keeps rotation simple, and doesn't depend on an unverified property.
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
- **Allowed attribute values:** id newtypes, enums, numbers, booleans, sizes, durations, the facade's fixed templates, flk's own `ErrorCode` (`src/api/error_code.rs`), and paths under flk's own state dirs.
- **The `Display` of an external error** (a harness's stderr, an HTTP body) is not allowed. It is logged as its `ErrorCode` and a size.
- **Enforcement:**
  - the facade signatures take those types, not `&str`;
  - the `no-raw-trace-fields` gate extends to span and metric macros;
  - a test emits every facade event with a canary string in every string-typed input and asserts the canary never appears in the written files.
- **`flk report`'s allowlist scrubber stays** as defence in depth. It is rewritten against OTLP attribute keys in the switch slice (§6).

### 5. Audit

- **What is recorded:** every verb that ADR-0014 or ADR-0035 makes operator-only. Stated as a rule: every request that would be refused with `operator_only` for a non-operator caller, plus `flk client token add|rm` and owner changes. Each record has the actor (operator, client-token name or agent id), the action, the object's id, before and after as ids or states (never content), and the outcome.
- **Signing key:** a dedicated audit key in the operator's data dir, mode 0600, not the mesh `NodeIdentity`, so that rotating one doesn't rotate the other. Key loss starts a new chain with a recorded break. It never silently continues.
- **Verification:**
  - the server verifies the chain when it serves the `audit` stream (ADR-0038 §5: `operate` scope only) and reports "verified up to segment N" or "broken link at segment M";
  - `flk audit verify` does the same offline;
  - a broken chain is surfaced, never repaired.
- **On or off:** owner decision 1.
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

Each waits for the vigil release that ships what it uses, and codes against that release's API, never the README.

| # | Slice | Needs |
|---|---|---|
| V0 | #1300 step 1: spike `vigil::init` behind a `vigil` cargo feature; confirm the file layout, rotation options and per-role init; report deviations from this record | vigil 0.1.0 |
| V1 | The switch: logs through vigil, `[telemetry]` retention, every §6 reader, the §4 redaction test | V0 |
| V2 | Spans: `flk.api.request`, `flk.cmd`, `flk.question`, `flk.mesh.hop`; id newtypes | V1, vigil 0.2.0 |
| V3 | Spans and metrics for structured agents: `flk.turn`, `flk.tool_call`, `flk.queue.deliver`, the §1 instruments | V2, ADR-0036 S4/S5 |
| V4 | Audit: signing key, recorded verbs, `flk audit verify` | V1, vigil 0.3.0, owner decision 1 |
| G13 | The GUI views (#1619) | ADR-0036 S7, V2, and V4 for the audit tile |

### 8. Owner decisions

| # | Question | Recommendation |
|---|---|---|
| 1 | Audit on by default once V4 ships, or opt-in? | **On by default.** Operator-only verbs are rare, the trail is small, and a trail that is off when needed is worthless. Cost: one more key to back up. |
| 2 | The retention defaults in §3 | As written. The owner may prefer longer trace retention for G13. |
| 3 | Defer cross-node `traceparent` to the next mesh protocol version | **Yes.** Joining hops by `MessageKey` covers the GUI's waterfall per node now, with no protocol change. |

## Failure modes and known limits

- **vigil slips or changes its API.** Every slice is gated on a release, and V0 reports any deviation before V1 starts. Until then, flk keeps today's writer, and nothing in this record has shipped.
- **Disk full.** A telemetry write failure must not fail the operation it describes. vigil's writer error is counted, and flk surfaces it once per process in `flk report` and on the `node` tile. It is never retried in a loop.
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
- `flock-*.log` paths disappear in the switch release. Scripts that tail them must move to `telemetry/<role>/logs.jsonl`, and the release note must say so.
- ADR-0038 §4a's trace affordance becomes a server-side lookup. G13 depends on V2, not only on a vigil release.
