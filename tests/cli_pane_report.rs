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

mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use support::environment::Command;

const OK_RESPONSE: &str = r#"{"id":"cli:request","result":{"type":"ok"}}"#;
const PANE_NOT_FOUND: &str =
    r#"{"id":"cli:request","error":{"code":"pane_not_found","message":"pane w1:pZZ not found"}}"#;

/// `flk`'s usage/refusal exit code.
const REFUSAL_EXIT: i32 = 2;

/// A socket path that cleans itself up, so a failing assertion cannot leave a
/// file behind in `temp_dir` for the next run to trip over.
struct TempSocket(PathBuf);

impl TempSocket {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Self(std::env::temp_dir().join(format!("flk-454-{}-{nanos}.sock", std::process::id())))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempSocket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Answer the version probe, then one request with a canned response and the request
/// line back to the test, so the assertions are about the wire.
fn serve_once(socket: &Path, response: &'static str) -> mpsc::Receiver<String> {
    let listener = UnixListener::bind(socket).expect("bind stand-in socket");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || loop {
        let (mut stream, _) = listener.accept().expect("a client should connect");
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .expect("read one request line");
        if support::compatibility::answer_probe(&mut stream, &line) {
            continue;
        }
        stream.write_all(response.as_bytes()).unwrap();
        stream.write_all(b"\n").unwrap();
        stream.flush().unwrap();
        let _ = tx.send(line);
        break;
    });
    rx
}

fn run_report_agent(
    socket: &Path,
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

/// Run the CLI with no listener bound at all, for the cases where the refusal
/// is expected BEFORE the socket. Binding a listener here and then waiting for
/// a request that must never arrive would burn the full recv timeout on every
/// run and prove nothing a connect-refused would not.
fn run_without_a_server(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_flk"))
        .args(["pane", "report-agent"])
        .args(args)
        .env(
            "FLOCK_SOCKET_PATH",
            std::env::temp_dir().join(format!("flk-454-no-server-{}.sock", std::process::id())),
        )
        .env_remove("FLOCK_CLIENT_SOCKET_PATH")
        .env_remove("FLOCK_SESSION")
        .env_remove("FLOCK_ENV")
        .env_remove("FLOCK_PANE_ID")
        .output()
        .expect("flk should run")
}

#[test]
fn a_leading_source_flag_is_a_flag_and_the_report_still_goes_out() {
    // The #454 invocation, verbatim. Before, `args[0]` was read as the pane id,
    // so `--source` became the pane and `ax-dispatch` was refused as an unknown
    // option — exit 2, which a `|| true` reporter turns into silence.
    let socket = TempSocket::new();
    let (output, request) = run_report_agent(
        socket.path(),
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
}

#[test]
fn an_env_pane_id_is_the_reporting_default() {
    let socket = TempSocket::new();
    let (output, request) = run_report_agent(
        socket.path(),
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
}

#[test]
fn the_legacy_positional_pane_id_still_reaches_the_server() {
    // What `scripts/seed_navigator_demo.sh:90` passes, and what the docs show.
    // The hook assets do not come through this argv at all: they shell to
    // `flk hook <agent>` or speak the socket directly.
    let socket = TempSocket::new();
    let (output, request) = run_report_agent(
        socket.path(),
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
}

#[test]
fn a_pane_the_server_cannot_resolve_is_a_non_zero_exit() {
    // The second half of the trap: `999999` names no pane, so the server says
    // so, and the exit status has to carry it. Exit 0 here reads as "reported"
    // to every caller that only checks status.
    let socket = TempSocket::new();
    let (output, request) = run_report_agent(
        socket.path(),
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
}

#[test]
fn an_unknown_flag_is_refused_by_name_before_the_socket() {
    let output = run_without_a_server(&[
        "--sauce",
        "--source",
        "ax-dispatch",
        "--agent",
        "zzz",
        "--state",
        "working",
    ]);

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
    // No server was ever bound, so a run that reached the socket would have
    // failed with a connection error instead of this refusal.
}

#[test]
fn a_stray_bare_word_is_refused_rather_than_eaten_as_the_pane() {
    // #454 follow-up. An unquoted multi-word value leaves a bare word that no
    // flag claims. Reading it as the pane would report to a pane nobody named
    // and exit 0 — the same trap #454 is about, moved rather than closed. It
    // must read as the refusal the sibling verbs give, on stderr, non-zero.
    for (args, stray) in [
        (
            vec![
                "--source",
                "ax-dispatch",
                "--agent",
                "zzz",
                "--state",
                "working",
                "--message",
                "waiting",
                "for",
                "the",
                "build",
            ],
            "for",
        ),
        (
            vec![
                "--source",
                "ax-dispatch",
                "--agent",
                "zzz",
                "--state",
                "working",
                "waiting",
            ],
            "waiting",
        ),
    ] {
        let output = run_without_a_server(&args);
        assert_eq!(
            output.status.code(),
            Some(REFUSAL_EXIT),
            "a stray word must be a refusal, not a report to an unnamed pane: {}",
            stderr_of(&output)
        );
        let stderr = stderr_of(&output);
        assert!(
            stderr.contains(stray),
            "the refusal must name the word it refused: {stderr}"
        );
        assert!(
            stderr.contains("unknown option"),
            "and read like the sibling verbs' refusal: {stderr}"
        );
    }
}

/// A flag's own value is that flag's, wherever it sits — the guard that keeps
/// the positional at index 0 must not have broken `--message waiting`.
#[test]
fn a_flag_value_is_still_its_flags_at_any_position() {
    let socket = TempSocket::new();
    let (output, request) = run_report_agent(
        socket.path(),
        OK_RESPONSE,
        None,
        &[
            "--source",
            "ax-dispatch",
            "--agent",
            "zzz",
            "--state",
            "working",
            "--message",
            "waiting",
        ],
    );

    assert_eq!(
        request["params"].get("message"),
        Some(&serde_json::json!("waiting")),
        "the trailing value belongs to --message, not to the pane: {request}"
    );
    assert!(output.status.success(), "{}", stderr_of(&output));
}

#[test]
fn help_advertises_the_optional_pane() {
    let output = Command::new(env!("CARGO_BIN_EXE_flk"))
        .args(["pane", "help"])
        .env(
            "FLOCK_SOCKET_PATH",
            std::env::temp_dir().join(format!("flk-454-help-{}.sock", std::process::id())),
        )
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
