# ADR-0039: flk's logs, metrics and traces go through vig-os/vigil as per-role OTLP/JSON Lines files, carry ids and never content, and are read back only by the flk server

- Status: Proposed
- Date: 2026-10-16, amended the same day (#1648: the ratchet's exact type set and baseline; §8: the owner's decisions recorded)
- Issues: [#1614](https://github.com/gerchowl/flock/issues/1614) (this record), [#1648](https://github.com/gerchowl/flock/issues/1648) (the ratchet, gates V1a.0), [#1300](https://github.com/gerchowl/flock/issues/1300) (adoption, gated on a vigil release), [#1323](https://github.com/gerchowl/flock/issues/1323) (superseded by the owner's vigil decision), [#1619](https://github.com/gerchowl/flock/issues/1619) (G13, the GUI views)
- Builds on: ADR-0038 §4a and §11 item 5 (the owner chose vigil on 2026-10-16; the GUI's telemetry views); ADR-0036 §4 and §5 (session streams, wire scopes, `cmd_id`); ADR-0010 (`flk report`); ADR-0005 and ADR-0028 (the durable event log, which stays state, not telemetry); ADR-0014 and ADR-0035 (operator-only verbs); ADR-0026 (mesh)
- Implemented early by #1698: the first V1a.0 input types and input-log redaction on today's writer (§1). vig-os/vigil is pre-alpha on 2026-10-16: no release, and its `src/` holds only a version constant. Slices that use vigil wait for the release that ships the API they use.

## Summary

flk keeps one logging facade (`src/logging.rs`). Behind it, vigil replaces today's hand-rolled rotating JSON writer and adds spans and metrics. Each process role writes its own OTLP/JSON Lines files under the data dir, one file per signal. Telemetry carries only typed values (ids, enums, numbers, sizes, fixed message templates, path kinds and typed errors), and never free text: no message bodies, prompts, tool or harness output, comment text or tokens. That is a schema change from today's facade, where 269 of 287 emitting functions take untyped parameters (439 in all). It is done first, on today's writer (V1a.0 to V1a.6), with no vigil needed. The flk server is the only reader the GUI uses (ADR-0038 §4a). `flk report`, `peers logs`, `tail_session_logs` and the log-budget gate are migrated in the release that switches the format, not left reading a dead file. Spans correlate with the transcript through ids the session stream already has, so the stored session stream does not change. Cross-node trace propagation waits for the next mesh protocol version.

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

The event names and their meaning carry over unchanged. **The fields do not: this is a schema change.** Inventory of `src/logging.rs` on dev at 536f6e86 (unchanged since 8cae9299), produced by the ratchet's own counting rule (below):
- **296 `pub` functions outside `#[cfg(test)]` modules and items. 287 of them emit** (their body calls a `tracing` event or span macro). The 9 that don't are infrastructure: `init_file_logging`, `init_mcp_file_logging`, `help_log_paths_summary`, the readers `parse_log_lines`, `tail_session_logs` and `merge_log_records`, `timed_switch_stage`, and the formatters `bounded_bytes_debug` and `shape_key_event`.
- **269 emitting functions take at least one parameter outside the allowlist, 439 parameters under 117 distinct names.** The allowlist is the typed values of §4: integers, `bool`, `Duration`, `Option` of those, and the types V1a.0 defines (below). By type:

  | Type | Count | Becomes |
  |---|---|---|
  | `&str`, `Option<&str>`, `&[&str]` | 357 + 9 + 1 = 367 | id newtypes, closed enums, `TelemetryError`, or the byte length of content |
  | `&'static str`, `Option<&'static str>` | 31 + 1 = 32 | closed enums (`method`, `outcome`, `role`, …) |
  | `&Path`, `&std::path::Path`, `Option<&Path>`, `Option<&std::path::Path>` | 18 + 2 + 1 + 1 = 22 | path kinds |
  | `&[u8]` | 6 | the byte length only |
  | `&crossterm::event::KeyEvent`, `&crossterm::event::KeyCode` | 5 + 1 = 6 | the key's class (character, named key, function key) and its modifiers; never the character |
  | `&io::Error` | 1 | `TelemetryError` |
  | structs and others: `&SweepEntry`, `&BridgeFailureLog`, `ExecReport`, `ExecCadence`, `Option<ExitStatus>` | 5 | allowlisted types, or a struct in the V1a.0 types module whose fields are all allowlisted |
  | **Total** | **439** | |

- **Why `&'static str` counts.** A static string can't carry runtime content, but it is an open vocabulary: nothing stops a new call site from passing a new value, and readers (the GUI filters, the log-budget gate) need the closed set. `Box::leak` can also turn runtime text into one. So all 32 become closed enums, and none is exempt.
- **Raw input and keys are content.** `raw_input_*` logs up to `MAX_TRACE_BYTE_PREVIEW` bytes of what the user typed into a pane, and `terminal_key_*` logs the key code, which for a character key is the character. Both can carry passwords typed into a terminal. Debug level does not make them safe, because `[log] level` can be raised. They keep only lengths and key classes.
- **Canonical input schema (amendment, #1698).** `src/logging/types.rs` is the V1a.0 types module. #1698 implements these first types early on today's JSON writer, retaining this field shape for V1a.4 and the later OTLP switch (which prefixes attributes with `flk.`). The enums serialize in snake_case. Optional fields are omitted, never null.

  | Field | Type and applicability |
  |---|---|
  | `input_len` | `usize`, chunk byte length, always present for raw input |
  | `input_class` | `InputClass`: `key`, `mouse`, `paste`, `focus`, `resize`, `unknown` |
  | `key_class` | `KeyClass`: `character`, `named`, `function`; only for keys |
  | `named_key` | `NamedKey`: `enter`, `tab`, `back_tab`, `backspace`, `esc`, `up`, `down`, `left`, `right`, `home`, `end`, `page_up`, `page_down`, `insert`, `delete`, `null`, `caps_lock`, `scroll_lock`, `num_lock`, `print_screen`, `pause`, `menu`, `keypad_begin`, `media`, `modifier`, `other`; only for named keys |
  | `function_key` | `u8`; only for function keys |
  | `modifiers` | `KeyModifiers`, object of booleans `shift`, `control`, `alt`, `super_key`, `hyper`, `meta`, containing only true flags; present for keys (including an empty object), and for mouse events with modifiers |

  Character keys never carry their character. Paste events have only `input_len` and `input_class=paste` beyond the fixed event envelope. Pending UTF-8, incomplete input and unsupported escape diagnostics carry only `input_len` beyond their envelope. Terminal-key records use the same key metadata, with no character, raw encoding, key state or codepoint. Resize belongs to the closed schema but the raw byte parser does not produce resize events. Named media and modifier keys collapse their subtypes. The report allowlist excludes all input metadata, and also excludes legacy `raw_bytes`, `parsed`, `key`, `code`, `bytes`, `sequence` and `encoded` fields.

  Diagnostic payload sizes on self-compaction, clipboard-image staging and handoff
  events use `byte_count`. Specifically, `self_compact_continued.byte_count` is
  the UTF-8 byte length of flock-authored continuation text typed into the pane,
  not user keystrokes. Neither the continuation text nor input characters or bytes
  are logged. Structured worker shutdown uses `failure_code` for
  its typed flk error code, not a process exit status. Reports allow these
  unambiguous diagnostic names while continuing to exclude all input metadata
  and the legacy `code`/`bytes` fields, including in existing log files.

  Input-event JSON retains the default formatter's field order (timestamp
  first), expanding the sealed carrier at its original position. The lone-escape
  WARN keeps the hint that an escape after an alt chord or focus switch may
  reach the pane as plain esc, without recording its bytes.

  Stable tracing accepts scalar fields only. Its JSON event formatter expands the sealed, serialized `InputMetadata` into these top-level fields, preserving `modifiers` as an object, rather than a JSON string. The internal carrier never appears in written JSON. This adapter fails formatting on an invalid carrier instead of emitting a different schema.

  No telemetry ratchet exists in this tree yet. This change removes eight non-allowlisted emitting parameters: the raw parsed bytes and debug string become one typed value, and five other raw-input bytes/text parameters plus the terminal-key encoded bytes become lengths. The historical 439 inventory is therefore reduced by eight by this slice, independently of subsequent facade additions. The obsolete non-emitting `bounded_bytes_debug` formatter is removed. Terminal key wrappers still classify crossterm events at the facade boundary, pending the remaining V1a.4 call-site migration tracked by #1614.

- **21 raw `.display()` fields:** 14 `path =`, plus `directory` (:22), `exe` (:1720), `api_socket` and `client_socket` (:1884-1885), `socket` (:2094), `to` (:4183) and `target` (:4250).
- **1 `error = %err` field** (:28), and 105 `err: &str` parameters.
- **Path-valued string parameters:** `path` (10), `checkout_path` (4), `dest` (4), `repo_root` (2), `to` and `from`, `cwd`, `file_name`.
- **Content-bearing free text, by name.** Each can carry agent-, harness-, remote- or user-written text:
  - `message` (3), including `notification_toast_forwarded(message)` (:2125), whose text agents write (via `src/server/headless.rs` :1606 and :3004);
  - `warning` in `node_clone_detection_unavailable` (:4860), which interpolates `"{warning}"` into the message itself. It breaks the fixed-template rule (§4) as well as the parameter rule, so V1a gives it a fixed template and a closed `CloneDetectionUnavailable` reason;
  - `line` in `remote_bridge_ssh_stderr` (:1053), and `stderr_tail` (:1169);
  - `stdout`, `stderr` and `response`;
  - `args` in `process_exec_failed` and `process_spawn_failed` (:1521, :1569), and `command` and `remote_command`;
  - `url` in `pane_open_url_failed` (:2328);
  - `err` (105), `reason` (14), `detail` (4), `diagnostic` (4), `diagnostics`, `content`, `context`, `note`, `notice`, `hint`, `warning`, `refusal`, `value`, `source_description`;
  - `ssh_opts`, `branch` (5, user-named), and `host` and `proxy_jump` (raw host strings).

**The rule covers every parameter type outside the allowlist, not a list of bad types.** After V1a, no facade function takes `&str` (static or not), `String`, `&[&str]`, `&Path`, bytes, key events, `io::Error`, `Display`, `Debug` or a struct from outside the types module. Each of the 439 parameters becomes one of the typed values of §4:
- an id newtype (`workspace_id`, `pane`, `agent`, `request_id`, `peer` as the operator's configured peer name, and so on);
- a closed enum (`kind`, `status`, `prev_state`, `new_state`, `program` as known programs or `other`, and so on);
- a path kind;
- a `TelemetryError`;
- a size or count of the text it replaces (for the content-bearing fields above, which keep only their byte length).

**The ratchet test, exactly.** It parses `src/logging.rs` (and any module the facade later splits into), skipping only `#[cfg(test)]` modules and `#[cfg(test)]` items. Production functions defined after a test helper still count:
- **What it counts:** every parameter of every `pub` or `pub(crate)` function whose type, with whitespace removed, is not on the allowlist.
- **The allowlist:** `u8`–`u128`, `i8`–`i128`, `usize`, `isize`, `bool`, `Duration` (written as `Duration` or `std::time::Duration`; the test resolves either spelling to the same type), `Option<T>` of an allowed `T`, and the types defined in the V1a.0 types module (id newtypes, closed enums, path kinds, `TelemetryError`, `FlkFailure`), plus flk's `ErrorCode` and `Method`. A struct from the types module is allowed only if each of its fields is also on the allowlist. The test checks that too, so a struct cannot smuggle a `String` in.
- **Exempt functions:** the 9 non-emitting functions above, pinned by name in the test. A function on that list that starts emitting fails the test.
- **The baseline** is 439, recorded in V1a.0. Each V1a sub-PR lowers it, and a count above the recorded number fails. V1a.6 brings it to 0.
- **`FlkFailure` is checked from the start.** Its variants' fields must all be on the allowlist, and the count for them starts and stays at 0. The canary test (§4) also drives every `FlkFailure` variant.

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
- **Roles:** `server` (written only by the process holding the ownership fence; see the handoff rule below), `server-<pid>` (any other server process), `client`, `cli` (today's `flock.log`, including `remote`), `relay`, and `mcp-<pid>`. Dead-pid `mcp-*` and `server-*` directories are pruned as dead-pid logs are today.
- **One writer per file stays the rule,** even though vigil documents multi-process safety. The per-role split already exists, keeps rotation simple, and doesn't depend on an unverified property.
- **The `server` role during a handoff.** Two server processes overlap during a handoff (ADR-0036 §5a). Only the process that holds ADR-0036 §5a's data-directory ownership fence writes `server`. A server process without the fence writes its own `server-<pid>` role instead: the importer before its ownership commit, the old server after it, and a new server whose handoff failed. These roles use the `mcp-<pid>` profile and are pruned the same way once the pid is dead. The switch happens at the fenced transfer itself. The old server closes its `server` writer before it releases the fence, and the new one opens `server` only after it has committed ownership, so the file never has two writers. Records still in the old server's bounded channel when the handoff starts are flushed to `server` as part of the handoff's flush step, inside ADR-0036 §5a's 10 s handoff deadline. Anything that can't be flushed in time is dropped and counted in `flk.telemetry.dropped`, as for a full channel. Records emitted after the old server closes its `server` writer go to its `server-<pid>` role.
- **A real total bound.**
  - `[telemetry] max_total_bytes` (default 512 MiB) bounds **rotated** files across every role. When it is passed, the oldest rotated files are deleted first.
  - Active files are never deleted, but each is bounded by its per-signal cap (§3), because the writer rotates at that size. So each fixed role (`server`, `client`, `cli`, `relay`) holds at most 16 + 32 + 8 = 56 MiB of active files.
  - `mcp-<pid>` roles use a smaller profile (logs 2 MiB, traces 4 MiB, metrics 1 MiB, with one rotated file each, counted in the total), so each holds at most 7 MiB active. Their number is bounded by the live MCP processes, one per live agent session; dead-pid roles are pruned at startup and by the server's periodic sweep.
  - **Dead roles until the sweep.** A `mcp-<pid>` or `server-<pid>` role whose process has died keeps its files until the next sweep (at startup and on the server's periodic check). Until then it still counts at its 7 MiB cap.
  - The bound is therefore `max_total_bytes + 4 × 56 MiB + 7 MiB × R`, where R counts the per-pid roles (`mcp-<pid>`, `server-<pid>`) that are live or died since the last sweep. With the defaults that is 736 MiB plus 7 MiB per such role: about one per live agent session, plus at most two during a handoff. `flk report` states the bound with the current R.
  - The `audit` trail is outside it (§5).
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
- **Errors: `TelemetryError`, typed by construction.** It replaces every `err: &str` and every free-text `reason`, `detail` and `diagnostic`. It has **no string field and no constructor that reads text**: none takes `&str`, `String` or `Display`. The two constructors that take an error value (`parse` and `from_chain`, below) read only its typed parts.
  - Fields:
    - `flk.error.class`: an enum: `io`, `spawn`, `ssh`, `protocol`, `parse`, `timeout`, `config`, `auth`, `external`, `other`;
    - `flk.error.io_kind`: `std::io::ErrorKind`, when the source is an `io::Error`;
    - `flk.error.os_code`: the raw OS error number;
    - `flk.error.code`: flk's `ErrorCode`;
    - `flk.error.what`: a closed enum of flk's own failure messages, each with a fixed template and only typed parameters;
    - `flk.error.position`: line and column, for `serde_json` parse errors;
    - `flk.error.len`: the byte length of external text, when the failure is inside external output.
  - Constructors:
    - `From<&io::Error>` (kind and OS code only);
    - `From<ErrorCode>`;
    - `From<FlkFailure>` (the closed enum);
    - `parse(&serde_json::Error)` (classification, line and column only);
    - `external(class, len)`.
    - `from_chain(&anyhow::Error)`: the **downcast-only** reducer. It walks `chain()` and calls `downcast_ref` for `io::Error`, `serde_json::Error`, `FlkFailure` and `ErrorCode`, handing the first match to the constructor above. If none matches, the result is class `other`. It never calls `Display`, `Debug` or `to_string` on any link. The canary test's `anyhow` fixture, with the canary in both the context and the root, proves it.
  - **Why typed, not a masked message:** masking (`mask_credentials`, then `Scrubber`) is a denylist. It cannot be shown complete: `serde_json` echoes input values, `anyhow` chains carry remote stderr, and unlabelled flk tokens match no pattern. Whether a call site said "own" or "external" would also be a judgement at every one of 105 sites. With no text input, nothing free-form can enter by construction, and the canary test (below) proves it, including against `serde_json` and `anyhow` fixtures.
  - **Diagnostic cost, owned:** `src/report/redact.rs` calls `err` the single most diagnostic field in a bug report, and this record removes it from telemetry. What a report keeps is the event name, the class, the io kind, the OS code, the `ErrorCode`, the closed `what` and parse positions, which identify most failures of flk's own code. What it loses is the free text: remote stderr, harness output, and messages from third-party crates. That text still reaches the operator through the command's own error output at the time it happens, and through the session stream (ADR-0036 §4) for agent output. The loss is accepted, because that text is the content #1614 item 4 excludes. A follow-up after V1a measures whether `flk report` still identifies the failures in the last month's filed bugs, and grows `FlkFailure` where it does not. `redact.rs`'s `SCRUBBED_FIELDS` shrinks to `message`, which is always the fixed template.
- **Paths** are emitted as `flk.path.kind` (an enum: `state`, `config`, `socket`, `repo`, `worktree`, `other`) plus the path relative to its root for `state`, `config` and `socket`. Repo and worktree paths are emitted as the flk worktree or repo id where one exists, and otherwise as the kind alone.
- **`ssh_opts`** is emitted as the list of option **names** (for example `ConnectTimeout,ControlMaster`); values are dropped.
- **Enforcement:**
  - the facade signatures take those types, not `&str`;
  - the `no-raw-trace-fields` gate extends to span and metric macros;
  - a **canary test** asserts that the canary never appears in the written files. The canary is planted:
    - in the source of every `TelemetryError`: an `io::Error` with a custom message, a `serde_json::Error` built by parsing input that contains the canary, and an `anyhow` chain whose context and root carry the canary;
    - in every path, peer name and branch passed into a facade function, and in every content-bearing field of the inventory (§1), driven at the call sites;
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
- **Synchronous, bounded, and fail-closed.** vigil's audit is lossless and fsynced per record, so each audited request pays one fsync. Only operator-only verbs are audited; they are rare, and none is on a hot path.
  - **Intent, then outcome.** Each audited verb writes an `intent` record before it acts and an `outcome` record after. The verb acts only once its `intent` record is durable.
  - **A deadline that fails closed.** The audit write runs on a dedicated audit thread. The request waits for it at most `[telemetry] audit_write_deadline_ms` (default 2000). If the deadline passes or the write fails, the verb is refused with `audit_unavailable` and does not act. An `intent` that lands after its deadline is followed by an `outcome: abandoned` record, so the trail never shows a verb as run when it was refused. Writes queue behind a stuck write, and each fails at its own deadline. The request never hangs (ADR-0037).
  - Reads are never audited and never blocked.
- **The reserve: a balloon file, owned by flk, not vigil.**
  - **Mechanism.** vigil owns the trail file and its fsync, so the reserve can't live inside that file. Instead, flk keeps `<data dir>/audit/reserve.balloon`: 4 MiB of written zero bytes (not sparse, not a clone), fsynced, created when audit is enabled.
  - **When it is spent.** When an `intent` write fails with `ENOSPC`, or the server's free-space check finds less than 2 × the balloon size free on the trail's filesystem, flk deletes the balloon. It then checks with `statvfs` that free space actually rose, and retries the write once. The delete, the check and the retry all run on the audit thread **inside the same 2 s deadline** as the first write; none starts a new one. If the deadline passes partway through, the verb is refused with `audit_unavailable`, and a balloon already deleted counts as spent (the refill rule below applies).
  - **Filesystem caveats.** On APFS (and on btrfs or ZFS) a local snapshot, such as Time Machine's, can still reference the balloon's blocks, so deleting it frees nothing. Preallocation through `F_PREALLOCATE` or `fallocate` makes no promise either, because it doesn't protect against snapshots, clones or compression. That is why flk checks that free space rose, rather than assuming it did. If it did not, the reserve counts as spent, the verb fails closed, and the operator docs name local snapshots as the cause to check.
  - **Who may spend it.** Only verbs that free space, a closed `FreesSpace` set in code pinned by a test: `worktree.remove` and `worktree.kill` (`src/api/schema.rs`), whenever they are audited, plus ADR-0036 S16's retention prune once it exists. Once the balloon is gone, every other audited verb fails closed with `audit_unavailable`, and its error says which verbs can free space.
  - **Refill.** The server's periodic check re-creates the balloon once free space is at least 4 × its size, before any audited verb outside `FreesSpace` is admitted again. `flk report` and the `node` tile show the reserve's state: present, spent, or refill pending.
- **On or off, the mechanism and its defaults** (balloon size, deadline): owner decision 1. If vigil 0.3's audit API can't express the intent-and-outcome pair, or can't bound a write by running it on a separate thread, V4 stops and returns to the owner. It does not weaken the rule.
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

Each waits for the vigil release that ships what it uses, and codes against that release's API, never the README. V1a.0 to V1a.6 need no vigil and can start once this record is accepted.

**Packaging.** vigil's first crates.io release is planned for 0.4.0. Until then, V0 to V3 take vigil as a **git dependency pinned to a release tag's commit**, behind the `vigil` cargo feature. This affects the Nix build: the flake's cargo vendoring needs an output hash for each git dependency (`cargoLock.outputHashes` or the vendoring tool's equivalent), updated with every pin bump, and `cargo-deny` must allow that git source explicitly. When 0.4.0 is on crates.io, the dependency moves there and the git allowance is removed.

| # | Slice | Needs |
|---|---|---|
| V0 | #1300 step 1: spike `vigil::init` behind a `vigil` cargo feature; confirm the file layout, rotation options and per-role init; report deviations from this record | vigil 0.1.0 |
| V1a.0 | The types in `src/logging/types.rs`, needing no vigil: `InputClass`, `KeyClass`, `NamedKey`, `KeyModifiers` and `InputMetadata` (implemented early by #1698), then `TelemetryError` with its constructors, the path kinds, the id newtypes and enums the facade needs, the canary harness, and the ratchet test (§1), recorded at 439 | — |
| V1a.1–V1a.6 | The rewrite of all 439 parameters outside the allowlist, with their call sites, one sub-PR per subsystem: (1) api and server; (2) client and switch; (3) peers, ssh and remote; (4) pane, terminal and UI; (5) process, spawn and platform; (6) notifications, hooks, structured and MCP. Each lowers the ratchet and stays within ~400 hand-written lines. The whole is an estimated 1,200–1,800 changed lines, which is why it is split. The last one brings the ratchet to 0 and updates `redact.rs` | V1a.0 |
| V1b | The switch: logs through vigil, `[telemetry]` retention and the total bound, the non-blocking channel, and every §6 reader | V0, V1a.6 |
| V2 | Spans: `flk.api.request`, `flk.cmd`, `flk.question`, `flk.mesh.hop`; id newtypes | V1b, vigil 0.2.0 |
| V3 | Spans and metrics for structured agents: `flk.turn`, `flk.tool_call`, `flk.queue.deliver`, the §1 instruments | V2, ADR-0036 S4/S5 |
| V4 | Audit: signing key, recorded verbs, intent and outcome records, the write deadline, the balloon reserve, `flk audit verify` | V1b, vigil 0.3.0, owner decision 1 |
| G13 | The GUI views (#1619) | ADR-0036 S7, V2, and V4 for the audit tile |

### 8. Owner decisions

**Decided on 2026-10-16.** The owner confirmed all three recommendations below in typed text in the GUI architect's session. That text answered a question that listed the three recommendations: "Confirming - also ok to use opus 5.5 for implementation." The second half of the quote, about the model used for implementation, concerns something else and decides nothing here. The `traceparent` follow-up is filed as #1699.

| # | Question | Decision |
|---|---|---|
| 1 | Audit on by default once V4 ships, or opt-in? And when a record can't be written: fail-closed (refuse the verb) or fail-open (run it and record a gap)? | **On by default, fail-closed, with intent and outcome records, a 2 s write deadline, and a 4 MiB balloon reserve that only `FreesSpace` verbs may spend (§5).** Operator-only verbs are rare, and a trail that is off, or silently skips records, is worthless when it is needed. Costs: one fsync per audited verb, and up to 2 s before `audit_unavailable` on a stuck disk; one more key to back up; 4 MiB held on disk; and on a disk that stays full past the reserve, or whose freed blocks a local snapshot pins, every audited verb except worktree removal is refused until space is freed (reads and non-audited verbs keep working). Fail-open is the rejected alternative: it keeps verbs available on a full disk, but its "verified" status could then cover a trail with holes that only an explicit gap record reveals. |
| 2 | The retention defaults in §3 | As written. The owner may prefer longer trace retention for G13. |
| 3 | Defer cross-node `traceparent` to the next mesh protocol version | **Yes.** Joining hops by `MessageKey` covers the GUI's waterfall per node now, with no protocol change. |

## Failure modes and known limits

- **vigil slips or changes its API.** Every slice is gated on a release, and V0 reports any deviation before V1 starts. Until then, flk keeps today's writer. The V1a redaction work can proceed independently, beginning with #1698's input types and input-log redaction.
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
- Bug reports lose free error text (§4). They keep the class, io kind, OS code, `ErrorCode` and flk's closed failure enum, and a follow-up checks that this still identifies filed bugs.
- Audited, operator-only verbs can be refused on a disk that stays full past the reserve (§5, owner decision 1).
- `flock-*.log` paths disappear in the switch release. Scripts that tail them must move to `telemetry/<role>/logs.jsonl`, and the release note must say so.
- ADR-0038 §4a's trace affordance becomes a server-side lookup. G13 depends on V2, not only on a vigil release.
