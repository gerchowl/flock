//! Version gates run through the CLI and an isolated socket, before any work.
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
            let response = if method == "ping" {
                serde_json::json!({"id":request["id"],"result":{"type":"pong","version":version,"protocol":1}})
            } else {
                serde_json::json!({"id":request["id"],"error":error.clone().unwrap_or_else(|| serde_json::json!({"code":"pane_not_found","message":"fixture pane missing"}))})
            };
            methods.push(method);
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

#[test]
fn delegate_old_server_is_rejected_before_any_work() {
    for verb in ["start", "send", "wait", "result", "status", "reap"] {
        let args = if verb == "start" {
            vec![
                "delegate", verb, "fixture", "--brief", "BRIEF", "--cwd", "CWD",
            ]
        } else {
            vec!["delegate", verb, "fixture"]
        };
        let (output, methods) = run(&args, Some("0.6.8-fork.9ebb536"), None);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("flk delegate needs a server ≥ 0.9.0 (running: 0.6.8-fork.9ebb536)"),
            "{stderr}"
        );
        assert!(stderr.contains("flk server live-handoff"));
        assert_eq!(methods, ["ping"]);
    }
}

#[test]
fn agent_result_old_or_unknown_server_is_rejected_before_lookup() {
    for version in [Some("0.8.0"), None, Some("unparseable")] {
        let (output, methods) = run(&["agent", "result", "fixture"], version, None);
        assert_eq!(output.status.code(), Some(1));
        assert!(String::from_utf8(output.stderr)
            .unwrap()
            .contains("flk agent result needs a server ≥ 0.9.0"));
        assert_eq!(methods, ["ping"]);
    }
}

#[test]
fn agent_result_supported_versions_preserve_unrelated_errors() {
    for version in ["0.9.0", "0.9.0-preview.abc", "0.10.0+build", "1.0.0"] {
        let (output, methods) = run(&["agent", "result", "fixture"], Some(version), None);
        assert_eq!(output.status.code(), Some(1));
        assert!(String::from_utf8(output.stderr)
            .unwrap()
            .contains("pane_not_found"));
        assert_eq!(methods, ["ping", "agent.result"]);
    }
}

#[test]
fn agent_result_unknown_variant_names_version_gap_without_variant_list() {
    let error = serde_json::json!({"code":"invalid_request", "message":"unknown variant `agent.result`, expected one of `ping`, `agent.list`"});
    let (output, methods) = run(&["agent", "result", "fixture"], Some("0.9.0"), Some(error));
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("flk agent result needs a server ≥ 0.9.0 (running: 0.9.0)"));
    assert!(!stderr.contains("unknown variant"));
    assert_eq!(methods, ["ping", "agent.result", "ping"]);
}

#[test]
fn delegate_unknown_variant_names_version_gap() {
    let error = serde_json::json!({"code":"invalid_request", "message":"unknown variant `agent.get`, expected one of `ping`"});
    let (output, methods) = run(
        &[
            "delegate", "start", "fixture", "--brief", "BRIEF", "--cwd", "CWD",
        ],
        Some("0.9.0"),
        Some(error),
    );
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("flk delegate needs a server ≥ 0.9.0"),
        "{stderr}"
    );
    assert!(!stderr.contains("unknown variant"));
    assert!(methods.len() >= 2);
}
