//! Capability-failure diagnosis through the CLI and an isolated socket.
#![allow(clippy::disallowed_methods)] // The harness drives the compiled binary.

mod support;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use support::environment::Command;

fn run(
    args: &[&str],
    version: Option<&str>,
    error: Option<serde_json::Value>,
    ping_stalls: bool,
) -> (std::process::Output, Vec<String>) {
    let dir = support::socket_dir::SocketDir::reserve("dc1683");
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
    let registry = dir
        .path()
        .join(support::paths::app_dir_name())
        .join("delegates")
        .join(support::paths::server_key(&socket));
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
    let mut version = version.map(str::to_owned);
    let mut protocol = match version.as_deref() {
        Some("current") => support::PROTOCOL_VERSION,
        Some("future") => support::PROTOCOL_VERSION + 1,
        _ => 25,
    };
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
                serde_json::json!({"id":request["id"],"result":{"type":"pong","version":version,"protocol":protocol,"capabilities":{"pane_input_write_ack":true,"live_handoff":false}}})
            } else if let Some(error) = &error {
                serde_json::json!({"id":request["id"],"error":error})
            } else if method == "agent.get" && request["params"]["target"] == "new-fixture" {
                serde_json::json!({"id":request["id"],"error":{"code":"agent_not_found","message":"no such agent"}})
            } else if method == "agent.get" {
                serde_json::json!({"id":request["id"],"result":{"agent":{
                    "terminal_id":"term_fixture", "pane_id":"w1:p1", "agent_status":"idle"
                }}})
            } else if method == "agent.send" && request["params"]["submit"] == true {
                serde_json::json!({"id":request["id"],"result":{"type":"guarded_submit",
                    "outcome":"accepted","reason":null,"retried":false}})
            } else {
                serde_json::json!({"id":request["id"],"result":{"text":"fixture reply"}})
            };
            if method == "server.live_handoff" {
                // A successful scripted handoff must produce the replacement
                // identity, rather than spend the real CLI's verification ceiling.
                version = request["params"]["expected_version"]
                    .as_str()
                    .map(str::to_owned);
                protocol = request["params"]["expected_protocol"]
                    .as_u64()
                    .map(|value| value as u32)
                    .unwrap_or(support::PROTOCOL_VERSION);
            }
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
    let input = (args.first() == Some(&"mcp")).then(|| {
        format!(
            "{}\n",
            serde_json::json!({
                "jsonrpc":"2.0", "id":1, "method":"tools/call",
                "params":{"name":"flock_agent_result", "arguments":{"target":"fixture"}}
            })
        )
        .into_bytes()
    });
    let output = support::command::output(command, input, support::command::CLI_LIMIT).unwrap();
    stopped.store(true, Ordering::SeqCst);
    (output, server.join().unwrap())
}

fn unknown_variant() -> serde_json::Value {
    serde_json::json!({"code":"invalid_request", "message":"unknown variant `agent.result`, expected one of `ping`, `agent.list`"})
}

#[test]
fn agent_result_success_has_no_diagnostic_ping() {
    let (output, methods) = run(
        &["agent", "result", "fixture"],
        Some("current"),
        None,
        false,
    );
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(methods, ["agent.result"]);
    assert!(output.stderr.is_empty());
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("fixture reply"));
}

fn gap_message(version: &str) -> String {
    format!("server flk {version} (protocol 25) is incompatible with client flk ")
}

fn assert_gap(output: std::process::Output, version: &str) {
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(78), "{stderr}");
    assert!(output.stdout.is_empty());
    assert!(stderr.contains(&gap_message(version)), "{stderr}");
    assert!(
        stderr.contains(&format!(
            " (protocol {}); hand it off with 'flk server live-handoff' or restart the server",
            support::PROTOCOL_VERSION
        )),
        "{stderr}"
    );
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
    assert_eq!(methods, ["agent.get", "ping"]);
}

/// The #868 shape: a worktree delegate against an older server used to
/// create the worktree, the workspace and the agent, then die at
/// `pane.submit` with a raw `unknown variant`. The refusal now lands on the
/// first state-changing request, before it is sent.
#[test]
fn delegate_start_in_a_worktree_refuses_skew_before_creating_it() {
    let (output, methods) = run(
        &[
            "delegate",
            "start",
            "new-fixture",
            "--brief",
            "BRIEF",
            "--worktree",
            "--branch",
            "chore/new-fixture",
            "--repo",
            "CWD",
            "--harness",
            "codex",
        ],
        Some("0.9.0"),
        None,
        false,
    );
    assert_gap(output, "0.9.0");
    assert_eq!(methods.iter().filter(|method| *method == "ping").count(), 1);
    for method in &methods {
        assert!(
            matches!(
                method.as_str(),
                "ping" | "agent.get" | "worktree.list" | "workspace.list"
            ),
            "{method} reached an incompatible server: {methods:?}"
        );
    }
}

