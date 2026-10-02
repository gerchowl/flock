//! E2E (#454): `flk pane report-agent` must not swallow the caller's first
//! flag as the pane id, and a pane it cannot resolve must not exit 0.
//!
//! Driven through the compiled binary against a stand-in socket rather than
//! through the parser, because the two things being protected are an exit code
//! and a wire request: a test that called `parse_report_agent_args` directly
//! would prove the parser agrees with itself and not that a reporting hook —
//! which wraps the call in `>/dev/null 2>&1 || true` because a hook must never
//! break what it reports on — gets a status it can act on.
//!
//! The stand-in answers one request per connection, so what each test asserts
//! is what flock actually put on the wire.

#![allow(clippy::disallowed_methods)] // The harness drives the compiled binary.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const OK_RESPONSE: &str = r#"{"id":"cli:request","result":{"type":"ok"}}"#;
const PANE_NOT_FOUND: &str =
    r#"{"id":"cli:request","error":{"code":"pane_not_found","message":"pane w1:pZZ not found"}}"#;

/// `flk`'s usage/refusal exit code.
const REFUSAL_EXIT: i32 = 2;

fn unique_socket() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("flk-454-{}-{nanos}.sock", std::process::id()))
}

/// One connection, one request line, one canned response — and the request
/// line back to the test, so the assertions are about the wire.
fn serve_once(socket: &PathBuf, response: &'static str) -> mpsc::Receiver<String> {
    let listener = UnixListener::bind(socket).expect("bind stand-in socket");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("a client should connect");
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .expect("read one request line");
        stream.write_all(response.as_bytes()).unwrap();
        stream.write_all(b"\n").unwrap();
        stream.flush().unwrap();
        let _ = tx.send(line);
    });
    rx
}

