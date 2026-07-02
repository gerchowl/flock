use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::EnvFilter;

const DEFAULT_MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
// Keep previous generations on rotation: retained=0 DELETED the log at the
// cap, and the failure being diagnosed usually lived in the tail just dropped.
const DEFAULT_RETAINED_LOG_FILES: usize = 2;

pub(crate) fn init_file_logging(file_name: &str) {
    let Ok(make_writer) = RotatingFileMakeWriter::new(
        crate::session::data_dir(),
        file_name,
        DEFAULT_MAX_LOG_BYTES,
        DEFAULT_RETAINED_LOG_FILES,
    ) else {
        return;
    };

    let filter =
        EnvFilter::try_from_env("FLOCK_LOG").unwrap_or_else(|_| EnvFilter::new("flock=info"));

    // JSON lines on disk (logging redesign PR-2): structured fields survive as
    // real fields instead of being flattened into message strings, and the
    // parse back into LogLine is serde, not a hand-rolled text parser.
    // flatten_event puts message/event/subsystem/... at the top level next to
    // timestamp/level/target — one hop for the parser, jq-friendly on disk.
    // Spans are off: nothing #[instrument]s yet, and the default timestamp
    // precision matches the old text layer, preserving merge_log_records'
    // lexicographic-sort contract across mixed-format tails.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(make_writer)
        .with_ansi(false)
        .with_target(true)
        .json()
        .flatten_event(true)
        .with_current_span(false)
        .with_span_list(false)
        .try_init();
}

pub(crate) fn help_log_paths_summary() -> String {
    let dir = crate::session::data_dir();
    format!(
        "{} (plus flock-client.log, flock-server.log)",
        dir.join("flock.log").display()
    )
}

/// The session's log files, in role order. The fixed set `peers logs` is
/// allowed to read — never an arbitrary path (#67).
const SESSION_LOG_FILES: [&str; 3] = ["flock.log", "flock-server.log", "flock-client.log"];

/// One parsed tracing record for the cross-host log view (#67), decoded from
/// the on-disk JSONL layer (or a legacy text line — see `parse_log_lines`).
/// This struct is also the `peers logs` SSH wire type: adding fields needs the
/// same `#[serde(default)]` treatment so mixed-version fleets keep parsing.
/// `source`/`host` are filled in as records are tagged with their origin file
/// and (for merged fleet output) their node.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LogLine {
    pub ts: String,
    pub level: String,
    pub target: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

/// Parse a session log back into records — dual-format (logging redesign
/// PR-2): a line starting with `{` is a JSONL record from the current json
/// layer; a line starting with a timestamp is a legacy text record (rotated
/// pre-JSONL generations, mid-upgrade fleet tails). Anything else — including
/// a truncated JSON tail — folds into the previous record's message, so a
/// partial write can't fabricate records or panic the parser. `source` tags
/// every record with the file it came from.
pub fn parse_log_lines(content: &str, source: Option<&str>) -> Vec<LogLine> {
    let mut records: Vec<LogLine> = Vec::new();
    for raw in content.lines() {
        let parsed = if raw.trim_start().starts_with('{') {
            parse_json_log_record(raw, source)
        } else {
            parse_log_record(raw, source)
        };
        match parsed {
            Some(record) => records.push(record),
            None => {
                if let Some(last) = records.last_mut() {
                    last.message.push('\n');
                    last.message.push_str(raw);
                }
                // A leading continuation with no prior record is dropped: it's a
                // partial tail we can't attribute. Rare; our emitters are flat.
            }
        }
    }
    records
}

/// Parse one JSONL record as emitted by the json fmt layer (flattened event
/// fields; timestamp/level/target/message at the top level). Tolerant of
/// unknown fields so newer emitters stay readable by older parsers.
fn parse_json_log_record(line: &str, source: Option<&str>) -> Option<LogLine> {
    #[derive(serde::Deserialize)]
    struct Wire {
        timestamp: String,
        level: String,
        target: String,
        #[serde(default)]
        message: String,
    }
    let wire: Wire = serde_json::from_str(line).ok()?;
    Some(LogLine {
        ts: wire.timestamp,
        level: wire.level,
        target: wire.target,
        message: wire.message,
        source: source.map(str::to_string),
        host: None,
    })
}

/// Parse a single full-format line, or `None` if it isn't a fresh record.
fn parse_log_record(line: &str, source: Option<&str>) -> Option<LogLine> {
    // `<ts>  <LEVEL> <target>: <message>` — timestamp first, then the level
    // (which our config left-pads), then `target: message`.
    let (ts, rest) = line.split_once(char::is_whitespace)?;
    if !looks_like_timestamp(ts) {
        return None;
    }
    let rest = rest.trim_start();
    let (level, rest) = rest.split_once(char::is_whitespace)?;
    if !is_log_level(level) {
        return None;
    }
    let rest = rest.trim_start();
    let (target, message) = rest.split_once(": ").unwrap_or((rest, ""));
    Some(LogLine {
        ts: ts.to_string(),
        level: level.to_string(),
        target: target.to_string(),
        message: message.to_string(),
        source: source.map(str::to_string),
        host: None,
    })
}

fn looks_like_timestamp(token: &str) -> bool {
    // RFC3339 UTC as emitted by the fmt layer, e.g. 2026-06-29T09:33:48.618253Z.
    token.len() >= 20
        && token.ends_with('Z')
        && token.contains('T')
        && token.starts_with(|c: char| c.is_ascii_digit())
}

fn is_log_level(token: &str) -> bool {
    matches!(token, "TRACE" | "DEBUG" | "INFO" | "WARN" | "ERROR")
}

/// Tail this node's session logs: read each of the fixed log files, parse, tag
/// by source, merge by timestamp, and return the last `lines` records. RFC3339
/// UTC timestamps sort correctly lexicographically. Missing files are treated
/// as empty (a fresh node may not have every role's log yet).
pub fn tail_session_logs(lines: usize) -> Vec<LogLine> {
    let dir = crate::session::data_dir();
    let mut records: Vec<LogLine> = Vec::new();
    for file in SESSION_LOG_FILES {
        if let Ok(content) = fs::read_to_string(dir.join(file)) {
            records.extend(parse_log_lines(&content, Some(file)));
        }
    }
    merge_log_records(records, lines)
}

/// Stable-sort records by timestamp and keep the last `lines`. Stable so two
/// events with the same timestamp keep file/arrival order. Relies on every node
/// emitting RFC3339 UTC at the SAME fixed sub-second precision (one `fmt`
/// config, fleet-wide) so the byte compare matches chronological order — if the
/// subscriber's timestamp format ever changes, revisit (`.` < `Z`, so a coarser
/// `…:01Z` would sort after a finer `…:01.000001Z`).
pub fn merge_log_records(mut records: Vec<LogLine>, lines: usize) -> Vec<LogLine> {
    records.sort_by(|a, b| a.ts.cmp(&b.ts));
    let start = records.len().saturating_sub(lines);
    records.drain(..start);
    records
}

pub(crate) fn startup(role: &'static str) {
    tracing::info!(
        event = "app.startup",
        subsystem = role,
        outcome = "started",
        pid = std::process::id(),
        "flock starting"
    );
}

pub(crate) fn shutdown(role: &'static str) {
    tracing::info!(
        event = "app.shutdown",
        subsystem = role,
        outcome = "completed",
        pid = std::process::id(),
        "flock exiting"
    );
}

pub(crate) fn api_request_started(request_id: &str, method: &'static str, changes_ui: bool) {
    let event = "api.request.start";
    let subsystem = "api";
    let outcome = "started";
    let message = "api request received";
    if changes_ui && !is_routine_api_method(method) {
        tracing::info!(
            event,
            subsystem,
            outcome,
            request_id,
            method,
            changes_ui,
            "{message}"
        );
    } else {
        tracing::debug!(
            event,
            subsystem,
            outcome,
            request_id,
            method,
            changes_ui,
            "{message}"
        );
    }
}

pub(crate) fn api_request_completed(
    request_id: &str,
    method: &'static str,
    outcome: &'static str,
    changes_ui: bool,
) {
    let event = "api.request.complete";
    let subsystem = "api";
    let message = "api request completed";
    if outcome != "ok" || (changes_ui && !is_routine_api_method(method)) {
        tracing::info!(event, subsystem, outcome, request_id, method, "{message}");
    } else {
        tracing::debug!(event, subsystem, outcome, request_id, method, "{message}");
    }
}

fn is_routine_api_method(method: &str) -> bool {
    matches!(
        method,
        "pane.get"
            | "pane.read"
            | "pane.list"
            | "workspace.list"
            | "tab.list"
            | "pane.report_agent"
            | "pane.report_agent_session"
            | "pane.report_metadata"
    )
}

pub(crate) fn api_request_failed(request_id: &str, method: &'static str, err: &str) {
    tracing::warn!(
        event = "api.request.fail",
        subsystem = "api",
        outcome = "error",
        request_id,
        method,
        err,
        "api request failed"
    );
}

pub(crate) fn api_wait_started(request_id: &str, pane_id: &str, timeout_ms: Option<u64>) {
    tracing::info!(
        event = "api.wait.start",
        subsystem = "api",
        outcome = "started",
        request_id,
        pane_id,
        timeout_ms,
        "api output wait started"
    );
}

pub(crate) fn api_wait_completed(request_id: &str, pane_id: &str, outcome: &'static str) {
    tracing::info!(
        event = "api.wait.complete",
        subsystem = "api",
        outcome,
        request_id,
        pane_id,
        "api output wait finished"
    );
}

pub(crate) fn api_wait_timed_out(request_id: &str, pane_id: &str) {
    tracing::warn!(
        event = "api.wait.timeout",
        subsystem = "api",
        outcome = "timeout",
        request_id,
        pane_id,
        "api output wait timed out"
    );
}

pub(crate) fn pane_spawn_started(
    pane_id: u32,
    rows: u16,
    cols: u16,
    scrollback_limit_bytes: usize,
) {
    tracing::info!(
        event = "pane.spawn.start",
        subsystem = "pane",
        outcome = "started",
        pane_id,
        rows,
        cols,
        scrollback_limit_bytes,
        "spawning pane terminal"
    );
}

pub(crate) fn pane_spawned(pane_id: u32, pid: u32) {
    tracing::info!(
        event = "pane.spawned",
        subsystem = "pane",
        outcome = "ok",
        pane_id,
        pid,
        "pane child spawned"
    );
}

pub(crate) fn pane_exited(pane_id: u32, status: &str) {
    tracing::info!(
        event = "pane.exit",
        subsystem = "pane",
        outcome = "completed",
        pane_id,
        status,
        "pane child exited"
    );
}

pub(crate) fn pane_exit_failed(pane_id: u32, err: &str) {
    tracing::error!(
        event = "pane.exit",
        subsystem = "pane",
        outcome = "error",
        pane_id,
        err,
        "pane child wait failed"
    );
}

pub(crate) fn workspace_created(workspace_id: &str, root_pane_id: u32) {
    tracing::info!(
        event = "workspace.create",
        subsystem = "workspace",
        outcome = "ok",
        workspace_id,
        pane_id = root_pane_id,
        "workspace created"
    );
}

pub(crate) fn workspace_focused(workspace_id: &str) {
    tracing::info!(
        event = "workspace.focus",
        subsystem = "workspace",
        outcome = "ok",
        workspace_id,
        "workspace focused"
    );
}

pub(crate) fn workspace_closed(workspace_id: &str) {
    tracing::info!(
        event = "workspace.close",
        subsystem = "workspace",
        outcome = "ok",
        workspace_id,
        "workspace closed"
    );
}

pub(crate) fn workspace_renamed(workspace_id: &str) {
    tracing::info!(
        event = "workspace.rename",
        subsystem = "workspace",
        outcome = "ok",
        workspace_id,
        "workspace renamed"
    );
}

pub(crate) fn tab_created(workspace_id: &str, tab_id: &str, root_pane_id: u32) {
    tracing::info!(
        event = "tab.create",
        subsystem = "tab",
        outcome = "ok",
        workspace_id,
        tab_id,
        pane_id = root_pane_id,
        "tab created"
    );
}

pub(crate) fn tab_focused(workspace_id: &str, tab_id: &str) {
    tracing::info!(
        event = "tab.focus",
        subsystem = "tab",
        outcome = "ok",
        workspace_id,
        tab_id,
        "tab focused"
    );
}

pub(crate) fn tab_closed(workspace_id: &str, tab_id: &str) {
    tracing::info!(
        event = "tab.close",
        subsystem = "tab",
        outcome = "ok",
        workspace_id,
        tab_id,
        "tab closed"
    );
}