#[test]
fn every_mutating_verb_refuses_skew_before_sending() {
    for args in [
        &["agent", "send", "fixture", "hello"][..],
        &["agent", "restart", "fixture", "--reason", "upgrade"],
        &["pane", "send-text", "w1:p1", "hello"],
        &["msg", "reply", "msg_fixture", "hello"],
        &["worktree", "create", "--cwd", "CWD", "--branch", "chore/x"],
        &["wait", "park", "--until", "reply:c1", "--deadline", "10m"],
    ] {
        let (output, methods) = run(args, Some("0.9.0"), None, false);
        assert_gap(output, "0.9.0");
        assert!(
            methods
                .iter()
                .all(|method| method == "ping" || method == "agent.get"),
            "{args:?} sent {methods:?}"
        );
    }
}

/// Right after a live handoff the shell's old client talks to a newer
/// server, and that must not be refused: only an OLDER server is.
#[test]
fn mutating_verbs_reach_an_equal_or_newer_server() {
    for version in ["current", "future"] {
        let (output, methods) = run(
            &["agent", "send", "fixture", "hello"],
            Some(version),
            None,
            false,
        );
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(output.status.code(), Some(0), "{version}: {stderr}");
        assert!(!stderr.contains("is incompatible with client"), "{stderr}");
        assert_eq!(methods, ["ping", "agent.send"], "{version}");
    }
}

#[test]
fn read_only_verbs_still_reach_an_older_server() {
    let (output, methods) = run(&["agent", "result", "fixture"], Some("0.9.0"), None, false);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(methods, ["agent.result"]);
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("fixture reply"));
}

#[test]
fn server_lifecycle_verbs_are_never_refused_for_skew() {
    for (args, method) in [
        (&["server", "live-handoff"][..], "server.live_handoff"),
        (&["server", "stop"], "server.stop"),
    ] {
        let (output, methods) = run(args, Some("0.9.0"), None, false);
        assert!(output.status.success(), "{args:?}: {output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(!stderr.contains("is incompatible with client"), "{stderr}");
        // live-handoff reads the server's version before and after (#958), so
        // its status pings are not a skew gate.
        let verbs: Vec<_> = methods.iter().filter(|sent| *sent != "ping").collect();
        assert_eq!(verbs, [method], "{args:?} sent {methods:?}");
    }
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
fn msg_send_refuses_skew_before_sending() {
    let (output, methods) = run(
        &["msg", "send", "fixture", "hello", "--intent", "fyi"],
        Some("0.10.0"),
        Some(serde_json::json!({"code":"invalid_request", "message":"unknown variant `msg.send`"})),
        false,
    );
    assert_gap(output, "0.10.0");
    assert_eq!(methods, ["ping"]);
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
            assert_eq!(methods, ["agent.result", "ping"]);
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
        assert_eq!(methods, ["agent.result", "ping"]);
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
    assert_eq!(methods, ["agent.result"]);
}

#[test]
fn delegate_success_has_no_diagnostic_ping() {
    let (output, methods) = run(&["delegate", "status", "fixture"], None, None, false);
    assert_eq!(output.status.code(), Some(0));
    // The screen read asks whether an idle turn left shells running (#911).
    assert_eq!(methods, ["agent.get", "agent.get", "pane.read"]);
    assert!(output.stderr.is_empty());
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
    assert_eq!(methods, ["agent.get", "agent.get", "ping"]);
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
        .contains(&gap_message("0.10.0")));
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
fn surfaced_lookup_errors_report_the_protocol_gap() {
    let (output, methods) = run(
        &["delegate", "status", "fixture"],
        Some("0.10.0"),
        Some(unknown_variant()),
        false,
    );
    assert_gap(output, "0.10.0");
    assert_eq!(methods, ["agent.get", "ping"]);
}

#[test]
fn equal_or_newer_protocol_keeps_original_invalid_request() {
    for version in ["current", "future"] {
        let (output, methods) = run(
            &["agent", "result", "fixture"],
            Some(version),
            Some(unknown_variant()),
            false,
        );
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("invalid_request"), "{stderr}");
        assert!(stderr.contains("unknown variant"), "{stderr}");
        assert!(!stderr.contains("is incompatible with client"), "{stderr}");
        assert_eq!(methods, ["agent.result", "ping"]);
    }
}

#[test]
fn agent_result_old_server_names_version_gap() {
    let (output, methods) = run(
        &["agent", "result", "fixture"],
        Some("0.6.8-fork.9ebb536"),
        Some(unknown_variant()),
        false,
    );
    assert_eq!(output.status.code(), Some(78));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("flk agent result needs a server ≥ 0.9.0 (running: 0.6.8-fork.9ebb536)"),
        "{stderr}"
    );
    assert_eq!(methods, ["agent.result", "ping"]);
}

