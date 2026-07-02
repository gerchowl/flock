use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::EnvFilter;

const DEFAULT_MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const DEFAULT_RETAINED_LOG_FILES: usize = 0;

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

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(make_writer)
        .with_ansi(false)
        .with_target(true)
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

/// One parsed tracing line for the cross-host log view (#67). Mirrors the
/// default `tracing_subscriber::fmt` layout: `<rfc3339-utc>  <LEVEL> <target>:
/// <message>`. `source`/`host` are filled in as records are tagged with their
/// origin file and (for merged fleet output) their node.
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

/// Parse the text emitted by our `fmt` subscriber back into records. One event
/// is one line; a line that doesn't start with `<timestamp> <LEVEL> ` is treated
/// as a continuation of the previous record's message (a `\n` embedded in a
/// field), so multi-line values are preserved rather than dropped. `source`
/// tags every record with the file it came from.
pub fn parse_log_lines(content: &str, source: Option<&str>) -> Vec<LogLine> {
    let mut records: Vec<LogLine> = Vec::new();
    for raw in content.lines() {
        match parse_log_record(raw, source) {
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

pub(crate) fn remote_ssh_keepalive_config_missing(err: &str) {
    tracing::info!(
        event = "remote.ssh_config",
        subsystem = "remote",
        outcome = "fallback",
        err,
        "could not write ssh keepalive config; using plain ssh"
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
}
