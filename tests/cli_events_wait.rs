//! E2E (#1029): `flk events wait` against a scripted socket, for the cases a
//! real server cannot produce on demand: a retention gap in the middle of a
//! wait, and where a wait without `--after` starts.

// Integration tests drive the compiled binary through raw Command; the
// TracedCommand funnel polices flock's own subprocesses, not the harness's.
#![allow(clippy::disallowed_methods)]

mod support;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use support::environment::Command;

/// Run `flk args` against a socket that answers each `events.read` with the
/// next of `replies`, and return the output and the `after` of every read.
fn wait_against(args: &[&str], replies: Vec<Value>) -> (std::process::Output, Vec<Value>) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("f1029-{}-{nanos}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let socket = dir.join("api.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || {
        let mut afters = Vec::new();
        let mut replies = replies.into_iter().peekable();
        while replies.peek().is_some() {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let mut response = if request["method"] == "ping" {
                json!({"result": {"type": "pong", "version": "fixture",
                    "protocol": support::PROTOCOL_VERSION}})
            } else {
                assert_eq!(request["method"], "events.read");
                afters.push(request["params"]["after"].clone());
                replies.next().unwrap()
            };
            response["id"] = request["id"].clone();
            writeln!(stream, "{response}").unwrap();
        }
        afters
    });
    let output = Command::new(env!("CARGO_BIN_EXE_flk"))
        .args(args)
        .env("FLOCK_SOCKET_PATH", &socket)
        .env_remove("FLOCK_CLIENT_SOCKET_PATH")
        .env_remove("FLOCK_SESSION")
        .env_remove("FLOCK_ENV")
        .output()
        .unwrap();
    let afters = server.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    (output, afters)
}

/// A wait with no `--after` starts at `data.head` (not the number the message
/// ends on), reports a gap it meets on stderr, resumes past it, and still
/// returns the first matching event after it.
#[test]
fn a_gap_during_a_wait_is_reported_and_skipped() {
    let event = json!({"seq": 6, "event": "pane_exited", "data": {"pane_id": "w1:p1"}});
    let (output, afters) = wait_against(
        &[
            "events",
            "wait",
            "--kinds",
            "pane_exited",
            "--timeout",
            "10s",
        ],
        vec![
            json!({"error": {"code": "cursor_ahead", "data": {"head": 1},
                "message": "cursor is past this server's newest event 99; read from 0 or 99"}}),
            json!({"result": {"gap": {"from": 2, "to": 5}, "events": [], "next_after": 5}}),
            json!({"result": {"events": [event], "next_after": 6}}),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(stderr.contains("gap: events 2..=5"), "{stderr}");
    let printed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(printed, event);
    assert_eq!(afters, [json!(u64::MAX), json!(1), json!(5)]);
}
