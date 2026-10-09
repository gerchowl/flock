//! Capability-failure diagnosis through the CLI and an isolated socket.
#![allow(clippy::disallowed_methods)] // The harness drives the compiled binary.

mod support;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use support::environment::Command;

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("f634-{}-{nanos}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(
    args: &[&str],
    version: Option<&str>,
    error: Option<serde_json::Value>,
    ping_stalls: bool,
) -> (std::process::Output, Vec<String>) {
    let dir = TempDir::new();
    let socket = dir.path().join("api.sock");
    let brief = dir.path().join("brief.md");
    std::fs::write(&brief, "fixture task").unwrap();
    let args: Vec<_> = args
        .iter()
        .map(|arg| match *arg {
            "BRIEF" => brief.to_str().unwrap(),
            "CWD" => dir.path().to_str().unwrap(),
            other => other,
        })
        .collect();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in socket.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let registry = dir
        .path()
        .join("flock-dev/delegates")
        .join(format!("{hash:016x}"));
    std::fs::create_dir_all(&registry).unwrap();
    let entry = serde_json::json!({
        "name":"fixture", "terminal_id":"term_fixture", "pane_id":"w1:p1",
        "root_pane":"w1:p0", "workspace_id":"w1", "mode":"cwd", "worktree":null,
        "branch":null, "harness":"codex", "model":null, "round":1,
        "brief":brief, "submitted_at_ms":0, "cursor":"term_fixture:0:0:0:i", "created_at_ms":0
    });
    std::fs::write(registry.join("fixture.json"), entry.to_string()).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stopped = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&stopped);
    let version = version.map(str::to_owned);
    let server = std::thread::spawn(move || {
        let mut methods = Vec::new();
        while !stop.load(Ordering::SeqCst) {
            let (mut stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(err) => panic!("accept: {err}"),
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            let method = request["method"].as_str().unwrap().to_owned();
            methods.push(method.clone());
            if method == "ping" && ping_stalls {
                std::thread::sleep(Duration::from_millis(600));
                continue;
            }
            let response = if method == "ping" {
                serde_json::json!({"id":request["id"],"result":{"type":"pong","version":version,"protocol":if version.as_deref() == Some("current") { 27 } else { 25 }}})
            } else if let Some(error) = &error {
                serde_json::json!({"id":request["id"],"error":error})
            } else if method == "agent.get" {
                serde_json::json!({"id":request["id"],"result":{"agent":{
                    "terminal_id":"term_fixture", "pane_id":"w1:p1", "agent_status":"idle"
                }}})
            } else {
                serde_json::json!({"id":request["id"],"result":{"text":"fixture reply"}})
            };
            writeln!(stream, "{response}").unwrap();
        }
        methods
    });
    let mut command = Command::new(env!("CARGO_BIN_EXE_flk"));
    command
        .args(&args)
        .env("FLOCK_SOCKET_PATH", &socket)
        .env("XDG_STATE_HOME", dir.path())
        .env_remove("FLOCK_CLIENT_SOCKET_PATH")
        .env_remove("FLOCK_SESSION")
        .env_remove("FLOCK_ENV");
    let output = if args.first() == Some(&"mcp") {
        let mut child = command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        writeln!(
            input,
            "{}",
            serde_json::json!({
                "jsonrpc":"2.0", "id":1, "method":"tools/call",
                "params":{"name":"flock_agent_result", "arguments":{"target":"fixture"}}
            })
        )
        .unwrap();
        drop(input);
        child.wait_with_output().unwrap()
    } else {
        command.output().unwrap()
    };
    stopped.store(true, Ordering::SeqCst);
    (output, server.join().unwrap())
}

fn unknown_variant() -> serde_json::Value {
    serde_json::json!({"code":"invalid_request", "message":"unknown variant `agent.result`, expected one of `ping`, `agent.list`"})
}

#[test]
fn successful_commands_probe_once_and_keep_their_result() {
    let (output, methods) = run(
        &["agent", "result", "fixture"],
        Some("current"),
        None,
        false,
    );
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(methods, ["ping", "agent.result"]);
    assert!(output.stderr.is_empty());
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("fixture reply"));
}

fn assert_gap(output: std::process::Output, version: &str) {
    assert_eq!(output.status.code(), Some(78));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains(&format!("server is flk {version} (protocol 25); this command needs protocol 27. Hand off with 'flk server live-handoff' or restart the server")), "{stderr}");
    assert!(!stderr.contains("unknown variant"), "{stderr}");
    assert_eq!(stderr.matches("warning:").count(), 1, "{stderr}");
}

