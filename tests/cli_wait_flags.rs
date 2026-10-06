//! E2E (#553): the two wait verbs agree on what a bad `--timeout` is, and a
//! settled wait treats it as the usage error it is.
//!
//! Driven through the compiled binary because the exit code *is* the contract:
//! a supervisor scripting `--status settled` branches on 0/3/4/124/2/1, and a
//! typo in `--timeout` that surfaces as 1 rather than 2 is indistinguishable
//! from "the server said something I could not use". Calling the parser
//! directly would assert on the message text and not on the number the script
//! reads.
//!
//! No server is needed and none is started. `--status settled` validates before
//! it sends anything, so a run that got as far as the socket would fail loudly
//! and differently — which is why the refused cases below still exit 2 against a
//! socket path that cannot exist.
//!
//! The last case is the one that makes the other two worth having: the
//! non-settled statuses must KEEP their historical code. An earlier round made
//! `--settle` a usage error under `--status settled` only, and the temptation
//! to generalise that to `--timeout` is exactly what this pins shut — the plain
//! statuses report a bad timeout as an io error, and a script that has always
//! read 1 there must keep reading 1.

// Integration tests drive the compiled binary through raw Command; the
// TracedCommand funnel polices flock's own subprocesses, not the harness's.
#![allow(clippy::disallowed_methods)]

use std::process::{Command, Output};

/// The usage-error code both wait verbs reserve for a flag they cannot accept.
const USAGE_EXIT: i32 = 2;
/// What a plain-status wait has always returned for a bad `--timeout`: the
/// value reached as an io error out of the parse, before exits were split.
const LEGACY_ERROR_EXIT: i32 = 1;

fn flk(args: &[&str]) -> Output {
    let mut socket = std::env::temp_dir();
    socket.push("flock-553-no-such-server.sock");
    Command::new(env!("CARGO_BIN_EXE_flk"))
        .args(args)
        .env("FLOCK_SOCKET_PATH", &socket)
        .env_remove("FLOCK_CLIENT_SOCKET_PATH")
        .env_remove("FLOCK_ENV")
        .output()
        .expect("flk should run")
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A settled wait refuses a `--timeout` it cannot read, in BOTH verbs.
///
/// Round 1 fixed `--settle` and left `--timeout` returning an io error, so
/// `--status settled --timeout abc` exited 1 — the code a settled wait uses for
/// "some other error", and the code `124` was introduced to keep distinct from.
#[test]
fn a_settled_wait_refuses_a_timeout_it_cannot_read() {
    for args in [
        // `flk agent wait <target> --status settled`
        vec![
            "agent",
            "wait",
            "w1:p1",
            "--status",
            "settled",
            "--timeout",
            "abc",
        ],
        // `flk wait agent-status <pane_id> --status settled`
        vec![
            "wait",
            "agent-status",
            "w1:p1",
            "--status",
            "settled",
            "--timeout",
            "abc",
        ],
    ] {
        let output = flk(&args);
        assert_eq!(
            output.status.code(),
            Some(USAGE_EXIT),
            "{args:?}: a bad --timeout is a usage error: {}",
            stderr_of(&output),
        );
        assert!(
            stderr_of(&output).contains("--timeout"),
            "{args:?} must name the flag it refused: {}",
            stderr_of(&output),
        );
    }
}

/// The other settle flags stay usage errors under a settled target, and stay
/// exit 2 before any request — the behaviour round 0 introduced, pinned here
/// alongside `--timeout` so the two cannot drift apart.
#[test]
fn the_settle_flags_are_all_usage_errors_under_a_settled_target() {
    // Written out whole rather than composed, because each case differs in
    // WHICH target it carries: `--ready` is the one that must be refused for
    // having a settle flag at all, so it has to be spelled out as such.
    let cases: [(&[&str], &str); 4] = [
        (
            &[
                "agent", "wait", "w1:p1", "--status", "settled", "--settle", "abc",
            ],
            "--settle",
        ),
        (
            &[
                "agent", "wait", "w1:p1", "--status", "idle", "--settle", "100",
            ],
            "--settle",
        ),
        (
            &[
                "agent",
                "wait",
                "w1:p1",
                "--status",
                "working",
                "--after",
                "term_x:0:0:1:i",
            ],
            "--after",
        ),
        (
            &["agent", "wait", "w1:p1", "--ready", "--settle", "100"],
            "--settle",
        ),
    ];
    for (args, named) in cases {
        let output = flk(args);
        assert_eq!(
            output.status.code(),
            Some(USAGE_EXIT),
            "{args:?} must be a usage error: {}",
            stderr_of(&output),
        );
        assert!(
            stderr_of(&output).contains(named),
            "{args:?} must name {named}: {}",
            stderr_of(&output),
        );
    }
}

/// And the plain statuses keep the code they always had.
///
/// This is the half that would be easy to break while fixing the half above: a
/// caller who has always read exit 1 for `--status idle --timeout abc` is
/// reading it for "bad input", not for "settled-ish failure", and swapping it
/// to 2 would repoint every such script at a code it does not handle.
#[test]
fn a_plain_status_wait_keeps_its_historical_exit_code_for_a_bad_timeout() {
    for args in [
        vec![
            "agent",
            "wait",
            "w1:p1",
            "--status",
            "idle",
            "--timeout",
            "abc",
        ],
        vec![
            "wait",
            "agent-status",
            "w1:p1",
            "--status",
            "idle",
            "--timeout",
            "abc",
        ],
        vec![
            "agent",
            "wait",
            "w1:p1",
            "--status",
            "working",
            "--timeout",
            "abc",
        ],
    ] {
        let output = flk(&args);
        assert_eq!(
            output.status.code(),
            Some(LEGACY_ERROR_EXIT),
            "{args:?} must keep its historical exit code: {}",
            stderr_of(&output),
        );
        assert!(
            stderr_of(&output).contains("--timeout"),
            "{args:?} must still say which value it refused: {}",
            stderr_of(&output),
        );
    }
}