pub(crate) fn tab_renamed(workspace_id: &str, tab_id: &str) {
    tracing::info!(
        event = "tab.rename",
        subsystem = "tab",
        outcome = "ok",
        workspace_id,
        tab_id,
        "tab renamed"
    );
}

pub(crate) fn session_saved(path: &Path, workspaces: usize) {
    tracing::info!(
        event = "persist.save",
        subsystem = "persist",
        outcome = "ok",
        path = %path.display(),
        workspaces,
        "session saved"
    );
}

pub(crate) fn session_save_failed(path: &Path, err: &str) {
    tracing::error!(
        event = "persist.save",
        subsystem = "persist",
        outcome = "error",
        path = %path.display(),
        err,
        "failed to save session"
    );
}

pub(crate) fn session_cleared(path: &Path) {
    tracing::info!(
        event = "persist.clear",
        subsystem = "persist",
        outcome = "ok",
        path = %path.display(),
        "session cleared"
    );
}

pub(crate) fn session_clear_failed(path: &Path, err: &str) {
    tracing::error!(
        event = "persist.clear",
        subsystem = "persist",
        outcome = "error",
        path = %path.display(),
        err,
        "failed to clear session"
    );
}

pub(crate) fn session_restored(workspaces: usize, outcome: &'static str) {
    tracing::info!(
        event = "persist.restore",
        subsystem = "persist",
        outcome,
        workspaces,
        "session restore evaluated"
    );
}

pub(crate) fn update_check_started() {
    tracing::info!(
        event = "update.check.start",
        subsystem = "update",
        outcome = "started",
        "checking for updates"
    );
}

pub(crate) fn update_check_failed(err: &str) {
    tracing::warn!(
        event = "update.check.complete",
        subsystem = "update",
        outcome = "error",
        err,
        "update check failed"
    );
}

pub(crate) fn update_available(version: &str) {
    tracing::info!(
        event = "update.available",
        subsystem = "update",
        outcome = "ok",
        version,
        "update available"
    );
}

pub(crate) fn config_write_failed(path: &Path, context: &str, err: &str) {
    tracing::warn!(
        event = "config.write",
        subsystem = "config",
        outcome = "error",
        path = %path.display(),
        context,
        err,
        "failed to write config"
    );
}

pub(crate) fn integration_action(
    action: &'static str,
    target: &'static str,
    outcome: &'static str,
) {
    tracing::info!(
        event = "integration.action",
        subsystem = "integration",
        outcome,
        action,
        target,
        "integration action finished"
    );
}

// --- remote family (logging redesign PR-1) ---------------------------------
// The failed-remote-connect story: every ssh probe, the resolved remote
// binary, and the exact bridge command must be visible at the DEFAULT level —
// they were invisible at ANY FLOCK_LOG level before this family existed.

pub(crate) fn remote_bridge_started(
    target: &str,
    ssh_config_file: Option<&Path>,
    ssh_opts: &[&str],
    remote_command: &str,
) {
    tracing::info!(
        event = "remote.bridge.started",
        subsystem = "remote",
        outcome = "started",
        target,
        ssh_config_file = ssh_config_file
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        ssh_opts = ssh_opts.join(","),
        remote_command,
        "ssh bridge starting"
    );
}

pub(crate) fn remote_bridge_exited(target: &str, code: Option<i32>) {
    let status = code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal".into());
    if code == Some(0) {
        tracing::debug!(
            event = "remote.bridge.exited",
            subsystem = "remote",
            outcome = "ok",
            target,
            status,
            "ssh bridge exited"
        );
    } else {
        tracing::warn!(
            event = "remote.bridge.exited",
            subsystem = "remote",
            outcome = "error",
            target,
            status,
            "ssh bridge exited"
        );
    }
}

pub(crate) fn remote_bridge_failed(target: &str, err: &str) {
    tracing::warn!(
        event = "remote.bridge.failed",
        subsystem = "remote",
        outcome = "error",
        target,
        err,
        "ssh bridge connection failed"
    );
}

pub(crate) fn remote_binary_resolved(
    target: &str,
    path: &str,
    version: &str,
    source: &'static str,
) {
    tracing::info!(
        event = "remote.binary_resolved",
        subsystem = "remote",
        outcome = "ok",
        target,
        path,
        version,
        source,
        "remote flock binary resolved"
    );
}

pub(crate) fn remote_probe_result(target: &str, os: &str, arch: &str) {
    tracing::info!(
        event = "remote.probe.result",
        subsystem = "remote",
        outcome = "ok",
        target,
        os,
        arch,
        "remote platform probed"
    );
}

pub(crate) fn remote_probe_failed(target: &str, stage: &'static str, err: &str) {
    tracing::warn!(
        event = "remote.probe.result",
        subsystem = "remote",
        outcome = "error",
        target,
        stage,
        err,
        "remote probe failed"
    );
}

pub(crate) fn remote_install_started(target: &str, source_description: &str, dest: &str) {
    tracing::info!(
        event = "remote.install.start",
        subsystem = "remote",
        outcome = "started",
        target,
        source_description,
        dest,
        "remote install starting"
    );
}

pub(crate) fn remote_install_completed(target: &str, dest: &str) {
    tracing::info!(
        event = "remote.install.complete",
        subsystem = "remote",
        outcome = "ok",
        target,
        dest,
        "remote install complete"
    );
}

pub(crate) fn remote_install_failed(target: &str, dest: &str, err: &str) {
    tracing::error!(
        event = "remote.install.complete",
        subsystem = "remote",
        outcome = "error",
        target,
        dest,
        err,
        "remote install failed"
    );
}

pub(crate) fn remote_install_declined(target: &str, dest: &str) {
    tracing::info!(
        event = "remote.install.complete",
        subsystem = "remote",
        outcome = "declined",
        target,
        dest,
        "remote install declined by user"
    );
}

// --- process family (logging redesign PR-3) --------------------------------
// One traced funnel: every external process flock spawns emits its command
// line here so the "what did flock actually run?" question is answerable from
// the log tail. `src/process.rs` owns the invocation sites; this facade owns
// the schema. Non-zero exit is WARN (a caller's story is broken), zero exit
// is INFO (routine but audit-worthy), spawn failure is ERROR (the command
// never ran — the loudest failure to surface).

/// A child process finished. `status` is `None` if the wrapper never got a
/// `Wait`ed status (currently unused: `output`/`status` always yield one).
/// The exit-code component drives level selection so a caller's non-zero exit
/// lands at WARN in the tail without extra ceremony at the call site.
pub(crate) fn process_exec_completed(
    subsystem: &'static str,
    program: &str,
    args: &str,
    status: Option<std::process::ExitStatus>,
    duration_ms: u64,
) {
    let event = "process.exec";
    let code = status.and_then(|s| s.code());
    let status_str = code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal".into());
    if code == Some(0) {
        tracing::info!(
            event,
            subsystem,
            outcome = "ok",
            program,
            args,
            status = status_str,
            duration_ms,
            "process exec completed"
        );
    } else {
        tracing::warn!(
            event,
            subsystem,
            outcome = "error",
            program,
            args,
            status = status_str,
            duration_ms,
            "process exec exited non-zero"
        );
    }
}

/// A child process could not be reaped (I/O error mid-wait, or the child's
/// stdio pipes broke). Rare — but the tail needs the story.
pub(crate) fn process_exec_failed(subsystem: &'static str, program: &str, args: &str, err: &str) {
    tracing::error!(
        event = "process.exec",
        subsystem,
        outcome = "error",
        program,
        args,
        err,
        "process exec failed"
    );
}

/// A child process was spawned (fire-and-follow: the caller wires up its own
/// wait/kill semantics). The child's later exit is the caller's event, not
/// this facade's.
pub(crate) fn process_spawned(subsystem: &'static str, program: &str, args: &str, pid: u32) {
    tracing::info!(
        event = "process.spawn",
        subsystem,
        outcome = "ok",
        program,
        args,
        pid,
        "process spawned"
    );
}

/// `Command::spawn` refused: the program isn't executable, isn't on PATH, or
/// the OS rejected the fork/exec. The command never ran — surface loudly.
pub(crate) fn process_spawn_failed(subsystem: &'static str, program: &str, args: &str, err: &str) {
    tracing::error!(
        event = "process.spawn",
        subsystem,
        outcome = "error",
        program,
        args,
        err,
        "process spawn failed"
    );
}

pub(crate) fn remote_ssh_keepalive_config_missing(err: &str) {
    tracing::info!(
        event = "remote.ssh_config",
        subsystem = "remote",
        outcome = "fallback",
        err,
        "could not write ssh keepalive config; using plain ssh"
    );
}

// --- client_conn family (logging redesign PR-4) ----------------------------
// Every phase of a thin-client Unix-socket connection: nonblocking setup, the
// listener accept/reject loop, the handshake read/write, and per-connection
// read/write/flush failures once the session is running. A broken connection
// is normal in fleet churn — DEBUG for benign disconnects, WARN for setup
// misses, ERROR for the accept loop giving up (the server can no longer take
// new clients).

pub(crate) fn client_conn_nonblocking_failed(err: &str) {
    tracing::warn!(
        event = "client_conn.setup",
        subsystem = "client_conn",
        outcome = "error",
        stage = "nonblocking",
        err,
        "failed to set client stream nonblocking"
    );
}

pub(crate) fn client_conn_accept_failed(err: &str) {
    tracing::error!(
        event = "client_conn.listener",
        subsystem = "client_conn",
        outcome = "error",
        mode = "accept",
        err,
        "client listener accept failed"
    );
}

pub(crate) fn client_conn_reject_failed(err: &str) {
    tracing::error!(
        event = "client_conn.listener",
        subsystem = "client_conn",
        outcome = "error",
        mode = "reject",
        err,
        "client listener reject failed"
    );
}

pub(crate) fn client_conn_refusal_send_failed(err: &str) {
    tracing::debug!(
        event = "client_conn.handshake",
        subsystem = "client_conn",
        outcome = "error",
        stage = "handoff_refusal",
        err,
        "failed to send live-handoff refusal to pending client"
    );
}

pub(crate) fn client_conn_handshake_failed(client_id: u64, err: &str) {
    tracing::debug!(
        event = "client_conn.handshake",
        subsystem = "client_conn",
        outcome = "error",
        stage = "handshake",
        client_id,
        err,
        "client handshake failed"
    );
}

pub(crate) fn client_conn_hello_read_failed(client_id: u64, err: &str) {
    tracing::debug!(
        event = "client_conn.handshake",
        subsystem = "client_conn",
        outcome = "error",
        stage = "read_hello",
        client_id,
        err,
        "failed to read client hello"
    );
}

pub(crate) fn client_conn_write_failed(err: &str) {
    tracing::debug!(
        event = "client_conn.write",
        subsystem = "client_conn",
        outcome = "error",
        stage = "write",
        err,
        "client write failed, closing writer"
    );
}

pub(crate) fn client_conn_flush_failed(err: &str) {
    tracing::debug!(
        event = "client_conn.write",
        subsystem = "client_conn",
        outcome = "error",
        stage = "flush",
        err,
        "client flush failed, closing writer"
    );
}

pub(crate) fn client_conn_read_failed(client_id: u64, err: &str) {
    tracing::debug!(
        event = "client_conn.read",
        subsystem = "client_conn",
        outcome = "error",
        client_id,
        err,
        "client read error, closing"
    );
}

// --- server family (logging redesign PR-4) ---------------------------------
// Server lifecycle events — daemon spawn, socket bind, ready-poll, and
// shutdown cleanup. The "did the server actually come up?" story must be
// answerable from the tail without decoding raw fd errors.

pub(crate) fn server_socket_check_failed(err: &str) {
    tracing::warn!(
        event = "server.socket.check",
        subsystem = "server",
        outcome = "error",
        err,
        "unexpected error checking server socket"
    );
}

pub(crate) fn server_daemon_spawning(exe: &Path) {
    tracing::info!(
        event = "server.daemon.spawn",
        subsystem = "server",
        outcome = "started",
        exe = %exe.display(),
        "spawning server daemon"
    );
}

pub(crate) fn server_socket_ready(path: &Path) {
    tracing::info!(
        event = "server.socket.ready",
        subsystem = "server",
        outcome = "ok",
        path = %path.display(),
        "server socket ready"
    );
}

