//! E2E (#380): `flk msg send` must refuse a flag it does not understand
//! rather than deliver it as the first words of the message body.
//!
//! Driven through the compiled binary, not the parser, because the thing this
//! protects is a binary: the cross-host relay is `flk msg send` invoked over
//! ssh on the peer that owns the recipient (`send_peer_message`), and the
//! peer's exit status is the only channel the refusal has. A test that called
//! the parser directly would assert on the words and not on the exit code the
//! relay actually classifies.
//!
//! No server is needed and none is started: the refusal happens during
//! parsing, before the socket. `FLOCK_SOCKET_PATH` points at a path that
//! cannot exist so that anything which *does* get past parsing fails quickly
//! and for an unmistakably different reason.

// Integration tests drive the compiled binary through raw Command; the
// TracedCommand funnel polices flock's own subprocesses, not the harness's.
#![allow(clippy::disallowed_methods)]

use std::process::{Command, Output};

/// `flk`'s usage/refusal exit code — and the value `peers::REMOTE_REFUSAL_EXIT`
/// reads off a peer to tell "it refused" from "it never answered".
const REFUSAL_EXIT: i32 = 2;

fn flk_msg(args: &[&str]) -> Output {
    let mut socket = std::env::temp_dir();
    socket.push("flock-380-no-such-server.sock");
    Command::new(env!("CARGO_BIN_EXE_flk"))
        .arg("msg")
        .args(args)
        .env("FLOCK_SOCKET_PATH", &socket)
        .env_remove("FLOCK_CLIENT_SOCKET_PATH")
        .env_remove("FLOCK_ENV")
        .output()
        .expect("flk should run")
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

#[test]
fn an_unknown_flag_is_refused_instead_of_becoming_the_message_body() {
    // Before this, the flag and its value were appended to the body and the
    // send SUCCEEDED — so a `--intent needs_reply` relayed to a host on an
    // older build arrived as a message beginning "--intent needs_reply", with
    // nothing at either end recording that a flag had been misunderstood.
    let output = flk_msg(&[
        "send",
        "--agent",
        "agent_sage_1",
        "--intent-from-a-future-build",
        "needs_reply",
        "the real message",
    ]);
    assert_eq!(
        output.status.code(),
        Some(REFUSAL_EXIT),
        "a flag this build does not understand must be a refusal, not a body: {}",
        stderr_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("--intent-from-a-future-build"),
        "the refusal must name the flag it refused: {stderr}"
    );
    assert!(
        stderr.contains("--reply-to"),
        "and must list what this build DOES understand — across the relay that \
         list is the peer naming its own vintage: {stderr}"
    );
    assert!(
        !stderr.contains("the real message"),
        "the body is not the diagnosis: {stderr}"
    );
}

#[test]
fn a_dash_dash_terminator_lets_a_body_begin_with_dashes() {
    // The escape hatch the refusal depends on. It already existed on `send`
    // (the relay quotes every body behind one); the refusal is what makes it
    // load-bearing rather than incidental.
    let output = flk_msg(&["send", "--", "--not-a-flag", "still-not-a-flag"]);
    let stderr = stderr_of(&output);
    assert!(
        !stderr.contains("unknown option"),
        "everything after `--` is text: {stderr}"
    );
    // Two positionals and no `--agent` is a target plus a body, so parsing
    // got all the way through and the command failed on the missing server.
    assert!(
        !stderr.contains("usage: flk msg send"),
        "`--` must not eat the arguments it terminates: {stderr}"
    );
}

#[test]
fn reply_refuses_unknown_flags_and_honours_the_terminator() {
    // `reply` grew `--intent` in the same PR `send` did (#280) and had no
    // terminator at all, so a reply whose text began with dashes lost its
    // correlation id to the body.
    let refused = flk_msg(&["reply", "--needs-reply", "c-1", "answered"]);
    assert_eq!(
        refused.status.code(),
        Some(REFUSAL_EXIT),
        "{}",
        stderr_of(&refused)
    );
    assert!(
        stderr_of(&refused).contains("--needs-reply"),
        "{}",
        stderr_of(&refused)
    );

    let accepted = flk_msg(&["reply", "c-1", "--", "--not-a-flag"]);
    let stderr = stderr_of(&accepted);
    assert!(!stderr.contains("unknown option"), "{stderr}");
    assert!(!stderr.contains("usage: flk msg reply"), "{stderr}");
}