fn run_report_agent(
    socket: &PathBuf,
    response: &'static str,
    pane_env: Option<&str>,
    args: &[&str],
) -> (Output, serde_json::Value) {
    let request = serve_once(socket, response);
    let mut command = Command::new(env!("CARGO_BIN_EXE_flk"));
    command
        .args(["pane", "report-agent"])
        .args(args)
        .env("FLOCK_SOCKET_PATH", socket)
        .env_remove("FLOCK_CLIENT_SOCKET_PATH")
        .env_remove("FLOCK_SESSION")
        .env_remove("FLOCK_ENV");
    match pane_env {
        Some(pane) => command.env("FLOCK_PANE_ID", pane),
        None => command.env_remove("FLOCK_PANE_ID"),
    };
    let output = command.output().expect("flk should run");
    let line = request
        .recv_timeout(Duration::from_secs(10))
        .expect("the CLI should have sent exactly one request");
    let request: serde_json::Value =
        serde_json::from_str(&line).unwrap_or_else(|err| panic!("a JSON request: {err}: {line}"));
    (output, request)
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn cleanup(socket: &PathBuf) {
    let _ = fs::remove_file(socket);
}

#[test]
fn a_leading_source_flag_is_a_flag_and_the_report_still_goes_out() {
    // The #454 invocation, verbatim. Before, `args[0]` was read as the pane id,
    // so `--source` became the pane and `ax-dispatch` was refused as an unknown
    // option — exit 2, which a `|| true` reporter turns into silence.
    let socket = unique_socket();
    let (output, request) = run_report_agent(
        &socket,
        OK_RESPONSE,
        None,
        &[
            "--source",
            "ax-dispatch",
            "--agent",
            "zzz",
            "--state",
            "working",
        ],
    );

    assert_eq!(request["method"], "pane.report_agent");
    assert_eq!(
        request["params"]["source"], "ax-dispatch",
        "--source must survive as the source: {request}"
    );
    assert_eq!(request["params"]["agent"], "zzz");
    assert_eq!(request["params"]["state"], "working");
    // No pane named and none in the env: the report claims the calling pane,
    // which the server heals by process ancestry — the same default
    // report-recap uses.
    assert_eq!(request["params"]["pane_id"], "");
    assert!(
        output.status.success(),
        "an unresolvable-at-parse-time report is still sent: {}",
        stderr_of(&output)
    );

    cleanup(&socket);
}

#[test]
fn an_env_pane_id_is_the_reporting_default() {
    let socket = unique_socket();
    let (output, request) = run_report_agent(
        &socket,
        OK_RESPONSE,
        Some("p_7"),
        &[
            "--source",
            "flock:claude",
            "--agent",
            "claude",
            "--state",
            "idle",
        ],
    );

    assert_eq!(
        request["params"]["pane_id"], "p_7",
        "with no pane on the command line, $FLOCK_PANE_ID names it: {request}"
    );
    assert!(output.status.success(), "{}", stderr_of(&output));

    cleanup(&socket);
}

#[test]
fn the_legacy_positional_pane_id_still_reaches_the_server() {
    // What `scripts/seed_navigator_demo.sh:90` passes, and what the docs show.
    // The hook assets do not come through this argv at all: they shell to
    // `flk hook <agent>` or speak the socket directly.
    let socket = unique_socket();
    let (output, request) = run_report_agent(
        &socket,
        OK_RESPONSE,
        None,
        &[
            "w1:p3",
            "--source",
            "flock:claude",
            "--agent",
            "claude",
            "--state",
            "blocked",
        ],
    );

    assert_eq!(request["method"], "pane.report_agent");
    assert_eq!(
        request["params"]["pane_id"], "w1:p3",
        "the positional pane id must not be read as a flag or dropped: {request}"
    );
    assert_eq!(request["params"]["state"], "blocked");
    assert!(output.status.success(), "{}", stderr_of(&output));

    cleanup(&socket);
}

#[test]
fn a_pane_the_server_cannot_resolve_is_a_non_zero_exit() {
    // The second half of the trap: `999999` names no pane, so the server says
    // so, and the exit status has to carry it. Exit 0 here reads as "reported"
    // to every caller that only checks status.
    let socket = unique_socket();
    let (output, request) = run_report_agent(
        &socket,
        PANE_NOT_FOUND,
        None,
        &[
            "999999",
            "--source",
            "ax-dispatch",
            "--agent",
            "zzz",
            "--state",
            "working",
        ],
    );

    assert_eq!(request["params"]["pane_id"], "999999");
    assert_eq!(
        output.status.code(),
        Some(1),
        "an unresolved pane is a failure, not a delivered report: {}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("pane_not_found"),
        "the refusal must name the code a caller can act on: {}",
        stderr_of(&output)
    );

    cleanup(&socket);
}

#[test]
fn an_unknown_flag_is_refused_by_name_before_the_socket() {
    let socket = unique_socket();
    let output = Command::new(env!("CARGO_BIN_EXE_flk"))
        .args([
            "pane",
            "report-agent",
            "--sauce",
            "--source",
            "ax-dispatch",
            "--agent",
            "zzz",
            "--state",
            "working",
        ])
        .env("FLOCK_SOCKET_PATH", &socket)
        .env_remove("FLOCK_CLIENT_SOCKET_PATH")
        .env_remove("FLOCK_SESSION")
        .env_remove("FLOCK_ENV")
        .env_remove("FLOCK_PANE_ID")
        .output()
        .expect("flk should run");

    assert_eq!(
        output.status.code(),
        Some(REFUSAL_EXIT),
        "{}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("--sauce"),
        "the refusal must name the flag: {}",
        stderr_of(&output)
    );
    assert!(
        !socket.exists() && std::os::unix::net::UnixStream::connect(&socket).is_err(),
        "a refusal happens before the socket, so nothing may have connected"
    );

    cleanup(&socket);
}

#[test]
fn help_advertises_the_optional_pane() {
    let output = Command::new(env!("CARGO_BIN_EXE_flk"))
        .args(["pane", "help"])
        .env("FLOCK_SOCKET_PATH", "/nonexistent/flk-454-help.sock")
        .env_remove("FLOCK_CLIENT_SOCKET_PATH")
        .env_remove("FLOCK_SESSION")
        .env_remove("FLOCK_ENV")
        .output()
        .expect("flk should run");

    let help = stderr_of(&output);
    assert!(
        help.contains("flk pane report-agent [<pane_id>] --source ID"),
        "help must show the pane id as optional: {help}"
    );
}