pub(crate) fn server_auto_detect_starting(path: &Path) {
    tracing::info!(
        event = "server.auto_detect.start",
        subsystem = "server",
        outcome = "started",
        path = %path.display(),
        "auto-detect launch starting"
    );
}

// --- handoff family: rollback + ownership ack (logging redesign PR-4) ------
// Live handoff (#38) forks a fresh server and hands the current runtime to
// it. The story the tail must answer: WHICH import server, WHICH phase, and
// WHY did it stop? `phase` names the rollback step (exited/inspect/kill/
// reaped/reap). Ownership-ack failures leave the OLD server as owner so the
// clients don't get abandoned — WARN, not ERROR: recoverable degradation.

pub(crate) fn handoff_import_rollback_exited(pid: u32, status: &str) {
    tracing::info!(
        event = "handoff.import.rollback",
        subsystem = "handoff",
        outcome = "ok",
        phase = "exited",
        pid,
        status,
        "handoff import server exited during rollback"
    );
}

pub(crate) fn handoff_import_rollback_reaped(pid: u32, status: &str) {
    tracing::info!(
        event = "handoff.import.rollback",
        subsystem = "handoff",
        outcome = "ok",
        phase = "reaped",
        pid,
        status,
        "handoff import server reaped during rollback"
    );
}

pub(crate) fn handoff_import_rollback_step_failed(pid: u32, phase: &'static str, err: &str) {
    let message = match phase {
        "inspect" => "failed to inspect handoff import server before rollback",
        "kill" => "failed to kill handoff import server during rollback",
        "reap" => "failed to reap handoff import server during rollback",
        _ => "handoff import rollback step failed",
    };
    tracing::warn!(
        event = "handoff.import.rollback",
        subsystem = "handoff",
        outcome = "error",
        phase,
        pid,
        err,
        "{message}"
    );
}

pub(crate) fn handoff_owned_ack_setup_failed(err: &str) {
    tracing::warn!(
        event = "handoff.ownership.ack",
        subsystem = "handoff",
        outcome = "error",
        stage = "timeout_setup",
        err,
        "failed to set handoff ownership ack timeout"
    );
}

pub(crate) fn handoff_owned_ack_unexpected(response: &str) {
    tracing::warn!(
        event = "handoff.ownership.ack",
        subsystem = "handoff",
        outcome = "unexpected",
        response,
        "handoff import sent unexpected ownership ack after commit"
    );
}

pub(crate) fn handoff_owned_ack_read_failed(err: &str) {
    tracing::warn!(
        event = "handoff.ownership.ack",
        subsystem = "handoff",
        outcome = "error",
        stage = "read",
        err,
        "handoff import ownership ack was not received after commit"
    );
}

// --- server (headless) family: bind + shutdown (logging redesign PR-4) -----

pub(crate) fn server_started(api_socket: &Path, client_socket: &Path) {
    tracing::info!(
        event = "server.start",
        subsystem = "server",
        outcome = "ok",
        api_socket = %api_socket.display(),
        client_socket = %client_socket.display(),
        "flock server started"
    );
}

pub(crate) fn server_client_socket_listening(path: &Path) {
    tracing::info!(
        event = "server.socket.listening",
        subsystem = "server",
        outcome = "ok",
        path = %path.display(),
        "client protocol socket listening"
    );
}

pub(crate) fn server_client_socket_cleanup_failed(path: &Path, err: &str) {
    tracing::warn!(
        event = "server.socket.cleanup",
        subsystem = "server",
        outcome = "error",
        path = %path.display(),
        err,
        "failed to remove client socket on shutdown"
    );
}

// --- terminal_attach family (logging redesign PR-4) ------------------------
// Direct-attach client connects to a specific terminal_id and drives it as if
// it were a local pty. Per-input failures are WARN (recovered — the attach is
// still alive) unless connection-level; connect is INFO (low-frequency
// lifecycle).

pub(crate) fn terminal_attach_connected(client_id: u64, terminal_id: &str, cols: u16, rows: u16) {
    tracing::info!(
        event = "terminal_attach.connected",
        subsystem = "terminal_attach",
        outcome = "ok",
        client_id,
        terminal_id,
        cols,
        rows,
        "terminal attach client connected"
    );
}

pub(crate) fn terminal_attach_input_failed(client_id: u64, terminal_id: &str, err: &str) {
    tracing::warn!(
        event = "terminal_attach.input",
        subsystem = "terminal_attach",
        outcome = "error",
        client_id,
        terminal_id,
        err,
        "terminal attach input failed"
    );
}

pub(crate) fn terminal_attach_paste_failed(client_id: u64, terminal_id: &str, err: &str) {
    tracing::warn!(
        event = "terminal_attach.paste",
        subsystem = "terminal_attach",
        outcome = "error",
        client_id,
        terminal_id,
        err,
        "terminal attach clipboard image paste failed"
    );
}

pub(crate) fn terminal_attach_scroll_failed(client_id: u64, terminal_id: &str, err: &str) {
    tracing::warn!(
        event = "terminal_attach.scroll",
        subsystem = "terminal_attach",
        outcome = "error",
        client_id,
        terminal_id,
        err,
        "terminal attach scroll failed"
    );
}

// --- clipboard family (logging redesign PR-4) ------------------------------
// Client-forwarded clipboard images are staged to a disk file and then pasted
// as a path token. Receive is DEBUG (per-input volume); stage completion is
// INFO because the STAGED PATH is the answer to "where did the image go?".

pub(crate) fn client_clipboard_image_received(client_id: u64, len: usize, extension: &str) {
    tracing::debug!(
        event = "clipboard.image.receive",
        subsystem = "clipboard",
        outcome = "ok",
        client_id,
        len,
        extension,
        "client clipboard image received"
    );
}

pub(crate) fn client_clipboard_image_staged(client_id: u64, bytes: usize, path: &str) {
    tracing::info!(
        event = "clipboard.image.stage",
        subsystem = "clipboard",
        outcome = "ok",
        client_id,
        bytes,
        path,
        "staged client clipboard image"
    );
}

pub(crate) fn client_clipboard_image_stage_failed(client_id: u64, err: &str) {
    tracing::warn!(
        event = "clipboard.image.stage",
        subsystem = "clipboard",
        outcome = "error",
        client_id,
        err,
        "failed to stage client clipboard image"
    );
}

// --- frame_serialize family (logging redesign PR-4) ------------------------
// Serialization is meant to be infallible — a WARN here means we dropped a
// message and (for per-client) we're about to disconnect that client.
// `kind` distinguishes the frame shape so the log tail explains which pipe
// broke without hunting call sites: server_message / retained_frame /
// text_only_frame / frame / mouse_capture_mode.

pub(crate) fn frame_serialize_broadcast_failed(kind: &'static str, err: &str) {
    let message = match kind {
        "server_message" => "failed to serialize message for clients",
        "mouse_capture_mode" => "failed to serialize mouse capture mode for clients",
        _ => "failed to serialize broadcast frame for clients",
    };
    tracing::warn!(
        event = "frame.serialize",
        subsystem = "frame",
        outcome = "error",
        scope = "broadcast",
        kind,
        err,
        "{message}"
    );
}

pub(crate) fn frame_serialize_client_failed(client_id: u64, kind: &'static str, err: &str) {
    let message = match kind {
        "server_message" => "failed to serialize message for client",
        "retained_frame" => "failed to serialize retained frame for client",
        "text_only_frame" => "failed to serialize text-only frame for client",
        _ => "failed to serialize frame for client",
    };
    tracing::warn!(
        event = "frame.serialize",
        subsystem = "frame",
        outcome = "error",
        scope = "client",
        client_id,
        kind,
        err,
        "{message}"
    );
}

// --- workspace family (logging redesign PR-4) ------------------------------
// Creating a workspace at a requested cwd is a user-visible failure — the
// caller falls back to Navigate mode, but the tail needs the reason.

pub(crate) fn workspace_create_at_cwd_failed(err: &str) {
    tracing::error!(
        event = "workspace.create",
        subsystem = "workspace",
        outcome = "error",
        err,
        "failed to create workspace at requested cwd"
    );
}

// --- handoff family (rest of the family, logging redesign PR-4) ------------

pub(crate) fn handoff_import_spawned(pid: u32, socket: &Path) {
    tracing::info!(
        event = "handoff.import.spawn",
        subsystem = "handoff",
        outcome = "ok",
        pid,
        socket = %socket.display(),
        "spawned handoff import server"
    );
}

pub(crate) fn handoff_preserve_runtime(terminal_id: &str) {
    tracing::debug!(
        event = "handoff.preserve_runtime",
        subsystem = "handoff",
        outcome = "ok",
        terminal_id,
        "preserving pane runtime for handoff"
    );
}

pub(crate) fn handoff_report_ownership_failed(err: &str) {
    tracing::warn!(
        event = "handoff.ownership.report",
        subsystem = "handoff",
        outcome = "error",
        err,
        "failed to report handoff ownership; continuing as owner"
    );
}

// --- notification family (logging redesign PR-4) ---------------------------
// Toast/sound notifications the SERVER forwards to the foreground client
// (headless mode), plus the pane-state transition the toast may correlate to.
// All DEBUG — high-frequency during interactive work, useful only when
// debugging why a specific notification did / didn't fire.

pub(crate) fn notification_toast_forwarded(message: &str) {
    tracing::debug!(
        event = "notification.toast.forward",
        subsystem = "notification",
        outcome = "ok",
        msg = message,
        "forwarding toast notification from API request"
    );
}

pub(crate) fn notification_sound_forwarded(sound: &str) {
    tracing::debug!(
        event = "notification.sound.forward",
        subsystem = "notification",
        outcome = "ok",
        sound,
        "forwarding sound notification from API request"
    );
}

pub(crate) fn pane_state_change_detected(
    ws_idx: usize,
    pane_id: u32,
    prev_state: &str,
    new_state: &str,
    agent: &str,
) {
    tracing::debug!(
        event = "pane.state.change",
        subsystem = "notification",
        outcome = "detected",
        ws_idx,
        pane_id,
        prev_state,
        new_state,
        agent,
        "pane effective state changed during API request, checking notification"
    );
}

// --- render family (logging redesign PR-4) ---------------------------------

pub(crate) fn render_virtual_frame(cols: u16, rows: u16, foreground_client_id: Option<u64>) {
    tracing::debug!(
        event = "render.virtual_frame",
        subsystem = "render",
        outcome = "ok",
        cols,
        rows,
        foreground_client_id,
        "rendered virtual frame(s)"
    );
}

// --- raw_input family (logging redesign PR-5) ------------------------------
// The stdin decoder is downstream of every keystroke the host terminal
// produces, so its debug/warn tail runs hot. Byte payloads are Debug-formatted
// but BOUNDED (see `bounded_bytes_debug`) so a runaway paste can't turn the log
// into a memory hog. Levels mirror the pre-facade calls: unsupported/dropped
// buffers are DEBUG (routine framing noise), the lone-escape flush is WARN
// (may reach the pane as a spurious Esc), UTF-8-continuation waits are TRACE.

const MAX_TRACE_BYTE_PREVIEW: usize = 64;

/// Shape a byte slice for a tracing field so a runaway paste can't blow up
/// the log. Long payloads are truncated with a "(+N more)" tail; the Debug
/// form (`[0x1b, 0x5b, ...]`) survives.
pub(crate) fn bounded_bytes_debug(bytes: &[u8]) -> String {
    if bytes.len() <= MAX_TRACE_BYTE_PREVIEW {
        format!("{:?}", bytes)
    } else {
        let head = &bytes[..MAX_TRACE_BYTE_PREVIEW];
        format!(
            "{:?} (+{} more)",
            head,
            bytes.len() - MAX_TRACE_BYTE_PREVIEW
        )
    }
}

pub(crate) fn raw_input_event_parsed(chunk: &[u8], event: &str) {
    tracing::debug!(
        event = "raw_input.parsed",
        subsystem = "raw_input",
        outcome = "ok",
        raw_bytes = bounded_bytes_debug(chunk),
        parsed = event,
        "raw input event parsed"
    );
}

pub(crate) fn raw_input_flushing_lone_escape(bytes: &[u8]) {
    tracing::warn!(
        event = "raw_input.flush",
        subsystem = "raw_input",
        outcome = "lone_escape",
        bytes = bounded_bytes_debug(bytes),
        "flushing lone escape after input timeout; if this follows an alt chord or focus switch it may reach the pane as plain esc"
    );
}

pub(crate) fn raw_input_waiting_utf8_continuation(bytes: &[u8]) {
    tracing::trace!(
        event = "raw_input.wait",
        subsystem = "raw_input",
        outcome = "utf8_continuation",
        bytes = bounded_bytes_debug(bytes),
        "waiting for UTF-8 continuation bytes"
    );
}