#[test]
fn delegate_start_refuses_skew_before_allocating() {
    let (output, methods) = run(
        &[
            "delegate",
            "start",
            "new-fixture",
            "--brief",
            "BRIEF",
            "--cwd",
            "CWD",
            "--harness",
            "codex",
        ],
        Some("0.10.0"),
        None,
        false,
    );
    assert_gap(output, "0.10.0");
    assert_eq!(methods, ["ping"]);
}

#[test]
fn agent_send_submit_refuses_skew_before_typing() {
    let (output, methods) = run(
        &["agent", "send", "--submit", "fixture", "hello"],
        Some("0.10.0"),
        None,
        false,
    );
    assert_gap(output, "0.10.0");
    assert_eq!(methods, ["ping"]);
}

#[test]
fn msg_send_unknown_method_has_uniform_error() {
    let (output, methods) = run(
        &["msg", "send", "fixture", "hello", "--intent", "fyi"],
        Some("0.10.0"),
        Some(serde_json::json!({"code":"invalid_request", "message":"unknown variant `msg.send`"})),
        false,
    );
    assert_gap(output, "0.10.0");
    assert_eq!(methods, ["ping", "msg.send"]);
}

#[test]
fn capability_failures_are_uniform_across_versions_and_error_codes() {
    for version in ["0.6.8-fork.9ebb536", "0.9.0-preview.abc", "0.10.0+build"] {
        for error in [
            unknown_variant(),
            serde_json::json!({"code":"unknown_method", "message":"unsupported"}),
            serde_json::json!({"code":-32601, "message":"unsupported"}),
        ] {
            let (output, methods) = run(
                &["agent", "result", "fixture"],
                Some(version),
                Some(error),
                false,
            );
            assert_gap(output, version);
            assert_eq!(methods, ["ping", "agent.result"]);
        }
    }
}

#[test]
fn unavailable_identity_preserves_original_error() {
    for stalls in [false, true] {
        let (output, methods) = run(
            &["agent", "result", "fixture"],
            None,
            Some(unknown_variant()),
            stalls,
        );
        assert_eq!(output.status.code(), Some(1));
        assert!(String::from_utf8(output.stderr)
            .unwrap()
            .contains("unknown variant"));
        assert_eq!(methods, ["ping", "agent.result"]);
    }
}

#[test]
fn unrelated_errors_are_preserved() {
    let (output, methods) = run(
        &["agent", "result", "fixture"],
        Some("0.10.0"),
        Some(serde_json::json!({"code":"pane_not_found", "message":"fixture pane missing"})),
        false,
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("fixture pane missing"));
    assert_eq!(methods, ["ping", "agent.result"]);
}

#[test]
fn older_server_can_still_run_supported_commands_with_one_warning() {
    let (output, methods) = run(
        &["delegate", "status", "fixture"],
        Some("0.10.0"),
        None,
        false,
    );
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(methods, ["ping", "agent.get", "agent.get"]);
    assert_eq!(
        String::from_utf8(output.stderr)
            .unwrap()
            .matches("warning:")
            .count(),
        1
    );
}

#[test]
fn missing_turn_cursor_retains_pre_09_diagnosis() {
    let (output, methods) = run(
        &["delegate", "send", "fixture", "--brief", "BRIEF"],
        Some("0.6.8"),
        None,
        false,
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("needs a server ≥ 0.9.0"));
    assert_eq!(methods, ["ping", "agent.get", "agent.get"]);
}

#[test]
fn mcp_returns_structured_version_gap() {
    let (output, methods) = run(
        &["mcp", "serve"],
        Some("0.10.0"),
        Some(unknown_variant()),
        false,
    );
    assert_eq!(output.status.code(), Some(0));
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["error"]["data"]["refusal"], "server_version_gap");
    assert!(response["error"]["message"]
        .as_str()
        .unwrap()
        .contains("server is flk 0.10.0 (protocol 25); this command needs protocol 27"));
    assert_eq!(methods, ["agent.result", "ping"]);
}

#[test]
fn status_reuses_its_own_ping() {
    let (output, methods) = run(
        &["status", "server", "--json"],
        Some("current"),
        None,
        false,
    );
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(methods, ["ping"]);
    assert!(output.stderr.is_empty());
}

#[test]
fn mcp_success_needs_no_diagnostic_ping() {
    let (output, methods) = run(&["mcp", "serve"], Some("current"), None, false);
    assert_eq!(output.status.code(), Some(0));
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(methods, ["agent.result"]);
}

#[test]
fn consumed_lookup_errors_still_report_the_protocol_gap() {
    let (output, methods) = run(
        &["delegate", "status", "fixture"],
        Some("0.10.0"),
        Some(unknown_variant()),
        false,
    );
    assert_gap(output, "0.10.0");
    assert_eq!(methods, ["ping", "agent.get"]);
}
