//! E2E (#1025): the moved exit codes of ADR-0029 §2, through the real `flk`
//! binary against a scripted socket. Each moved case returns its new code,
//! names the old one on stderr, and leaves stdout to the result alone.
//!
//! The blocked settled wait (3 -> 6) is pinned against a real server in
//! `agent_settled.rs`, `not_started` (8 -> 11) in `delegate_verdict.rs`, and
//! the delegate refusals (1 -> 10) in `delegate_claude.rs`.

// Integration tests drive the compiled binary through raw Command; the
// TracedCommand funnel polices flock's own subprocesses, not the harness's.
#![allow(clippy::disallowed_methods)]

mod support;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::Output;
use std::thread;
use std::time::{Duration, Instant};
use support::environment::Command;

/// A socket that answers every request with `answer(method)`, and the protocol
/// check a state-changing verb sends first (#868) with this build's protocol.
fn scripted_server(
    name: &str,
    answer: fn(&str) -> serde_json::Value,
) -> (PathBuf, thread::JoinHandle<()>) {
    let dir = std::env::temp_dir().join(format!("flk1025-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("s.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            stream.set_nonblocking(false).unwrap();
            let mut line = String::new();
            if BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .is_err()
            {
                continue;
            }
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            let method = request["method"].as_str().unwrap_or_default();
            let mut reply = if method == "ping" {
                serde_json::json!({"result":{
                    "type":"pong","version":"fixture","protocol":support::PROTOCOL_VERSION
                }})
            } else {
                answer(method)
            };
            reply["id"] = request["id"].clone();
            let _ = writeln!(stream, "{reply}");
        }
    });
    (socket, handle)
}

fn flk(socket: &PathBuf, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_flk"))
        .args(args)
        .env("FLOCK_SOCKET_PATH", socket)
        .env_remove("FLOCK_CLIENT_SOCKET_PATH")
        .env_remove("FLOCK_ENV")
        .output()
        .expect("flk should run")
}

fn assert_moved(output: &Output, new: i32, old: i32) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(new), "stderr: {stderr}");
    let notice = format!("note: exit code is now {new} (was {old}); see CHANGELOG");
    assert_eq!(
        stderr.lines().filter(|line| *line == notice).count(),
        1,
        "stderr: {stderr}"
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("note:"),
        "the notice must stay off stdout"
    );
}

fn refuse(_: &str) -> serde_json::Value {
    serde_json::json!({"error":{"code":"not_settled","message":"not_settled"}})
}

/// 1 -> 10: a guarded submit the server refused before typing anything.
#[test]
fn a_refusal_before_acting_exits_10_and_names_1() {
    let (socket, _server) = scripted_server("refused", refuse);
    let output = flk(&socket, &["pane", "run", "1-1", "--if-input-empty", "hi"]);
    assert_moved(&output, 10, 1);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not_settled"), "{stderr}");
}

/// `agent send` is send-like: the same refusal is 10.
#[test]
fn agent_send_refused_before_acting_exits_10() {
    let (socket, _server) = scripted_server("send", refuse);
    let output = flk(&socket, &["agent", "send", "worker", "hi"]);
    assert_moved(&output, 10, 1);
}

/// `agent restart` is guarded: a busy or rate-limited agent is refused before
/// anything restarts, and that is 10 too.
#[test]
fn agent_restart_refused_before_acting_exits_10() {
    fn busy(_: &str) -> serde_json::Value {
        serde_json::json!({"error":{"code":"restart_busy","message":"restart_busy"}})
    }
    let (socket, _server) = scripted_server("restart", busy);
    let output = flk(&socket, &["agent", "restart", "worker", "--reason", "r"]);
    assert_moved(&output, 10, 1);
}

/// A verb that is neither send-like nor guarded keeps exit 1 for a `later`
/// refusal, with no notice: the move is scoped to those verbs (ADR-0029 §2).
#[test]
fn a_refusal_from_a_read_verb_keeps_exit_1() {
    let (socket, _server) = scripted_server("read", refuse);
    let output = flk(&socket, &["agent", "get", "worker"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    assert!(!stderr.contains("note: exit code"), "{stderr}");
}

/// An error that is not a refusal keeps exit 1 and prints no notice.
#[test]
fn an_ordinary_error_keeps_exit_1_without_a_notice() {
    fn missing(_: &str) -> serde_json::Value {
        serde_json::json!({"error":{"code":"pane_not_found","message":"no pane"}})
    }
    let (socket, _server) = scripted_server("missing", missing);
    let output = flk(&socket, &["pane", "run", "1-1", "--if-input-empty", "hi"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    assert!(!stderr.contains("note: exit code"), "{stderr}");
}

fn refused_message(method: &str) -> serde_json::Value {
    match method {
        "msg.status" => serde_json::json!({"result":{
            "correlation_id":"q1025","state":"refused","detail":"msg_not_allowed"
        }}),
        _ => serde_json::json!({"result":{
            "correlation_id":"q1025","outcome":"refused","detail":"msg_not_allowed"
        }}),
    }
}

/// 4 -> 13: `msg status` and `wait reply --json` on a refused message, with
/// stdout still exactly the JSON response.
#[test]
fn a_refused_message_exits_13_and_names_4() {
    let (socket, _server) = scripted_server("msg", refused_message);
    let status = flk(&socket, &["msg", "status", "q1025"]);
    assert_moved(&status, 13, 4);
    let waited = flk(&socket, &["wait", "reply", "q1025", "--json"]);
    assert_moved(&waited, 13, 4);
    let stdout: serde_json::Value = serde_json::from_slice(&waited.stdout).unwrap();
    assert_eq!(stdout["result"]["outcome"], "refused", "{stdout}");
}