pub(crate) fn raw_input_waiting_escaped_utf8_continuation(bytes: &[u8]) {
    tracing::trace!(
        event = "raw_input.wait",
        subsystem = "raw_input",
        outcome = "escaped_utf8_continuation",
        bytes = bounded_bytes_debug(bytes),
        "waiting for escaped UTF-8 continuation bytes"
    );
}

pub(crate) fn raw_input_dropping_incomplete_buffer(bytes: &[u8]) {
    tracing::debug!(
        event = "raw_input.drop",
        subsystem = "raw_input",
        outcome = "incomplete",
        bytes = bounded_bytes_debug(bytes),
        "dropping incomplete raw input buffer after timeout"
    );
}

pub(crate) fn raw_input_unsupported_escape(sequence: &str) {
    tracing::debug!(
        event = "raw_input.drop",
        subsystem = "raw_input",
        outcome = "unsupported_escape",
        sequence,
        "dropping unsupported escape sequence"
    );
}

// --- pane input family (logging redesign PR-5) -----------------------------
// Mouse-driven pane interactions the app can't recover from cleanly. Opening
// a URL is a WARN because the click had a visible target — the user expects
// the browser to launch.

pub(crate) fn pane_open_url_failed(url: &str, err: &str) {
    tracing::warn!(
        event = "pane.open_url",
        subsystem = "pane",
        outcome = "error",
        url,
        err,
        "failed to open pane URL"
    );
}

// --- pane_mouse family (logging redesign PR-5) -----------------------------
// Encoding + forwarding failures for mouse events routed into a pane
// runtime. `kind` is Debug-shaped at the call site (MouseEventKind is a
// crossterm enum whose payload — button / column-delta — matters); the
// facade takes it as an already-shaped string so the raw ?field stays
// confined to logging.rs. Every failure is WARN: the pane misses an event
// but keeps running.

pub(crate) fn pane_mouse_wheel_encode_failed(pane: u32, kind: &str) {
    tracing::warn!(
        event = "pane.mouse.wheel",
        subsystem = "pane_mouse",
        outcome = "encode_error",
        pane,
        kind,
        "failed to encode mouse wheel event"
    );
}

pub(crate) fn pane_mouse_wheel_forward_failed(pane: u32, err: &str) {
    tracing::warn!(
        event = "pane.mouse.wheel",
        subsystem = "pane_mouse",
        outcome = "forward_error",
        pane,
        err,
        "failed to forward mouse wheel event"
    );
}

pub(crate) fn pane_mouse_button_forward_failed(pane: u32, kind: &str, err: &str) {
    tracing::warn!(
        event = "pane.mouse.button",
        subsystem = "pane_mouse",
        outcome = "forward_error",
        pane,
        kind,
        err,
        "failed to forward mouse button event"
    );
}

pub(crate) fn pane_mouse_motion_forward_failed(pane: u32, kind: &str, err: &str) {
    tracing::warn!(
        event = "pane.mouse.motion",
        subsystem = "pane_mouse",
        outcome = "forward_error",
        pane,
        kind,
        err,
        "failed to forward mouse motion event"
    );
}

pub(crate) fn pane_mouse_alternate_scroll_forward_failed(pane: u32, err: &str) {
    tracing::warn!(
        event = "pane.mouse.alternate_scroll",
        subsystem = "pane_mouse",
        outcome = "forward_error",
        pane,
        err,
        "failed to forward alternate-scroll key"
    );
}

// --- terminal_key family (logging redesign PR-5) ---------------------------
// Terminal-mode key decoding: which key was intercepted (for a navigate
// action / custom command / pane scroll), which was dropped as
// modifier-only, and which forwards were considered ambiguous (Esc / Alt
// chords). `shape_key_event` is the single formatter for a crossterm
// KeyEvent — the code / modifiers / kind / state tuple that every other
// site would otherwise raw-Debug — and lives inside logging.rs so the gate
// stays hard everywhere else. Level discipline mirrors pre-facade calls:
// intercepts are DEBUG (routine but story-critical when a keybind misfires),
// modifier-only drops are DEBUG, empty-encoding is WARN.

/// Shape a crossterm KeyEvent for the tracing tail: the code / modifiers /
/// kind / state tuple every terminal-key site cares about. Single formatter
/// so a schema change lands in one place instead of eighteen.
pub(crate) fn shape_key_event(event: &crossterm::event::KeyEvent) -> String {
    format!(
        "code={:?} modifiers={:?} kind={:?} state={:?}",
        event.code, event.modifiers, event.kind, event.state
    )
}

pub(crate) fn terminal_key_intercept_action(event: &crossterm::event::KeyEvent, action: &str) {
    tracing::debug!(
        event = "terminal_key.intercept",
        subsystem = "terminal_key",
        outcome = "action",
        key = shape_key_event(event),
        action,
        "intercepted terminal direct keybinding before forwarding to pane"
    );
}

pub(crate) fn terminal_key_intercept_command(event: &crossterm::event::KeyEvent, command: &str) {
    tracing::debug!(
        event = "terminal_key.intercept",
        subsystem = "terminal_key",
        outcome = "command",
        key = shape_key_event(event),
        command,
        "intercepted terminal direct custom command before forwarding to pane"
    );
}

pub(crate) fn terminal_key_page_intercept(code: &crossterm::event::KeyCode, lines: usize) {
    tracing::debug!(
        event = "terminal_key.intercept",
        subsystem = "terminal_key",
        outcome = "page_scroll",
        code = format!("{:?}", code),
        lines,
        "intercepted page key for pane scrollback"
    );
}

pub(crate) fn terminal_key_modifier_only_dropped(event: &crossterm::event::KeyEvent) {
    tracing::debug!(
        event = "terminal_key.drop",
        subsystem = "terminal_key",
        outcome = "modifier_only",
        key = shape_key_event(event),
        "dropping modifier-only terminal key event instead of forwarding it to pane"
    );
}

pub(crate) fn terminal_key_forward_ambiguous(
    event: &crossterm::event::KeyEvent,
    protocol: &str,
    encoded: &[u8],
) {
    tracing::debug!(
        event = "terminal_key.forward",
        subsystem = "terminal_key",
        outcome = "ambiguous",
        key = shape_key_event(event),
        protocol,
        encoded = bounded_bytes_debug(encoded),
        "forwarding potentially-ambiguous terminal key to pane"
    );
}

pub(crate) fn terminal_key_empty_encoding(event: &crossterm::event::KeyEvent) {
    tracing::warn!(
        event = "terminal_key.encode",
        subsystem = "terminal_key",
        outcome = "empty",
        key = shape_key_event(event),
        "key produced empty encoding"
    );
}

// --- client family (logging redesign PR-5) ---------------------------------
// The thin client's connection + slot lifecycle. Setup and handshake events
// are INFO (rare, load-bearing for "did we come up?"). Runtime failures on
// the active slot (server read error, dropped-file bridge, notifications,
// config reload) are WARN — the session survives but the user notices
// something. Slot lifecycle chatter (warm/pause/stale/switch dial) is DEBUG
// — high volume during fleet churn, useful only when debugging a slot flip
// that didn't stick. `err` / `diagnostic` payloads are shape-converted to
// `&str` at the call side; Debug-shaped payloads (encoding / theme /
// diagnostics slice) are formatted at the site so the raw ?field stays
// confined to logging.rs.

pub(crate) fn client_fleet_snapshot_invalid(err: &str) {
    tracing::warn!(
        event = "client.fleet_snapshot",
        subsystem = "client",
        outcome = "invalid",
        err,
        "ignoring malformed fleet snapshot from launcher"
    );
}

pub(crate) fn client_handshake_succeeded(version: u32, encoding: &str, handshake_ms: u64) {
    tracing::info!(
        event = "client.handshake",
        subsystem = "client",
        outcome = "ok",
        version,
        encoding,
        handshake_ms,
        "handshake succeeded"
    );
}

pub(crate) fn client_host_theme_captured(theme: &str) {
    tracing::info!(
        event = "client.host_theme",
        subsystem = "client",
        outcome = "captured",
        theme,
        "captured host terminal theme for handshake"
    );
}

pub(crate) fn client_connecting(path: &Path, message: &str) {
    tracing::info!(
        event = "client.connect",
        subsystem = "client",
        outcome = "started",
        path = %path.display(),
        "{message}"
    );
}

pub(crate) fn client_render_encoding_active(encoding: &str) {
    tracing::debug!(
        event = "client.render_encoding",
        subsystem = "client",
        outcome = "active",
        encoding,
        "client render encoding active"
    );
}

pub(crate) fn client_dropped_file_read_failed(err: &str) {
    tracing::warn!(
        event = "client.dropped_file",
        subsystem = "client",
        outcome = "error",
        err,
        "failed to read dropped local file; passing the paste through"
    );
}

/// The attach subsystem's per-switch first-frame timing (#43). This one
/// keeps the flock::attach target since the /attach: log is a stable UX
/// story separate from `client.*` operational chatter.
pub(crate) fn client_attach_switch_first_paint(to: &str, warm: bool, elapsed_ms: u64) {
    tracing::debug!(
        target: "flock::attach",
        side = "client",
        stage = "switch",
        to,
        warm,
        elapsed_ms,
        "attach: switch first frame painted"
    );
}

pub(crate) fn client_slot_shutdown_demoted(slot: &str) {
    tracing::debug!(
        event = "client.slot.shutdown",
        subsystem = "client_slot",
        outcome = "demoted",
        slot,
        "warm slot server shut down; demoted silently"
    );
}

pub(crate) fn client_slot_message_dropped(slot: &str) {
    tracing::debug!(
        event = "client.slot.message",
        subsystem = "client_slot",
        outcome = "dropped",
        slot,
        "dropping message from non-active slot"
    );
}

pub(crate) fn client_slot_flip_failed(target: &str, err: &str) {
    tracing::warn!(
        event = "client.slot.flip",
        subsystem = "client_slot",
        outcome = "error",
        target,
        err,
        "slot flip failed; demoting"
    );
}

pub(crate) fn client_slot_disconnected_demoted(slot: &str) {
    tracing::debug!(
        event = "client.slot.disconnect",
        subsystem = "client_slot",
        outcome = "demoted",
        slot,
        "warm slot disconnected; demoted silently"
    );
}

pub(crate) fn client_slot_switch_pause_failed(target: &str, err: &str) {
    tracing::warn!(
        event = "client.slot.warm",
        subsystem = "client_slot",
        outcome = "pause_failed",
        target,
        err,
        "switch-dial warmed slot but pause failed"
    );
}

pub(crate) fn client_slot_prewarm_redundant(target: &str) {
    tracing::debug!(
        event = "client.slot.warm",
        subsystem = "client_slot",
        outcome = "redundant",
        target,
        "slot already connected; dropping redundant pre-warm"
    );
}

pub(crate) fn client_slot_warm_pause_failed(target: &str, err: &str) {
    tracing::debug!(
        event = "client.slot.warm",
        subsystem = "client_slot",
        outcome = "pause_failed",
        target,
        err,
        "failed to pause newly warmed slot"
    );
}

pub(crate) fn client_slot_warmed_paused(target: &str) {
    tracing::debug!(
        event = "client.slot.warm",
        subsystem = "client_slot",
        outcome = "ok",
        target,
        "slot warmed and paused"
    );
}

pub(crate) fn client_slot_warmed_stale(target: &str, gen: u64) {
    tracing::debug!(
        event = "client.slot.warm",
        subsystem = "client_slot",
        outcome = "stale",
        target,
        gen,
        "stale SlotWarmed; dropping stream"
    );
}

pub(crate) fn client_slot_dial_failed_stale(target: &str, gen: u64) {
    tracing::debug!(
        event = "client.slot.dial",
        subsystem = "client_slot",
        outcome = "stale",
        target,
        gen,
        "stale SlotDialFailed; dropping"
    );
}

pub(crate) fn client_slot_warm_all_dial_failed(target: &str, err: &str) {
    tracing::debug!(
        event = "client.slot.dial",
        subsystem = "client_slot",
        outcome = "warm_all_failed",
        target,
        err,
        "warm-all dial failed; slot stays cold"
    );
}

pub(crate) fn client_slot_switch_dial_failed(target: &str, err: &str) {
    tracing::debug!(
        event = "client.slot.dial",
        subsystem = "client_slot",
        outcome = "switch_failed",
        target,
        err,
        "switch dial failed"
    );
}

