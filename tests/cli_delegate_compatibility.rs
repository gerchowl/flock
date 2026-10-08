//! Capability-failure diagnosis through the CLI and an isolated socket.
#![allow(clippy::disallowed_methods)] // The harness drives the compiled binary.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
                while !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                break;
            }
            let response = if method == "ping" {
                serde_json::json!({"id":request["id"],"result":{"type":"pong","version":version,"protocol":1}})
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
    let output = Command::new(env!("CARGO_BIN_EXE_flk"))
        .args(args)
        .env("FLOCK_SOCKET_PATH", &socket)
        .env("XDG_STATE_HOME", dir.path())
        .env_remove("FLOCK_CLIENT_SOCKET_PATH")
        .env_remove("FLOCK_SESSION")
        .env_remove("FLOCK_ENV")
        .output()
        .unwrap();
    stopped.store(true, Ordering::SeqCst);
    (output, server.join().unwrap())
}

fn unknown_variant() -> serde_json::Value {
    serde_json::json!({"code":"invalid_request", "message":"unknown variant `agent.result`, expected one of `ping`, `agent.list`"})
}

#[test]
fn agent_result_success_has_no_diagnostic_ping() {
    let (output, methods) = run(&["agent", "result", "fixture"], None, None, false);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(methods, ["agent.result"]);
}

#[test]
fn delegate_success_has_no_diagnostic_ping() {
    let (output, methods) = run(&["delegate", "status", "fixture"], None, None, false);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(methods, ["agent.get", "agent.get"]);
}

#[test]
fn agent_result_old_server_names_version_gap() {
    let (output, methods) = run(
        &["agent", "result", "fixture"],
        Some("0.6.8-fork.9ebb536"),
        Some(unknown_variant()),
        false,
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("flk agent result needs a server ≥ 0.9.0 (running: 0.6.8-fork.9ebb536); hand it off with `flk server live-handoff` or upgrade and restart it"), "{stderr}");
    assert!(!stderr.contains("unknown variant"));
    assert_eq!(methods, ["agent.result", "ping"]);
}

#[test]
fn unknown_or_supported_version_preserves_original_error() {
    for version in [
        None,
        Some("unparseable"),
        Some("0.9.0"),
        Some("0.9.0-preview.abc"),
        Some("0.10.0+build"),
    ] {
        let (output, methods) = run(
            &["agent", "result", "fixture"],
            version,
            Some(unknown_variant()),
            false,
        );
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("unknown variant `agent.result`, expected one of `ping`, `agent.list`"),
            "{stderr}"
        );
        assert!(!stderr.contains("needs a server"));
        assert_eq!(methods, ["agent.result", "ping"]);
    }
}

#[test]
fn diagnostic_ping_timeout_preserves_original_error() {
    let started = std::time::Instant::now();
    let (output, methods) = run(
        &["agent", "result", "fixture"],
        None,
        Some(unknown_variant()),
        true,
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("unknown variant `agent.result`"),
        "{stderr}"
    );
    assert!(!stderr.contains("needs a server"));
    assert_eq!(methods, ["agent.result", "ping"]);
}

#[test]
fn unrelated_errors_have_no_diagnostic_ping() {
    let error = serde_json::json!({"code":"pane_not_found","message":"fixture pane missing"});
    let (output, methods) = run(
        &["agent", "result", "fixture"],
        Some("0.6.8"),
        Some(error),
        false,
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("fixture pane missing"));
    assert_eq!(methods, ["agent.result"]);
}

#[test]
fn delegate_unknown_variant_names_version_gap() {
    let (output, methods) = run(
        &["delegate", "status", "fixture"],
        Some("0.6.8-fork.9ebb536"),
        Some(unknown_variant()),
        false,
    );
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("flk delegate needs a server ≥ 0.9.0 (running: 0.6.8-fork.9ebb536); hand it off with `flk server live-handoff` or upgrade and restart it"), "{stderr}");
    assert!(!stderr.contains("unknown variant"));
    assert_eq!(methods, ["agent.get", "ping"]);
}

#[test]
fn delegate_missing_turn_cursor_diagnoses_only_confirmed_old_server() {
    for version in [Some("0.6.8-fork.9ebb536"), None, Some("0.9.0")] {
        let (output, methods) = run(
            &["delegate", "send", "fixture", "--brief", "BRIEF"],
            version,
            None,
            false,
        );
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8(output.stderr).unwrap();
        if version == Some("0.6.8-fork.9ebb536") {
            assert!(
                stderr
                    .contains("flk delegate needs a server ≥ 0.9.0 (running: 0.6.8-fork.9ebb536)"),
                "{stderr}"
            );
            assert!(stderr.contains("flk server live-handoff"));
        } else {
            assert!(
                stderr.contains("the server's record carried no turn cursor"),
                "{stderr}"
            );
            assert!(!stderr.contains("needs a server"));
        }
        assert_eq!(methods, ["agent.get", "agent.get", "ping"]);
    }
}

#[test]
fn unknown_method_diagnoses_confirmed_old_server() {
    let error =
        serde_json::json!({"code":"unknown_method", "message":"unknown method agent.result"});
    let (output, methods) = run(
        &["agent", "result", "fixture"],
        Some("0.8.0"),
        Some(error),
        false,
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("flk agent result needs a server ≥ 0.9.0 (running: 0.8.0)"));
    assert_eq!(methods, ["agent.result", "ping"]);
}