#[test]
fn an_existing_ping_warns_once_for_an_older_server() {
    let (output, methods) = run(&["status", "server", "--json"], Some("0.10.0"), None, false);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(methods, ["ping"]);
    assert_eq!(
        String::from_utf8(output.stderr)
            .unwrap()
            .matches("warning:")
            .count(),
        1
    );
}

/// #1029: both events verbs answer an older server's unknown `events.read`
/// with the version gap (exit 78), the wait included before it waits.
#[test]
fn events_verbs_report_the_version_gap_against_an_older_server() {
    for args in [
        &["events", "read"][..],
        &[
            "events",
            "wait",
            "--kinds",
            "pane_exited",
            "--timeout",
            "5s",
        ],
    ] {
        let (output, methods) = run(
            args,
            Some("1.2.0"),
            Some(
                serde_json::json!({"code":"invalid_request", "message":"unknown variant `events.read`"}),
            ),
            false,
        );
        assert_gap(output, "1.2.0");
        assert_eq!(methods, ["events.read", "ping"], "{args:?}");
    }
}

/// #1155 / #868: each `flk goal` write is refused before it is sent to an
/// older server, under `--json` too, and `goal status` reaches it and turns
/// its unknown method into the version gap through `normalize`.
#[test]
fn goal_verbs_report_the_version_gap_against_an_older_server() {
    let writes = [
        &["goal", "arm", "ship it", "--check", "just check"][..],
        &["goal", "pause", "--minutes", "5", "--agent", "fixture"],
        &["goal", "resume", "--agent", "fixture"],
        &["goal", "stop", "--agent", "fixture"],
    ];
    for args in writes {
        let (output, methods) = run(args, Some("1.2.0"), None, false);
        assert_gap(output, "1.2.0");
        assert_eq!(methods, ["ping"], "{args:?} was sent");

        let json: Vec<_> = args.iter().copied().chain(["--json"]).collect();
        let (output, methods) = run(&json, Some("1.2.0"), None, false);
        assert_eq!(output.status.code(), Some(78), "{json:?}");
        let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            envelope["error"]["code"], "server_version_gap",
            "{envelope}"
        );
        assert_eq!(envelope["error"]["exit"], 78, "{envelope}");
        assert_eq!(methods, ["ping"], "{json:?} was sent");
    }
    let (output, methods) = run(
        &["goal", "status", "--agent", "fixture"],
        Some("1.2.0"),
        Some(
            serde_json::json!({"code":"invalid_request", "message":"unknown variant `goal.status`"}),
        ),
        false,
    );
    assert_gap(output, "1.2.0");
    assert_eq!(methods, ["goal.status", "ping"]);
}

/// The gate refuses only an older server: a current one is sent the write.
#[test]
fn goal_writes_reach_a_current_server() {
    let (output, methods) = run(
        &["goal", "stop", "--agent", "fixture"],
        Some("current"),
        None,
        false,
    );
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(methods, ["ping", "goal.stop"]);
}

/// A goal write the caller may not make is a policy refusal: exit 13, like
/// `operator_only` (ADR-0030 §1 amendment of 2026-10-13, ADR-0029 §2).
#[test]
fn a_refused_goal_caller_exits_13() {
    let (output, methods) = run(
        &["goal", "stop", "--agent", "fixture"],
        Some("current"),
        Some(serde_json::json!({"code":"goal_caller_refused", "message":"not your goal"})),
        false,
    );
    assert_eq!(output.status.code(), Some(13));
    assert_eq!(methods, ["ping", "goal.stop"]);
}