pub(crate) fn client_slot_switch_dial_timed_out(target: &str) {
    tracing::debug!(
        event = "client.slot.dial",
        subsystem = "client_slot",
        outcome = "switch_timeout",
        target,
        "switch dial timed out"
    );
}

pub(crate) fn client_server_read_error(err: &str) {
    tracing::warn!(
        event = "client.server_read",
        subsystem = "client",
        outcome = "error",
        err,
        "server read error"
    );
}

pub(crate) fn client_config_sound_diagnostic(diagnostic: &str) {
    tracing::warn!(
        event = "client.config.reload",
        subsystem = "client",
        outcome = "diagnostic",
        diagnostic,
        "local sound config diagnostic"
    );
}

pub(crate) fn client_config_reload_failed(diagnostics: &str) {
    tracing::warn!(
        event = "client.config.reload",
        subsystem = "client",
        outcome = "error",
        diagnostics,
        "failed to reload local client config; keeping current client config"
    );
}

pub(crate) fn client_terminal_notification_failed(err: &str) {
    tracing::warn!(
        event = "client.notification",
        subsystem = "client",
        outcome = "error",
        kind = "terminal",
        err,
        "failed to emit terminal notification"
    );
}

pub(crate) fn client_system_notification_failed(err: &str) {
    tracing::warn!(
        event = "client.notification",
        subsystem = "client",
        outcome = "error",
        kind = "system",
        err,
        "failed to emit system notification"
    );
}

struct RotatingFileMakeWriter {
    state: Arc<Mutex<RotatingFileState>>,
}

impl RotatingFileMakeWriter {
    fn new(
        dir: PathBuf,
        file_name: &str,
        max_bytes: u64,
        retained_files: usize,
    ) -> io::Result<Self> {
        fs::create_dir_all(&dir)?;
        let path = dir.join(file_name);
        let mut state = RotatingFileState {
            path,
            max_bytes,
            retained_files,
            file: None,
            current_size: 0,
            disabled: false,
        };
        state.open_current_file()?;
        Ok(Self {
            state: Arc::new(Mutex::new(state)),
        })
    }
}

impl<'a> MakeWriter<'a> for RotatingFileMakeWriter {
    type Writer = RotatingFileGuard;

    fn make_writer(&'a self) -> Self::Writer {
        RotatingFileGuard {
            state: Arc::clone(&self.state),
        }
    }
}

struct RotatingFileGuard {
    state: Arc<Mutex<RotatingFileState>>,
}

impl Write for RotatingFileGuard {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let Ok(mut state) = self.state.lock() else {
            return Ok(buf.len());
        };
        if state.disabled {
            return Ok(buf.len());
        }
        if state.rotate_if_needed(buf.len() as u64).is_err() {
            state.disabled = true;
            return Ok(buf.len());
        }
        if let Some(file) = state.file.as_mut() {
            match file.write(buf) {
                Ok(written) => {
                    state.current_size = state.current_size.saturating_add(written as u64);
                    Ok(written)
                }
                Err(_) => {
                    state.disabled = true;
                    Ok(buf.len())
                }
            }
        } else {
            Ok(buf.len())
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let Ok(mut state) = self.state.lock() else {
            return Ok(());
        };
        if state.disabled {
            return Ok(());
        }
        match state.file.as_mut() {
            Some(file) => match file.flush() {
                Ok(()) => Ok(()),
                Err(_) => {
                    state.disabled = true;
                    Ok(())
                }
            },
            None => Ok(()),
        }
    }
}

struct RotatingFileState {
    path: PathBuf,
    max_bytes: u64,
    retained_files: usize,
    file: Option<File>,
    current_size: u64,
    disabled: bool,
}

impl RotatingFileState {
    fn rotate_if_needed(&mut self, incoming_len: u64) -> io::Result<()> {
        if self.file.is_none() {
            self.open_current_file()?;
        }
        if self.max_bytes == 0 || self.current_size.saturating_add(incoming_len) <= self.max_bytes {
            return Ok(());
        }
        self.rotate_files()?;
        self.open_current_file()
    }

    fn open_current_file(&mut self) -> io::Result<()> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.current_size = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        self.file = Some(file);
        Ok(())
    }

    fn rotate_files(&mut self) -> io::Result<()> {
        self.file.take();
        if self.retained_files == 0 {
            match fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
            self.current_size = 0;
            return Ok(());
        }

        let oldest = rotated_log_path(&self.path, self.retained_files);
        match fs::remove_file(&oldest) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }

        for index in (1..=self.retained_files).rev() {
            let source = if index == 1 {
                self.path.clone()
            } else {
                rotated_log_path(&self.path, index - 1)
            };
            let target = rotated_log_path(&self.path, index);
            if !source.exists() {
                continue;
            }
            fs::rename(source, target)?;
        }

        self.current_size = 0;
        Ok(())
    }
}

fn rotated_log_path(path: &Path, index: usize) -> PathBuf {
    let suffix = format!(".{}", index);
    let file_name = path
        .file_name()
        .map(|name| {
            let mut name = name.to_os_string();
            name.push(&suffix);
            name
        })
        .unwrap_or_else(|| suffix.clone().into());
    path.with_file_name(file_name)
}

