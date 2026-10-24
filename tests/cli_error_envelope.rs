//! E2E (#1024): the error envelope of ADR-0029 §4 and the global `--format`
//! of §3, through the real `flk` binary against a scripted socket. Under
//! `--json` / `--format json` an error is one JSON document on stdout, with the
//! retry class and exit status from the registries, and one human line on
//! stderr. Without them nothing changes: stdout stays empty and stderr holds
//! the one line it always held.

// Integration tests drive the compiled binary through raw Command; the
// TracedCommand funnel polices flock's own subprocesses, not the harness's.
#![allow(clippy::disallowed_methods)]

mod support;

use std::path::PathBuf;
use std::process::Output;

use serde_json::{json, Value};
use support::scripted::{flk, scripted_server};

fn not_found(_: &Value) -> Value {
    json!({"error":{"code":"agent_not_found","message":"agent target nosuch not found"}})
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// stdout is exactly one JSON document (a trailing human line would fail to
/// parse), the exit status is `exit`, and the envelope says the same.
fn envelope(output: &Output, exit: i32) -> Value {
    let stderr = stderr(output);
    assert_eq!(output.status.code(), Some(exit), "stderr: {stderr}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("stdout is not one JSON document ({err}): {stdout:?}"));
    assert_eq!(value["error"]["exit"], exit, "{value}");
    value
}

/// The human line under machine output: exactly one stderr line naming the
/// code, besides any registered notice.
fn one_human_line(output: &Output, code: &str) {
    let stderr = stderr(output);
    let lines: Vec<&str> = stderr
        .lines()
        .filter(|line| !line.starts_with("note: exit code"))
        .collect();
    assert_eq!(lines.len(), 1, "stderr: {stderr}");
    assert!(
        lines[0].starts_with(&format!("error: {code}: ")),
        "{stderr}"
    );
}

/// The issue's acceptance: `flk agent get nosuch --json` parses, and exits with
/// the code the registries assign `agent_not_found` on dev (1: the not-found
/// mapping to 4 of ADR-0029 §2 is not in the registry yet).
#[test]
fn agent_get_nosuch_json_is_an_envelope_on_stdout() {
    let (socket, _server) = scripted_server("envelope-agent", not_found);
    let output = flk(&socket, &["agent", "get", "nosuch", "--json"]);
    let value = envelope(&output, 1);
    assert_eq!(
        value["error"],
        json!({"code": "agent_not_found", "message": "agent target nosuch not found",
               "retry": "never", "exit": 1}),
        "{value}"
    );
    assert_eq!(value["id"], "cli:agent:get");
    one_human_line(&output, "agent_not_found");
}

/// Without machine output: nothing on stdout, and the one stderr line the
/// verb always printed.
#[test]
fn plain_agent_get_nosuch_prints_one_line_on_stderr() {
    let (socket, _server) = scripted_server("envelope-plain", not_found);
    for args in [
        &["agent", "get", "nosuch"][..],
        &["agent", "get", "nosuch", "--format", "text"],
    ] {
        let output = flk(&socket, args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        let stderr = stderr(&output);
        assert_eq!(stderr.lines().count(), 1, "{args:?}: {stderr}");
        let line: Value = serde_json::from_str(stderr.trim()).unwrap();
        assert_eq!(line["error"]["code"], "agent_not_found");
        assert!(line["error"].get("exit").is_none(), "unchanged: {line}");
    }
}

/// `--format json` is the same flag as `--json`, before or after the verb.
#[test]
fn format_json_is_global_in_any_position() {
    let (socket, _server) = scripted_server("envelope-format", not_found);
    for args in [
        &["--format", "json", "agent", "get", "nosuch"][..],
        &["agent", "get", "--format=json", "nosuch"],
        &["--json", "agent", "get", "nosuch"],
    ] {
        let output = flk(&socket, args);
        assert_eq!(envelope(&output, 1)["error"]["code"], "agent_not_found");
    }
    let bad = flk(&socket, &["agent", "get", "nosuch", "--format", "yaml"]);
    assert_eq!(bad.status.code(), Some(2));
    assert!(bad.stdout.is_empty());
}

/// `pane read` prints the bare `error` object without machine output; under
/// `--json` it is the envelope, and its own `--format text` stays its own.
#[test]
fn pane_read_error_is_an_envelope() {
    fn gone(_: &Value) -> Value {
        json!({"error":{"code":"pane_not_found","message":"no pane 9-9"}})
    }
    let (socket, _server) = scripted_server("envelope-pane", gone);
    let output = flk(
        &socket,
        &["pane", "read", "9-9", "--format", "text", "--json"],
    );
    let value = envelope(&output, 1);
    assert_eq!(value["error"]["code"], "pane_not_found");
    assert_eq!(value["error"]["retry"], "never");
    one_human_line(&output, "pane_not_found");

    let plain = flk(&socket, &["pane", "read", "9-9"]);
    assert!(plain.stdout.is_empty());
    assert_eq!(
        serde_json::from_slice::<Value>(&plain.stderr).unwrap(),
        json!({"code": "pane_not_found", "message": "no pane 9-9"})
    );
}

/// A guarded verb's refusal: exit 10 with the `later` class, the moved-code
/// notice on stderr and never on stdout.
#[test]
fn a_guarded_pane_refusal_is_an_envelope_with_exit_10() {
    fn refuse(_: &Value) -> Value {
        json!({"error":{"code":"not_settled","message":"agent is working"}})
    }
    let (socket, _server) = scripted_server("envelope-refused", refuse);
    let output = flk(
        &socket,
        &["--json", "pane", "run", "1-1", "--if-input-empty", "hi"],
    );
    let value = envelope(&output, 10);
    assert_eq!(value["error"]["retry"], "later");
    one_human_line(&output, "not_settled");
    assert!(stderr(&output).contains("note: exit code is now 10 (was 1)"));
}

/// `msg send` and its `--await` form: a refused send is the envelope.
#[test]
fn msg_send_error_is_an_envelope() {
    fn unknown(_: &Value) -> Value {
        json!({"error":{"code":"msg_target_not_found","message":"no agent w9"}})
    }
    let (socket, _server) = scripted_server("envelope-msg", unknown);
    for args in [
        &["msg", "send", "w9", "hi", "--json"][..],
        &[
            "msg",
            "send",
            "w9",
            "hi",
            "--intent",
            "needs-reply",
            "--await",
            "--json",
        ],
    ] {
        let output = flk(&socket, args);
        let value = envelope(&output, 1);
        assert_eq!(value["error"]["code"], "msg_target_not_found", "{args:?}");
        assert_eq!(value["error"]["retry"], "never", "{args:?}");
        one_human_line(&output, "msg_target_not_found");
    }
}

/// Server target details survive the CLI envelope without parsing its wording.
#[test]
fn message_ambiguity_candidates_survive_cli_json() {
    fn ambiguous(_: &Value) -> Value {
        json!({"error":{"code":"msg_target_ambiguous","message":"choose a recipient",
            "data":{"kind":"msg_target_ambiguous","expected":[{"type":"pane","pane":"<pane>"}],
            "candidates":[{"terminal_id":"terminal-fixture","pane_id":"w1:p1",
                "workspace_id":"w1","tab_id":"w1:t1","cwd":null,"agent_status":"idle"}]}}})
    }
    let (socket, _server) = scripted_server("envelope-candidates", ambiguous);
    let output = flk(&socket, &["msg", "send", "reviewer", "hi", "--json"]);
    let value = envelope(&output, 1);
    assert_eq!(
        value["error"]["data"],
        ambiguous(&Value::Null)["error"]["data"]
    );
    assert_eq!(value["error"]["exit"], 1);
    assert_eq!(value["error"]["retry"], "never");
    one_human_line(&output, "msg_target_ambiguous");
}

/// A registered delegate is identified by its durable id, not a coincidentally equal name.
#[test]
fn draft_refusal_guides_registered_delegates_after_manual_resolution() {
    fn refused(_: &Value) -> Value {
        json!({"error":{"code":"input_not_empty",
            "message":"clear or submit the existing draft in the agent UI, then retry",
            "data":{"agent_id":"agent_nodea_123abc","terminal_id":"term_fixture","pane_id":"w1:p1",
                "composer":{"visible_bytes":17},"remedy":{"retry_when":"composer_empty"}}}})
    }
    let (socket, _server) = scripted_server("draft-delegate", refused);
    let state = support::socket_dir::SocketDir::reserve("draft-state");
    let registry = state
        .path()
        .join(support::paths::app_dir_name())
        .join("delegates")
        .join(support::paths::server_key(&socket));
    std::fs::create_dir_all(&registry).unwrap();
    let mut record = json!({"name":"reviewer","agent_id":"agent_nodea_123abc",
        "terminal_id":"term_old","pane_id":"w9:p9","root_pane":"w9:p1","workspace_id":"w9",
        "mode":"cwd","harness":"codex","round":1,"brief":"fixture-brief","submitted_at_ms":1,
        "cursor":"0","created_at_ms":1});
    for (agent_id, terminal_id, registered) in [
        (Some("agent_nodea_123abc"), "term_old", true),
        (Some("agent_nodeb_123abc"), "term_fixture", false),
        (None, "term_fixture", true),
        (None, "term_old", false),
    ] {
        record["agent_id"] = json!(agent_id);
        record["terminal_id"] = json!(terminal_id);
        std::fs::write(registry.join("reviewer.json"), record.to_string()).unwrap();
        let mut command = support::environment::Command::new(env!("CARGO_BIN_EXE_flk"));
        command
            .args([
                "agent",
                "send",
                "--json",
                "--no-submit",
                "reviewer",
                "extra",
            ])
            .env("FLOCK_SOCKET_PATH", &socket)
            .env_remove("FLOCK_CLIENT_SOCKET_PATH")
            .env_remove("FLOCK_ENV")
            .env("XDG_STATE_HOME", state.path());
        let output = support::scripted::output(command);
        let value = envelope(&output, 10);
        assert_eq!(value["error"]["code"], "input_not_empty");
        assert_eq!(value["error"]["data"]["composer"]["visible_bytes"], 17);
        assert!(stderr(&output).contains("agent UI"));
        let mut command = support::environment::Command::new(env!("CARGO_BIN_EXE_flk"));
        command
            .args(["agent", "send", "--no-submit", "reviewer", "extra"])
            .env("FLOCK_SOCKET_PATH", &socket)
            .env_remove("FLOCK_CLIENT_SOCKET_PATH")
            .env_remove("FLOCK_ENV")
            .env("XDG_STATE_HOME", state.path());
        let plain = support::scripted::output(command);
        assert_eq!(plain.status.code(), Some(10));
        assert!(plain.stdout.is_empty());
        assert!(stderr(&plain).contains("agent UI"));
        assert_eq!(
            stderr(&plain).contains("flk delegate send reviewer --brief FILE"),
            registered
        );
        if registered {
            assert!(
                stderr(&output).contains("flk delegate send reviewer --brief FILE"),
                "{}",
                stderr(&output)
            );
            assert!(stderr(&output).contains("also refuses drafts"));
            assert_eq!(
                value["error"]["data"]["remedy"]["next_round"]["argv"],
                json!(["flk", "delegate", "send", "reviewer", "--brief", "FILE"])
            );
            assert_eq!(
                value["error"]["data"]["remedy"]["next_round"]["requires"],
                "composer_empty"
            );
        } else {
            assert!(!stderr(&output).contains("delegate send"));
            assert!(value["error"]["data"]["remedy"].get("next_round").is_none());
        }
    }
}

/// A delegate usage error (`not a delegate`) is `invalid_params`, exit 2.
#[test]
fn delegate_not_a_delegate_is_an_envelope() {
    fn unused(_: &Value) -> Value {
        json!({"error":{"code":"internal_error","message":"unexpected call"}})
    }
    let (socket, _server) = scripted_server("envelope-delegate", unused);
    let output = flk(&socket, &["delegate", "status", "nosuch", "--json"]);
    let value = envelope(&output, 2);
    assert_eq!(value["error"]["code"], "invalid_params");
    assert_eq!(value["error"]["message"], "not a delegate: nosuch");
    assert_eq!(value["error"]["retry"], "never");
    one_human_line(&output, "invalid_params");

    let plain = flk(&socket, &["delegate", "status", "nosuch"]);
    assert_eq!(plain.status.code(), Some(2));
    assert!(plain.stdout.is_empty());
    assert_eq!(stderr(&plain).trim(), "not a delegate: nosuch");
}

/// No server at all: the io error is a `transport` envelope, exit 1.
#[test]
fn an_unreachable_server_is_a_transport_envelope() {
    let socket = PathBuf::from(format!("/tmp/flk1024-none-{}.sock", std::process::id()));
    let output = flk(&socket, &["agent", "get", "w1", "--json"]);
    let value = envelope(&output, 1);
    assert_eq!(value["error"]["code"], "transport", "{value}");
    // A client-side failure answers no request: its `id` is null, not absent.
    assert_eq!(value.get("id"), Some(&Value::Null), "{value}");
    assert_eq!(value["error"]["retry"], "maybe");
    one_human_line(&output, "transport");
}

/// An effect failure is a result: under `--json` it is the stdout document.
#[test]
fn an_unconfirmed_send_is_the_stdout_document_under_json() {
    fn unconfirmed(_: &Value) -> Value {
        json!({"result":{"type":"paste","outcome":"unconfirmed","reason":"confirm_timeout","evidence":{}}})
    }
    let (socket, _server) = scripted_server("envelope-effect", unconfirmed);
    let output = flk(
        &socket,
        &["agent", "send", "--json", "--no-submit", "w1", "hi"],
    );
    assert_eq!(output.status.code(), Some(8), "{}", stderr(&output));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["result"]["outcome"], "unconfirmed");

    let plain = flk(&socket, &["agent", "send", "--no-submit", "w1", "hi"]);
    assert_eq!(plain.status.code(), Some(8));
    assert!(plain.stdout.is_empty());
}

/// Answers every request with an error whose message is the request's params,
/// so a test reads back exactly what the verb sent.
fn echo(request: &Value) -> Value {
    json!({"error":{"code":"invalid_params","message": request["params"].to_string()}})
}

/// The params the verb sent, from the one stderr line without machine output.
fn sent(output: &Output) -> Value {
    let stderr = stderr(output);
    let line: Value = serde_json::from_str(stderr.lines().last().unwrap_or_default())
        .unwrap_or_else(|err| panic!("{err}: {stderr}"));
    serde_json::from_str(line["error"]["message"].as_str().unwrap()).unwrap()
}

/// The global flags are never looked for in a verb's free text: these words
/// reach the pane verbatim, and the output mode stays the default.
#[test]
fn global_flags_inside_typed_text_pass_through_verbatim() {
    let (socket, _server) = scripted_server("envelope-freetext", echo);
    for (args, text) in [
        (
            &["pane", "run", "1-1", "git", "log", "--format=%H"][..],
            "git log --format=%H",
        ),
        (
            &[
                "pane", "run", "1-1", "gh", "pr", "view", "12", "--json", "state",
            ],
            "gh pr view 12 --json state",
        ),
        (
            &[
                "pane",
                "run",
                "1-1",
                "--if-input-empty",
                "ls",
                "--format",
                "text",
            ],
            "ls --format text",
        ),
        (
            &["pane", "send-text", "1-1", "--json", "state"],
            "--json state",
        ),
        (&["pane", "send-text", "1-1", "--format=%H"], "--format=%H"),
        (
            &["agent", "send", "w1", "hi", "--format=%H", "--json"],
            "hi --format=%H --json",
        ),
        (
            &["agent", "send", "--no-submit", "w1", "--json", "x"],
            "--json x",
        ),
    ] {
        let output = flk(&socket, args);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{args:?}: {}",
            stderr(&output)
        );
        assert!(output.stdout.is_empty(), "{args:?}: output mode changed");
        assert_eq!(sent(&output)["text"], text, "{args:?}");
    }
}

/// Before the verb's free text the flags are global: before the verb, or in
/// the verb's flag position.
#[test]
fn global_flags_before_the_free_text_switch_the_output() {
    let (socket, _server) = scripted_server("envelope-flagpos", echo);
    for (args, text) in [
        (
            &["--json", "pane", "run", "1-1", "ls", "--json"][..],
            "ls --json",
        ),
        (&["pane", "--format", "json", "send-text", "1-1", "a"], "a"),
        (
            &["pane", "run", "--json", "1-1", "--if-input-empty", "b"],
            "b",
        ),
    ] {
        let output = flk(&socket, args);
        let value = envelope(&output, 1);
        let params: Value =
            serde_json::from_str(value["error"]["message"].as_str().unwrap()).unwrap();
        assert_eq!(params["text"], text, "{args:?}");
    }
}

/// `--` keeps its documented meaning: everything after it is left alone, and
/// for `pane run --if-input-empty` it still ends the guard's flags.
#[test]
fn a_double_dash_still_ends_the_flags() {
    let (socket, _server) = scripted_server("envelope-dashdash", echo);
    let guarded = flk(
        &socket,
        &[
            "pane",
            "run",
            "--if-input-empty",
            "--",
            "1-1",
            "--json",
            "x",
        ],
    );
    assert!(guarded.stdout.is_empty());
    assert_eq!(sent(&guarded)["text"], "--json x");
    let agent = flk(&socket, &["agent", "send", "--", "w1", "--format", "json"]);
    assert!(agent.stdout.is_empty());
    assert_eq!(sent(&agent)["text"], "--format json");
}

/// An older server: the verb's error becomes `server_version_gap` and the
/// process exits 78. The one envelope says 78 too, because it prints last.
#[test]
fn the_skew_envelope_says_the_exit_the_process_returns() {
    fn old_server(request: &Value) -> Value {
        if request["method"] == "ping" {
            return json!({"result":{"type":"pong","version":"0.1.0","protocol":1}});
        }
        json!({"error":{"code":"unknown_method","message":"unknown method"}})
    }
    let (socket, _server) = scripted_server("envelope-skew", old_server);
    let output = flk(&socket, &["agent", "get", "w1", "--json"]);
    let value = envelope(&output, 78);
    assert_eq!(value["error"]["code"], "server_version_gap", "{value}");
    assert_eq!(value["error"]["retry"], "after_handoff");
}

/// A protocol-32 client refuses a write to a protocol-31 server before sending
/// it, preserving the typed handoff recovery path across the snapshot bump.
#[test]
fn protocol_32_client_refuses_protocol_31_server_with_handoff_recovery() {
    fn old_server(request: &Value) -> Value {
        assert_eq!(
            request["method"], "ping",
            "no mutation reaches the old server"
        );
        json!({"result":{"type":"pong","version":"3.1.0","protocol":31}})
    }
    let (socket, _server) = scripted_server("snapshot-skew", old_server);
    let output = flk(
        &socket,
        &[
            "--format", "json", "agent", "send", "--submit", "w1", "fixture",
        ],
    );
    let value = envelope(&output, 78);
    assert_eq!(value["error"]["code"], "server_version_gap");
    assert_eq!(value["error"]["retry"], "after_handoff");
}

/// #1031: a park the server refuses for its predicate exits 2 (a usage
/// error: fix the call), and one refused for the agent's open parks exits 10
/// (refused before acting). A park that succeeds prints one human line, or
/// the result alone under `--json`.
#[test]
fn wait_park_exits_by_its_refusal_and_prints_its_wait() {
    fn unsupported(_: &Value) -> Value {
        json!({"error":{"code":"wait_predicate_unsupported","message":"not yet"}})
    }
    fn too_many(_: &Value) -> Value {
        json!({"error":{"code":"too_many_waits","message":"16 open"}})
    }
    fn parked(request: &Value) -> Value {
        if request["method"] == "wait.park" {
            assert_eq!(request["params"]["until"], "pr:7 ci_completed");
        }
        json!({"result":{"type":"wait_parked","wait_id":"w_1_1","agent_id":"agent_a",
            "predicate":"reply:c1","deadline":5}})
    }
    let park = [
        "--json",
        "wait",
        "park",
        "--until",
        "pr:7 ci_completed",
        "--deadline",
        "5m",
    ];
    let (socket, _server) = scripted_server("park-unsupported", unsupported);
    let output = flk(&socket, &park);
    assert_eq!(envelope(&output, 2)["error"]["retry"], "never");
    one_human_line(&output, "wait_predicate_unsupported");
    let (socket, _server) = scripted_server("park-too-many", too_many);
    assert_eq!(
        envelope(&flk(&socket, &park), 10)["error"]["retry"],
        "later"
    );

    let (socket, _server) = scripted_server("park-ok", parked);
    let output = flk(&socket, &park[1..]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "parked w_1_1 on reply:c1 until 5 (Unix ms)\n"
    );
    let output = flk(&socket, &park);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["wait_id"], "w_1_1");
}

/// Restart drop reports survive both CLI output forms.
#[test]
fn restart_optional_1109_drop_report_schema_and_cli() {
    fn answer(_: &Value) -> Value {
        json!({"result":{"type":"agent_restart_queued","pane_id":"fixture-pane","session":"fixture-session","stop_only":false,"dropped_flags":[{"flag":"--worktree","reason":"existing cwd is preserved"},{"flag":"--from-pr","reason":"native resume selector determines session"}]}})
    }
    let (socket, _server) = scripted_server("restart-drop-report", answer);
    for args in [
        vec!["agent", "restart", "fixture-worker", "--reason", "reload"],
        vec![
            "--json",
            "agent",
            "restart",
            "fixture-worker",
            "--reason",
            "reload",
        ],
    ] {
        let output = flk(&socket, &args);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            response["result"]["dropped_flags"],
            answer(&Value::Null)["result"]["dropped_flags"]
        );
        assert!(output.stderr.is_empty());
    }
}
