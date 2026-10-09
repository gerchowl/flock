//! Exercise notification delivery exit codes through the compiled CLI.

#![allow(clippy::disallowed_methods)] // The harness drives the compiled binary.

mod support;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::thread;
use support::environment::Command;

struct TempSocket(std::path::PathBuf);

impl TempSocket {
    fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!("flk-589-{}-{nanos}.sock", std::process::id())))
    }
}

impl Drop for TempSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn show_response(response: serde_json::Value, expected_exit: i32) {
    let socket = TempSocket::new();
    let listener = UnixListener::bind(&socket.0).expect("bind isolated socket");
    let reply = response.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("CLI connects");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["method"], "notification.show");
        assert_eq!(request["params"]["title"], "owner needed");
        writeln!(stream, "{reply}").unwrap();
    });
    let output = Command::new(env!("CARGO_BIN_EXE_flk"))
        .args(["notification", "show", "owner needed"])
        .env("FLOCK_SOCKET_PATH", &socket.0)
        .env_remove("FLOCK_CLIENT_SOCKET_PATH")
        .env_remove("FLOCK_SESSION")
        .env_remove("FLOCK_ENV")
        .output()
        .expect("run CLI");
    server.join().unwrap();
    assert_eq!(output.status.code(), Some(expected_exit));
    let json = if expected_exit == 1 {
        &output.stderr
    } else {
        assert!(output.stderr.is_empty());
        &output.stdout
    };
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(json).unwrap(),
        response
    );
}

#[test]
fn notification_show_refusals_exit_three_and_preserve_json() {
    for reason in ["disabled", "busy", "rate_limited", "no_foreground_client"] {
        show_response(
            serde_json::json!({
                "id": "cli:notification:show",
                "result": {"type": "notification_show", "shown": false, "reason": reason}
            }),
            3,
        );
    }
}

#[test]
fn notification_show_delivery_exits_zero() {
    show_response(
        serde_json::json!({
            "id": "cli:notification:show",
            "result": {"type": "notification_show", "shown": true, "reason": "shown"}
        }),
        0,
    );
}

#[test]
fn notification_show_api_errors_exit_one() {
    show_response(
        serde_json::json!({
            "id": "cli:notification:show",
            "error": {"code": "invalid_params", "message": "notification title is empty"}
        }),
        1,
    );
}

#[test]
fn notification_show_help_explains_disabled_delivery() {
    let output = Command::new(env!("CARGO_BIN_EXE_flk"))
        .args(["notification", "show", "--help"])
        .output()
        .expect("run help");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("[ui.toast]"));
    assert!(help.contains("delivery = \"off\""));
    assert!(help.contains("disabled"));
    assert!(help.contains("exit 3"));
    assert!(help.contains("still recorded"));
}