/// Capture everything the facade emits on THIS thread as plain fmt text.
/// Test-only, crate-wide: call-site modules assert their facade wiring with it.
#[cfg(test)]
pub(crate) fn capture_logs(f: impl FnOnce()) -> String {
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl<'a> MakeWriter<'a> for Sink {
        type Writer = Sink;
        fn make_writer(&'a self) -> Sink {
            self.clone()
        }
    }

    let sink = Sink::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(sink.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    let bytes = sink.0.lock().unwrap().clone();
    String::from_utf8(bytes).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_bridge_started_logs_full_command_shape() {
        let out = capture_logs(|| {
            remote_bridge_started(
                "host1",
                Some(Path::new("/tmp/keepalive-cfg")),
                &["-o", "BatchMode=yes", "-o", "ConnectTimeout=5"],
                "exec \"$HOME/.local/bin/flock\" remote-client-bridge",
            );
        });
        assert!(out.contains("event=\"remote.bridge.started\""), "{out}");
        assert!(out.contains("subsystem=\"remote\""), "{out}");
        assert!(out.contains("target=\"host1\""), "{out}");
        assert!(
            out.contains("ssh_config_file=\"/tmp/keepalive-cfg\""),
            "{out}"
        );
        assert!(out.contains("BatchMode=yes"), "{out}");
        assert!(
            out.contains("exec \\\"$HOME/.local/bin/flock\\\" remote-client-bridge"),
            "the full remote command must be visible at INFO: {out}"
        );
        assert!(out.contains("INFO"), "bridge start must be INFO: {out}");
    }

    #[test]
    fn remote_bridge_exited_is_warn_on_nonzero_debug_on_zero() {
        let ok = capture_logs(|| remote_bridge_exited("host1", Some(0)));
        assert!(ok.contains("DEBUG"), "clean exit is debug noise: {ok}");
        assert!(ok.contains("event=\"remote.bridge.exited\""), "{ok}");

        let bad = capture_logs(|| remote_bridge_exited("host1", Some(3)));
        assert!(bad.contains("WARN"), "failed exit must be WARN: {bad}");
        assert!(bad.contains("status=\"3\""), "{bad}");

        let signal = capture_logs(|| remote_bridge_exited("host1", None));
        assert!(signal.contains("status=\"signal\""), "{signal}");
        assert!(signal.contains("WARN"), "{signal}");
    }

    #[test]
    fn remote_binary_resolved_names_path_version_and_source() {
        let out = capture_logs(|| {
            remote_binary_resolved("host1", "/usr/local/bin/flock", "flock 0.6.8", "path");
        });
        assert!(out.contains("event=\"remote.binary_resolved\""), "{out}");
        assert!(out.contains("path=\"/usr/local/bin/flock\""), "{out}");
        assert!(out.contains("version=\"flock 0.6.8\""), "{out}");
        assert!(out.contains("source=\"path\""), "{out}");
        assert!(
            out.contains("INFO"),
            "resolution is the wrong-path story — INFO: {out}"
        );
    }

    #[test]
    fn remote_probe_and_install_events_are_info_with_outcomes() {
        let probe = capture_logs(|| remote_probe_result("host1", "linux", "x86_64"));
        assert!(probe.contains("event=\"remote.probe.result\""), "{probe}");
        assert!(probe.contains("os=\"linux\""), "{probe}");
        assert!(probe.contains("arch=\"x86_64\""), "{probe}");
        assert!(probe.contains("INFO"), "{probe}");

        let started = capture_logs(|| {
            remote_install_started("host1", "local binary", "$HOME/.local/bin/flock")
        });
        assert!(
            started.contains("event=\"remote.install.start\""),
            "{started}"
        );

        let failed = capture_logs(|| {
            remote_install_failed("host1", "$HOME/.local/bin/flock", "ssh exited with 1")
        });
        assert!(
            failed.contains("event=\"remote.install.complete\""),
            "{failed}"
        );
        assert!(failed.contains("outcome=\"error\""), "{failed}");
        assert!(failed.contains("ERROR"), "{failed}");
    }

    fn temp_log_path(name: &str) -> PathBuf {
        let unique = format!(
            "flock-logging-tests-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join(unique).join("flock.log")
    }

    #[test]
    fn rotated_log_path_appends_numeric_suffix() {
        let path = PathBuf::from("/tmp/flock.log");
        assert_eq!(
            rotated_log_path(&path, 2),
            PathBuf::from("/tmp/flock.log.2")
        );
    }

    #[test]
    fn rotate_files_shifts_existing_generations() {
        let path = temp_log_path("rotate");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "current").unwrap();
        fs::write(rotated_log_path(&path, 1), "older").unwrap();

        let mut state = RotatingFileState {
            path: path.clone(),
            max_bytes: 128,
            retained_files: 2,
            file: None,
            current_size: 0,
            disabled: false,
        };
        state.rotate_files().unwrap();

        assert_eq!(
            fs::read_to_string(rotated_log_path(&path, 1)).unwrap(),
            "current"
        );
        assert_eq!(
            fs::read_to_string(rotated_log_path(&path, 2)).unwrap(),
            "older"
        );
        assert!(!path.exists());

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn write_replaces_log_without_retained_files_when_size_limit_is_reached() {
        let path = temp_log_path("replace");
        let dir = path.parent().unwrap().to_path_buf();
        fs::create_dir_all(&dir).unwrap();

        let writer = RotatingFileMakeWriter::new(dir.clone(), "flock.log", 8, 0).unwrap();
        {
            let mut guard = writer.make_writer();
            guard.write_all(b"12345678").unwrap();
            guard.write_all(b"abc").unwrap();
            guard.flush().unwrap();
        }

        assert_eq!(fs::read_to_string(&path).unwrap(), "abc");
        assert!(!rotated_log_path(&path, 1).exists());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn parse_log_lines_reads_jsonl_records() {
        // PR-2 of the logging redesign: the on-disk format becomes JSON lines
        // (flattened fields). The parser must map timestamp/level/target/message
        // into LogLine so the `peers logs` envelope and human printer stay
        // byte-identical.
        let content = concat!(
            r#"{"timestamp":"2026-06-29T09:33:48.618253Z","level":"INFO","target":"flock::app::api","message":"api request completed","event":"api.request.complete","subsystem":"api","outcome":"ok","request_id":"7"}"#,
            "\n",
            r#"{"timestamp":"2026-06-29T09:33:49.001000Z","level":"WARN","target":"flock::peers","message":"poll failed","err":"timeout"}"#,
            "\n",
        );
        let records = parse_log_lines(content, Some("flock-server.log"));
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].ts, "2026-06-29T09:33:48.618253Z");
        assert_eq!(records[0].level, "INFO");
        assert_eq!(records[0].target, "flock::app::api");
        assert_eq!(records[0].message, "api request completed");
        assert_eq!(records[0].source.as_deref(), Some("flock-server.log"));
        assert_eq!(records[1].level, "WARN");
        assert_eq!(records[1].message, "poll failed");
    }

    #[test]
    fn parse_log_lines_handles_mixed_text_and_json_lines() {
        // A rotated pre-JSONL file or a mid-upgrade tail mixes both formats,
        // so the parser sniffs per line and neither generation is dropped.
        let content = concat!(
            "2026-06-29T09:33:48.618253Z  INFO flock::app::api: legacy text record\n",
            r#"{"timestamp":"2026-06-29T09:33:49.000000Z","level":"INFO","target":"flock::app","message":"json record"}"#,
            "\n",
        );
        let records = parse_log_lines(content, None);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].message, "legacy text record");
        assert_eq!(records[1].message, "json record");
    }

    #[test]
    fn parse_log_lines_treats_invalid_json_line_as_orphan_continuation() {
        // A truncated JSONL tail must fold into the prior record (same
        // contract as text continuations), never panic or fabricate records.
        let content = concat!(
            r#"{"timestamp":"2026-06-29T09:33:48.618253Z","level":"INFO","target":"t","message":"whole"}"#,
            "\n",
            r#"{"timestamp":"2026-06-29T09:33:49.0"#,
            "\n",
        );
        let records = parse_log_lines(content, None);
        assert_eq!(records.len(), 1);
        assert!(records[0].message.contains("whole"));
        assert!(
            records[0]
                .message
                .contains("{\"timestamp\":\"2026-06-29T09:33:49.0"),
            "truncated tail folds into the prior message: {}",
            records[0].message
        );
    }

    #[test]
    fn rotation_with_default_retention_keeps_previous_generation() {
        // retained=0 meant rotation DELETED the log — the failure being
        // diagnosed usually lived in the tail we just dropped. The default
        // must keep at least one previous generation.
        let path = temp_log_path("default-retention");
        let dir = path.parent().unwrap().to_path_buf();
        fs::create_dir_all(&dir).unwrap();

        let writer =
            RotatingFileMakeWriter::new(dir.clone(), "flock.log", 8, DEFAULT_RETAINED_LOG_FILES)
                .unwrap();
        {
            let mut guard = writer.make_writer();
            guard.write_all(b"12345678").unwrap();
            guard.write_all(b"abc").unwrap();
            guard.flush().unwrap();
        }

        assert_eq!(fs::read_to_string(&path).unwrap(), "abc");
        assert_eq!(
            fs::read_to_string(rotated_log_path(&path, 1)).unwrap(),
            "12345678",
            "the previous generation must survive rotation"
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn parse_log_lines_reads_full_format_records() {
        let content = "\
2026-06-29T09:33:48.618253Z  INFO flock::app::api: api request completed id=7
2026-06-29T09:33:49.001000Z  WARN flock::peers: poll failed err=timeout
";
        let records = parse_log_lines(content, Some("flock-server.log"));
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].ts, "2026-06-29T09:33:48.618253Z");
        assert_eq!(records[0].level, "INFO");
        assert_eq!(records[0].target, "flock::app::api");
        assert_eq!(records[0].message, "api request completed id=7");
        assert_eq!(records[0].source.as_deref(), Some("flock-server.log"));
        assert_eq!(records[1].level, "WARN");
        assert_eq!(records[1].target, "flock::peers");
    }

    #[test]
    fn parse_log_lines_folds_continuations_into_prior_message() {
        // A `\n` embedded in a field is not a fresh record; it must attach to
        // the previous one rather than be dropped or mis-parsed.
        let content = "\
2026-06-29T09:33:48.618253Z ERROR flock::pane: spawn failed
  caused by: No such file or directory
2026-06-29T09:33:49.000000Z  INFO flock::app: ok
";
        let records = parse_log_lines(content, None);
        assert_eq!(records.len(), 2);
        assert!(records[0].message.contains("spawn failed"));
        assert!(records[0]
            .message
            .contains("caused by: No such file or directory"));
        assert_eq!(records[1].target, "flock::app");
    }

    #[test]
    fn parse_log_lines_drops_leading_orphan_continuation() {
        // A partial tail whose first line is a continuation with no record to
        // attach to is dropped, not mis-parsed into a bogus record.
        let content = "  orphaned continuation with no parent\n\
2026-06-29T09:33:48.618253Z  INFO flock::app: real\n";
        let records = parse_log_lines(content, None);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].message, "real");
    }

    #[test]
    fn merge_log_records_sorts_by_ts_and_keeps_last_n() {
        let mk = |ts: &str| LogLine {
            ts: ts.into(),
            level: "INFO".into(),
            target: "t".into(),
            message: "m".into(),
            source: None,
            host: None,
        };
        // Out of order across two source files; merge orders by timestamp.
        let records = vec![
            mk("2026-06-29T00:00:03Z"),
            mk("2026-06-29T00:00:01Z"),
            mk("2026-06-29T00:00:02Z"),
        ];
        let merged = merge_log_records(records, 2);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].ts, "2026-06-29T00:00:02Z");
        assert_eq!(merged[1].ts, "2026-06-29T00:00:03Z");
    }

    // ------ logging redesign PR-4: client_conn family ----------------------

    #[test]
    fn client_conn_setup_and_accept_events_split_by_level() {
        let nb = capture_logs(|| client_conn_nonblocking_failed("EAGAIN"));
        assert!(nb.contains("event=\"client_conn.setup\""), "{nb}");
        assert!(nb.contains("stage=\"nonblocking\""), "{nb}");
        assert!(nb.contains("err=\"EAGAIN\""), "{nb}");
        assert!(nb.contains("WARN"), "{nb}");

        let acc = capture_logs(|| client_conn_accept_failed("EBADF"));
        assert!(acc.contains("event=\"client_conn.listener\""), "{acc}");
        assert!(acc.contains("mode=\"accept\""), "{acc}");
        assert!(acc.contains("ERROR"), "{acc}");

        let rej = capture_logs(|| client_conn_reject_failed("EBADF"));
        assert!(rej.contains("mode=\"reject\""), "{rej}");
        assert!(rej.contains("ERROR"), "{rej}");
    }

    #[test]
    fn client_conn_handshake_stages_carry_client_id_when_present() {
        let refusal = capture_logs(|| client_conn_refusal_send_failed("EPIPE"));
        assert!(
            refusal.contains("event=\"client_conn.handshake\""),
            "{refusal}"
        );
        assert!(refusal.contains("stage=\"handoff_refusal\""), "{refusal}");
        assert!(refusal.contains("DEBUG"), "{refusal}");

        let hs = capture_logs(|| client_conn_handshake_failed(7, "framing"));
        assert!(hs.contains("stage=\"handshake\""), "{hs}");
        assert!(hs.contains("client_id=7"), "{hs}");
        assert!(hs.contains("DEBUG"), "{hs}");

        let hello = capture_logs(|| client_conn_hello_read_failed(11, "eof"));
        assert!(hello.contains("stage=\"read_hello\""), "{hello}");
        assert!(hello.contains("client_id=11"), "{hello}");
        assert!(hello.contains("DEBUG"), "{hello}");
    }

    #[test]
    fn client_conn_write_flush_read_split_by_stage() {
        let w = capture_logs(|| client_conn_write_failed("EPIPE"));
        assert!(w.contains("event=\"client_conn.write\""), "{w}");
        assert!(w.contains("stage=\"write\""), "{w}");
        assert!(w.contains("DEBUG"), "{w}");

        let f = capture_logs(|| client_conn_flush_failed("EPIPE"));
        assert!(f.contains("event=\"client_conn.write\""), "{f}");
        assert!(f.contains("stage=\"flush\""), "{f}");

        let r = capture_logs(|| client_conn_read_failed(5, "framing"));
        assert!(r.contains("event=\"client_conn.read\""), "{r}");
        assert!(r.contains("client_id=5"), "{r}");
        assert!(r.contains("err=\"framing\""), "{r}");
        assert!(r.contains("DEBUG"), "{r}");
    }

    // ------ logging redesign PR-4: server family (autodetect side) ---------

    #[test]
    fn server_socket_check_ready_and_daemon_are_shaped() {
        let check = capture_logs(|| server_socket_check_failed("permission denied"));
        assert!(check.contains("event=\"server.socket.check\""), "{check}");
        assert!(check.contains("err=\"permission denied\""), "{check}");
        assert!(check.contains("WARN"), "{check}");

        let ready = capture_logs(|| server_socket_ready(Path::new("/tmp/x.sock")));
        assert!(ready.contains("event=\"server.socket.ready\""), "{ready}");
        assert!(ready.contains("path=/tmp/x.sock"), "{ready}");
        assert!(ready.contains("INFO"), "{ready}");

        let spawn = capture_logs(|| server_daemon_spawning(Path::new("/usr/local/bin/flock")));
        assert!(spawn.contains("event=\"server.daemon.spawn\""), "{spawn}");
        assert!(spawn.contains("exe=/usr/local/bin/flock"), "{spawn}");
        assert!(spawn.contains("INFO"), "{spawn}");

        let detect = capture_logs(|| server_auto_detect_starting(Path::new("/tmp/x.sock")));
        assert!(
            detect.contains("event=\"server.auto_detect.start\""),
            "{detect}"
        );
        assert!(detect.contains("path=/tmp/x.sock"), "{detect}");
        assert!(detect.contains("INFO"), "{detect}");
    }

    // ------ logging redesign PR-4: handoff rollback + ownership ack --------

    #[test]
    fn handoff_rollback_records_phase_and_status_or_err() {
        let exited = capture_logs(|| handoff_import_rollback_exited(9, "exit code: 0"));
        assert!(
            exited.contains("event=\"handoff.import.rollback\""),
            "{exited}"
        );
        assert!(exited.contains("phase=\"exited\""), "{exited}");
        assert!(exited.contains("status=\"exit code: 0\""), "{exited}");
        assert!(exited.contains("INFO"), "{exited}");

        let reaped = capture_logs(|| handoff_import_rollback_reaped(9, "exit code: 0"));
        assert!(reaped.contains("phase=\"reaped\""), "{reaped}");
        assert!(reaped.contains("INFO"), "{reaped}");

        let inspect = capture_logs(|| handoff_import_rollback_step_failed(9, "inspect", "ESRCH"));
        assert!(inspect.contains("phase=\"inspect\""), "{inspect}");
        assert!(inspect.contains("err=\"ESRCH\""), "{inspect}");
        assert!(inspect.contains("WARN"), "{inspect}");

        let kill = capture_logs(|| handoff_import_rollback_step_failed(9, "kill", "EPERM"));
        assert!(kill.contains("phase=\"kill\""), "{kill}");
        assert!(kill.contains("WARN"), "{kill}");

        let reap = capture_logs(|| handoff_import_rollback_step_failed(9, "reap", "ECHILD"));
        assert!(reap.contains("phase=\"reap\""), "{reap}");
        assert!(reap.contains("WARN"), "{reap}");
    }

    #[test]
    fn handoff_ownership_ack_family_stages() {
        let setup = capture_logs(|| handoff_owned_ack_setup_failed("EINVAL"));
        assert!(setup.contains("event=\"handoff.ownership.ack\""), "{setup}");
        assert!(setup.contains("stage=\"timeout_setup\""), "{setup}");
        assert!(setup.contains("WARN"), "{setup}");

        let unexpected = capture_logs(|| handoff_owned_ack_unexpected("nope"));
        assert!(
            unexpected.contains("outcome=\"unexpected\""),
            "{unexpected}"
        );
        assert!(unexpected.contains("response=\"nope\""), "{unexpected}");
        assert!(unexpected.contains("WARN"), "{unexpected}");

        let read = capture_logs(|| handoff_owned_ack_read_failed("EPIPE"));
        assert!(read.contains("stage=\"read\""), "{read}");
        assert!(read.contains("WARN"), "{read}");
    }

    // ------ logging redesign PR-4: headless-family fns ---------------------

    #[test]
    fn server_started_names_both_sockets_at_info() {
        let out = capture_logs(|| {
            server_started(
                Path::new("/tmp/flock-api.sock"),
                Path::new("/tmp/flock.sock"),
            );
        });
        assert!(out.contains("event=\"server.start\""), "{out}");
        assert!(out.contains("subsystem=\"server\""), "{out}");
        assert!(out.contains("api_socket=/tmp/flock-api.sock"), "{out}");
        assert!(out.contains("client_socket=/tmp/flock.sock"), "{out}");
        assert!(out.contains("INFO"), "{out}");
    }

    #[test]
    fn server_client_socket_events_carry_path_and_correct_level() {
        let listen = capture_logs(|| server_client_socket_listening(Path::new("/tmp/x.sock")));
        assert!(
            listen.contains("event=\"server.socket.listening\""),
            "{listen}"
        );
        assert!(listen.contains("path=/tmp/x.sock"), "{listen}");
        assert!(listen.contains("INFO"), "{listen}");

        let cleanup = capture_logs(|| {
            server_client_socket_cleanup_failed(Path::new("/tmp/x.sock"), "EACCES")
        });
        assert!(
            cleanup.contains("event=\"server.socket.cleanup\""),
            "{cleanup}"
        );
        assert!(cleanup.contains("path=/tmp/x.sock"), "{cleanup}");
        assert!(cleanup.contains("err=\"EACCES\""), "{cleanup}");
        assert!(cleanup.contains("WARN"), "{cleanup}");
    }

    #[test]
    fn terminal_attach_connected_is_info_with_size_and_ids() {
        let out = capture_logs(|| terminal_attach_connected(3, "term-1", 80, 24));
        assert!(out.contains("event=\"terminal_attach.connected\""), "{out}");
        assert!(out.contains("subsystem=\"terminal_attach\""), "{out}");
        assert!(out.contains("client_id=3"), "{out}");
        assert!(out.contains("terminal_id=\"term-1\""), "{out}");
        assert!(out.contains("cols=80"), "{out}");
        assert!(out.contains("rows=24"), "{out}");
        assert!(out.contains("INFO"), "{out}");
    }

    #[test]
    fn terminal_attach_input_paste_scroll_failures_are_warn() {
        let inp = capture_logs(|| terminal_attach_input_failed(3, "term-1", "closed"));
        assert!(inp.contains("event=\"terminal_attach.input\""), "{inp}");
        assert!(inp.contains("err=\"closed\""), "{inp}");
        assert!(inp.contains("WARN"), "{inp}");

        let paste = capture_logs(|| terminal_attach_paste_failed(3, "term-1", "boom"));
        assert!(paste.contains("event=\"terminal_attach.paste\""), "{paste}");
        assert!(paste.contains("WARN"), "{paste}");

        let scroll = capture_logs(|| terminal_attach_scroll_failed(3, "term-1", "boom"));
        assert!(
            scroll.contains("event=\"terminal_attach.scroll\""),
            "{scroll}"
        );
        assert!(scroll.contains("WARN"), "{scroll}");
    }

    #[test]
    fn clipboard_receive_stage_and_stage_failure_shapes() {
        let recv = capture_logs(|| client_clipboard_image_received(7, 4096, "png"));
        assert!(recv.contains("event=\"clipboard.image.receive\""), "{recv}");
        assert!(recv.contains("extension=\"png\""), "{recv}");
        assert!(recv.contains("len=4096"), "{recv}");
        assert!(recv.contains("DEBUG"), "{recv}");

        let staged =
            capture_logs(|| client_clipboard_image_staged(7, 4096, "/tmp/flock-clip-7.png"));
        assert!(
            staged.contains("event=\"clipboard.image.stage\""),
            "{staged}"
        );
        assert!(
            staged.contains("path=\"/tmp/flock-clip-7.png\""),
            "{staged}"
        );
        assert!(staged.contains("bytes=4096"), "{staged}");
        assert!(staged.contains("INFO"), "{staged}");

        let fail = capture_logs(|| client_clipboard_image_stage_failed(7, "ENOSPC"));
        assert!(fail.contains("event=\"clipboard.image.stage\""), "{fail}");
        assert!(fail.contains("outcome=\"error\""), "{fail}");
        assert!(fail.contains("WARN"), "{fail}");
    }

    #[test]
    fn frame_serialize_broadcast_and_client_carry_kind() {
        let b =
            capture_logs(|| frame_serialize_broadcast_failed("server_message", "encoding failed"));
        assert!(b.contains("event=\"frame.serialize\""), "{b}");
        assert!(b.contains("scope=\"broadcast\""), "{b}");
        assert!(b.contains("kind=\"server_message\""), "{b}");
        assert!(b.contains("WARN"), "{b}");

        let mouse = capture_logs(|| {
            frame_serialize_broadcast_failed("mouse_capture_mode", "encoding failed")
        });
        assert!(mouse.contains("kind=\"mouse_capture_mode\""), "{mouse}");

        let c =
            capture_logs(|| frame_serialize_client_failed(7, "retained_frame", "encoding failed"));
        assert!(c.contains("scope=\"client\""), "{c}");
        assert!(c.contains("client_id=7"), "{c}");
        assert!(c.contains("kind=\"retained_frame\""), "{c}");
        assert!(c.contains("WARN"), "{c}");
    }

    #[test]
    fn workspace_create_at_cwd_failed_is_error() {
        let out = capture_logs(|| workspace_create_at_cwd_failed("no such directory"));
        assert!(out.contains("event=\"workspace.create\""), "{out}");
        assert!(out.contains("err=\"no such directory\""), "{out}");
        assert!(out.contains("ERROR"), "{out}");
    }

    #[test]
    fn handoff_import_spawned_is_info_with_pid_and_socket() {
        let out =
            capture_logs(|| handoff_import_spawned(4711, Path::new("/tmp/flock-handoff-1.sock")));
        assert!(out.contains("event=\"handoff.import.spawn\""), "{out}");
        assert!(out.contains("pid=4711"), "{out}");
        assert!(out.contains("socket=/tmp/flock-handoff-1.sock"), "{out}");
        assert!(out.contains("INFO"), "{out}");
    }

    #[test]
    fn handoff_preserve_runtime_names_terminal_at_debug() {
        let out = capture_logs(|| handoff_preserve_runtime("term-1"));
        assert!(out.contains("event=\"handoff.preserve_runtime\""), "{out}");
        assert!(out.contains("terminal_id=\"term-1\""), "{out}");
        assert!(out.contains("DEBUG"), "{out}");
    }

    #[test]
    fn handoff_report_ownership_failed_is_warn() {
        let out = capture_logs(|| handoff_report_ownership_failed("EPIPE"));
        assert!(out.contains("event=\"handoff.ownership.report\""), "{out}");
        assert!(out.contains("WARN"), "{out}");
    }

    #[test]
    fn notification_toast_and_sound_forwarded_are_debug() {
        let toast = capture_logs(|| notification_toast_forwarded("agent finished: hello"));
        assert!(
            toast.contains("event=\"notification.toast.forward\""),
            "{toast}"
        );
        assert!(toast.contains("msg=\"agent finished: hello\""), "{toast}");
        assert!(toast.contains("DEBUG"), "{toast}");

        let sound = capture_logs(|| notification_sound_forwarded("Done"));
        assert!(
            sound.contains("event=\"notification.sound.forward\""),
            "{sound}"
        );
        assert!(sound.contains("sound=\"Done\""), "{sound}");
        assert!(sound.contains("DEBUG"), "{sound}");
    }

    #[test]
    fn pane_state_change_detected_carries_all_ids() {
        let out =
            capture_logs(|| pane_state_change_detected(2, 17, "Idle", "NeedsAttention", "claude"));
        assert!(out.contains("event=\"pane.state.change\""), "{out}");
        assert!(out.contains("ws_idx=2"), "{out}");
        assert!(out.contains("pane_id=17"), "{out}");
        assert!(out.contains("prev_state=\"Idle\""), "{out}");
        assert!(out.contains("new_state=\"NeedsAttention\""), "{out}");
        assert!(out.contains("agent=\"claude\""), "{out}");
        assert!(out.contains("DEBUG"), "{out}");
    }

    #[test]
    fn render_virtual_frame_debug_shape() {
        let with = capture_logs(|| render_virtual_frame(80, 24, Some(5)));
        assert!(with.contains("event=\"render.virtual_frame\""), "{with}");
        assert!(with.contains("cols=80"), "{with}");
        assert!(with.contains("rows=24"), "{with}");
        assert!(with.contains("foreground_client_id=5"), "{with}");
        assert!(with.contains("DEBUG"), "{with}");

        let without = capture_logs(|| render_virtual_frame(80, 24, None));
        assert!(
            without.contains("event=\"render.virtual_frame\""),
            "{without}"
        );
    }

    // ------ logging redesign PR-5: raw_input family ------------------------

    #[test]
    fn bounded_bytes_debug_truncates_long_payloads() {
        let short = bounded_bytes_debug(&[1, 2, 3]);
        assert_eq!(short, "[1, 2, 3]", "short payloads Debug as-is");
        let long: Vec<u8> = (0..100).collect();
        let shaped = bounded_bytes_debug(&long);
        assert!(shaped.starts_with('['), "still Debug-shaped: {shaped}");
        assert!(
            shaped.contains("(+36 more)"),
            "long payload truncated with count: {shaped}"
        );
        assert!(
            !shaped.contains("99"),
            "the tail is dropped so a runaway paste can't blow up the log: {shaped}"
        );
    }

    #[test]
    fn raw_input_event_parsed_is_debug_with_bounded_bytes() {
        let out = capture_logs(|| raw_input_event_parsed(&[0x1b, 0x5b, 0x41], "Key(Up, empty)"));
        assert!(out.contains("event=\"raw_input.parsed\""), "{out}");
        assert!(out.contains("subsystem=\"raw_input\""), "{out}");
        assert!(out.contains("raw_bytes=\"[27, 91, 65]\""), "{out}");
        assert!(out.contains("parsed=\"Key(Up, empty)\""), "{out}");
        assert!(out.contains("DEBUG"), "{out}");
    }

    #[test]
    fn raw_input_flushing_lone_escape_is_warn_with_bytes() {
        let out = capture_logs(|| raw_input_flushing_lone_escape(&[0x1b]));
        assert!(out.contains("event=\"raw_input.flush\""), "{out}");
        assert!(out.contains("outcome=\"lone_escape\""), "{out}");
        assert!(out.contains("bytes=\"[27]\""), "{out}");
        assert!(out.contains("WARN"), "lone-escape must WARN: {out}");
    }

    #[test]
    fn raw_input_waiting_events_are_trace() {
        let utf8 = capture_logs(|| raw_input_waiting_utf8_continuation(&[0xc3]));
        assert!(utf8.contains("event=\"raw_input.wait\""), "{utf8}");
        assert!(utf8.contains("outcome=\"utf8_continuation\""), "{utf8}");
        assert!(utf8.contains("TRACE"), "{utf8}");

        let esc = capture_logs(|| raw_input_waiting_escaped_utf8_continuation(&[0x1b, 0xc3]));
        assert!(
            esc.contains("outcome=\"escaped_utf8_continuation\""),
            "{esc}"
        );
        assert!(esc.contains("TRACE"), "{esc}");
    }

    #[test]
    fn raw_input_dropping_incomplete_buffer_is_debug() {
        let out = capture_logs(|| raw_input_dropping_incomplete_buffer(&[0xff, 0xfe]));
        assert!(out.contains("event=\"raw_input.drop\""), "{out}");
        assert!(out.contains("outcome=\"incomplete\""), "{out}");
        assert!(out.contains("bytes=\"[255, 254]\""), "{out}");
        assert!(out.contains("DEBUG"), "{out}");
    }

    #[test]
    fn raw_input_unsupported_escape_is_debug_with_sequence() {
        let out = capture_logs(|| raw_input_unsupported_escape("\x1b[?9999z"));
        assert!(out.contains("event=\"raw_input.drop\""), "{out}");
        assert!(out.contains("outcome=\"unsupported_escape\""), "{out}");
        assert!(out.contains("sequence="), "{out}");
        assert!(out.contains("DEBUG"), "{out}");
    }

    #[test]
    fn pane_open_url_failed_is_warn_with_url_and_err() {
        let out = capture_logs(|| pane_open_url_failed("https://example.test", "no browser"));
        assert!(out.contains("event=\"pane.open_url\""), "{out}");
        assert!(out.contains("url=\"https://example.test\""), "{out}");
        assert!(out.contains("err=\"no browser\""), "{out}");
        assert!(out.contains("WARN"), "{out}");
    }

    #[test]
    fn pane_mouse_wheel_encode_and_forward_failures_are_warn() {
        let enc = capture_logs(|| pane_mouse_wheel_encode_failed(7, "ScrollDown"));
        assert!(enc.contains("event=\"pane.mouse.wheel\""), "{enc}");
        assert!(enc.contains("outcome=\"encode_error\""), "{enc}");
        assert!(enc.contains("pane=7"), "{enc}");
        assert!(enc.contains("kind=\"ScrollDown\""), "{enc}");
        assert!(enc.contains("WARN"), "{enc}");

        let fwd = capture_logs(|| pane_mouse_wheel_forward_failed(7, "closed"));
        assert!(fwd.contains("event=\"pane.mouse.wheel\""), "{fwd}");
        assert!(fwd.contains("outcome=\"forward_error\""), "{fwd}");
        assert!(fwd.contains("err=\"closed\""), "{fwd}");
        assert!(fwd.contains("WARN"), "{fwd}");
    }

    #[test]
    fn pane_mouse_button_and_motion_and_alt_scroll_forward_failures() {
        let b = capture_logs(|| pane_mouse_button_forward_failed(7, "Down(Left)", "closed"));
        assert!(b.contains("event=\"pane.mouse.button\""), "{b}");
        assert!(b.contains("kind=\"Down(Left)\""), "{b}");
        assert!(b.contains("err=\"closed\""), "{b}");
        assert!(b.contains("WARN"), "{b}");

        let m = capture_logs(|| pane_mouse_motion_forward_failed(7, "Drag(Left)", "closed"));
        assert!(m.contains("event=\"pane.mouse.motion\""), "{m}");
        assert!(m.contains("kind=\"Drag(Left)\""), "{m}");
        assert!(m.contains("WARN"), "{m}");

        let a = capture_logs(|| pane_mouse_alternate_scroll_forward_failed(7, "closed"));
        assert!(a.contains("event=\"pane.mouse.alternate_scroll\""), "{a}");
        assert!(a.contains("WARN"), "{a}");
    }

    #[test]
    fn shape_key_event_covers_all_four_fields() {
        let ev = crossterm::event::KeyEvent::new_with_kind_and_state(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::CONTROL,
            crossterm::event::KeyEventKind::Press,
            crossterm::event::KeyEventState::empty(),
        );
        let shaped = shape_key_event(&ev);
        assert!(shaped.contains("code="), "{shaped}");
        assert!(shaped.contains("modifiers="), "{shaped}");
        assert!(shaped.contains("kind="), "{shaped}");
        assert!(shaped.contains("state="), "{shaped}");
    }

    #[test]
    fn terminal_key_intercept_action_and_command_are_debug() {
        let ev = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        let a = capture_logs(|| terminal_key_intercept_action(&ev, "ToggleFloat"));
        assert!(a.contains("event=\"terminal_key.intercept\""), "{a}");
        assert!(a.contains("outcome=\"action\""), "{a}");
        assert!(a.contains("action=\"ToggleFloat\""), "{a}");
        assert!(a.contains("code="), "{a}");
        assert!(a.contains("DEBUG"), "{a}");

        let c = capture_logs(|| terminal_key_intercept_command(&ev, "edit config"));
        assert!(c.contains("outcome=\"command\""), "{c}");
        assert!(c.contains("command=\"edit config\""), "{c}");
        assert!(c.contains("DEBUG"), "{c}");
    }

    #[test]
    fn terminal_key_page_intercept_debug_shape() {
        let out =
            capture_logs(|| terminal_key_page_intercept(&crossterm::event::KeyCode::PageUp, 24));
        assert!(out.contains("event=\"terminal_key.intercept\""), "{out}");
        assert!(out.contains("outcome=\"page_scroll\""), "{out}");
        assert!(out.contains("code=\"PageUp\""), "{out}");
        assert!(out.contains("lines=24"), "{out}");
        assert!(out.contains("DEBUG"), "{out}");
    }

    #[test]
    fn terminal_key_modifier_only_dropped_debug_shape() {
        let ev = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Modifier(crossterm::event::ModifierKeyCode::LeftShift),
            crossterm::event::KeyModifiers::SHIFT,
        );
        let out = capture_logs(|| terminal_key_modifier_only_dropped(&ev));
        assert!(out.contains("event=\"terminal_key.drop\""), "{out}");
        assert!(out.contains("outcome=\"modifier_only\""), "{out}");
        assert!(out.contains("DEBUG"), "{out}");
    }

    #[test]
    fn terminal_key_forward_ambiguous_debug_with_protocol_and_encoded() {
        let ev = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::empty(),
        );
        let out = capture_logs(|| terminal_key_forward_ambiguous(&ev, "Legacy", &[0x1b]));
        assert!(out.contains("event=\"terminal_key.forward\""), "{out}");
        assert!(out.contains("outcome=\"ambiguous\""), "{out}");
        assert!(out.contains("protocol=\"Legacy\""), "{out}");
        assert!(out.contains("encoded=\"[27]\""), "{out}");
        assert!(out.contains("DEBUG"), "{out}");
    }

    #[test]
    fn terminal_key_empty_encoding_is_warn() {
        let ev = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::empty(),
        );
        let out = capture_logs(|| terminal_key_empty_encoding(&ev));
        assert!(out.contains("event=\"terminal_key.encode\""), "{out}");
        assert!(out.contains("outcome=\"empty\""), "{out}");
        assert!(out.contains("WARN"), "{out}");
    }

    // ------ logging redesign PR-5: client family ---------------------------

    #[test]
    fn client_fleet_snapshot_invalid_is_warn() {
        let out = capture_logs(|| client_fleet_snapshot_invalid("bad json"));
        assert!(out.contains("event=\"client.fleet_snapshot\""), "{out}");
        assert!(out.contains("outcome=\"invalid\""), "{out}");
        assert!(out.contains("err=\"bad json\""), "{out}");
        assert!(out.contains("WARN"), "{out}");
    }

    #[test]
    fn client_handshake_succeeded_is_info_with_all_fields() {
        let out = capture_logs(|| client_handshake_succeeded(3, "SemanticFrame", 42));
        assert!(out.contains("event=\"client.handshake\""), "{out}");
        assert!(out.contains("version=3"), "{out}");
        assert!(out.contains("encoding=\"SemanticFrame\""), "{out}");
        assert!(out.contains("handshake_ms=42"), "{out}");
        assert!(out.contains("INFO"), "{out}");
    }

    #[test]
    fn client_host_theme_captured_is_info_with_shaped_theme() {
        let out = capture_logs(|| client_host_theme_captured("TerminalTheme { fg: .. }"));
        assert!(out.contains("event=\"client.host_theme\""), "{out}");
        assert!(out.contains("theme="), "{out}");
        assert!(out.contains("INFO"), "{out}");
    }

    #[test]
    fn client_connecting_is_info_with_path_and_message() {
        let out = capture_logs(|| client_connecting(Path::new("/tmp/x.sock"), "connecting"));
        assert!(out.contains("event=\"client.connect\""), "{out}");
        assert!(out.contains("path=/tmp/x.sock"), "{out}");
        assert!(out.contains("connecting"), "{out}");
        assert!(out.contains("INFO"), "{out}");
    }

    #[test]
    fn client_render_encoding_active_is_debug() {
        let out = capture_logs(|| client_render_encoding_active("SemanticFrame"));
        assert!(out.contains("event=\"client.render_encoding\""), "{out}");
        assert!(out.contains("encoding=\"SemanticFrame\""), "{out}");
        assert!(out.contains("DEBUG"), "{out}");
    }

    #[test]
    fn client_dropped_file_read_failed_is_warn() {
        let out = capture_logs(|| client_dropped_file_read_failed("ENOENT"));
        assert!(out.contains("event=\"client.dropped_file\""), "{out}");
        assert!(out.contains("err=\"ENOENT\""), "{out}");
        assert!(out.contains("WARN"), "{out}");
    }

    #[test]
    fn client_attach_switch_first_paint_keeps_flock_attach_target() {
        let out = capture_logs(|| client_attach_switch_first_paint("host1", true, 42));
        assert!(out.contains("flock::attach"), "{out}");
        assert!(out.contains("stage=\"switch\""), "{out}");
        assert!(out.contains("to=\"host1\""), "{out}");
        assert!(out.contains("warm=true"), "{out}");
        assert!(out.contains("elapsed_ms=42"), "{out}");
        assert!(out.contains("DEBUG"), "{out}");
    }

    #[test]
    fn client_slot_lifecycle_events_are_debug_with_slot_or_target() {
        let sd = capture_logs(|| client_slot_shutdown_demoted("host1"));
        assert!(sd.contains("event=\"client.slot.shutdown\""), "{sd}");
        assert!(sd.contains("outcome=\"demoted\""), "{sd}");
        assert!(sd.contains("DEBUG"), "{sd}");

        let md = capture_logs(|| client_slot_message_dropped("host1"));
        assert!(md.contains("event=\"client.slot.message\""), "{md}");
        assert!(md.contains("outcome=\"dropped\""), "{md}");
        assert!(md.contains("DEBUG"), "{md}");

        let dd = capture_logs(|| client_slot_disconnected_demoted("host1"));
        assert!(dd.contains("event=\"client.slot.disconnect\""), "{dd}");
        assert!(dd.contains("DEBUG"), "{dd}");
    }

    #[test]
    fn client_slot_warm_pause_stale_shapes() {
        let sfp = capture_logs(|| client_slot_switch_pause_failed("host1", "EAGAIN"));
        assert!(sfp.contains("event=\"client.slot.warm\""), "{sfp}");
        assert!(sfp.contains("outcome=\"pause_failed\""), "{sfp}");
        assert!(sfp.contains("WARN"), "{sfp}");

        let red = capture_logs(|| client_slot_prewarm_redundant("host1"));
        assert!(red.contains("outcome=\"redundant\""), "{red}");
        assert!(red.contains("DEBUG"), "{red}");

        let wpf = capture_logs(|| client_slot_warm_pause_failed("host1", "EAGAIN"));
        assert!(wpf.contains("outcome=\"pause_failed\""), "{wpf}");
        assert!(wpf.contains("DEBUG"), "{wpf}");

        let ok = capture_logs(|| client_slot_warmed_paused("host1"));
        assert!(ok.contains("outcome=\"ok\""), "{ok}");
        assert!(ok.contains("DEBUG"), "{ok}");

        let stale_w = capture_logs(|| client_slot_warmed_stale("host1", 3));
        assert!(stale_w.contains("event=\"client.slot.warm\""), "{stale_w}");
        assert!(stale_w.contains("outcome=\"stale\""), "{stale_w}");
        assert!(stale_w.contains("gen=3"), "{stale_w}");
        assert!(stale_w.contains("DEBUG"), "{stale_w}");

        let stale_d = capture_logs(|| client_slot_dial_failed_stale("host1", 3));
        assert!(stale_d.contains("event=\"client.slot.dial\""), "{stale_d}");
        assert!(stale_d.contains("outcome=\"stale\""), "{stale_d}");
        assert!(stale_d.contains("DEBUG"), "{stale_d}");
    }

    #[test]
    fn client_slot_dial_failure_shapes() {
        let flip = capture_logs(|| client_slot_flip_failed("host1", "EPIPE"));
        assert!(flip.contains("event=\"client.slot.flip\""), "{flip}");
        assert!(flip.contains("err=\"EPIPE\""), "{flip}");
        assert!(flip.contains("WARN"), "{flip}");

        let warm = capture_logs(|| client_slot_warm_all_dial_failed("host1", "EPIPE"));
        assert!(warm.contains("outcome=\"warm_all_failed\""), "{warm}");
        assert!(warm.contains("DEBUG"), "{warm}");

        let switch = capture_logs(|| client_slot_switch_dial_failed("host1", "EPIPE"));
        assert!(switch.contains("outcome=\"switch_failed\""), "{switch}");
        assert!(switch.contains("DEBUG"), "{switch}");

        let timeout = capture_logs(|| client_slot_switch_dial_timed_out("host1"));
        assert!(timeout.contains("outcome=\"switch_timeout\""), "{timeout}");
        assert!(timeout.contains("DEBUG"), "{timeout}");
    }

    #[test]
    fn client_server_read_and_config_and_notifications_are_warn() {
        let read = capture_logs(|| client_server_read_error("EOF"));
        assert!(read.contains("event=\"client.server_read\""), "{read}");
        assert!(read.contains("WARN"), "{read}");

        let diag = capture_logs(|| client_config_sound_diagnostic("volume missing"));
        assert!(diag.contains("event=\"client.config.reload\""), "{diag}");
        assert!(diag.contains("outcome=\"diagnostic\""), "{diag}");
        assert!(diag.contains("WARN"), "{diag}");

        let reload = capture_logs(|| client_config_reload_failed("[Diag1, Diag2]"));
        assert!(reload.contains("outcome=\"error\""), "{reload}");
        assert!(
            reload.contains("diagnostics=\"[Diag1, Diag2]\""),
            "{reload}"
        );
        assert!(reload.contains("WARN"), "{reload}");

        let term = capture_logs(|| client_terminal_notification_failed("EPIPE"));
        assert!(term.contains("event=\"client.notification\""), "{term}");
        assert!(term.contains("kind=\"terminal\""), "{term}");
        assert!(term.contains("WARN"), "{term}");

        let sys = capture_logs(|| client_system_notification_failed("EPIPE"));
        assert!(sys.contains("kind=\"system\""), "{sys}");
        assert!(sys.contains("WARN"), "{sys}");
    }
}
