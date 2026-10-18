//! ADR-0029 §§1–2: the real CLI preserves uncertain/abandoned evidence and exits.

mod support;

// Bin-only flock shares only numeric constants, without the registry's tests.
#[allow(dead_code)] // This test needs only the uncertain and abandoned exits.
#[path = "../src/cli/exit/constants.rs"]
mod exit;

use serde_json::{json, Value};
use std::time::Duration;
use support::socket_dir::SocketDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

const CEILING: Duration = Duration::from_secs(10);

/// Exactly ping, then agent.send. EOF, malformed requests and extra stages fail.
/// The outer ceiling bounds accept, line reads, writes and CLI completion alike.
async fn peer(listener: UnixListener, response: Value) {
    for method in ["ping", "agent.send"] {
        let (stream, _) = listener.accept().await.expect("CLI connects");
        let mut stream = BufReader::new(stream);
        let mut line = String::new();
        assert!(
            stream.read_line(&mut line).await.expect("request line") > 0,
            "CLI disconnected before {method}"
        );
        let request: Value = serde_json::from_str(&line).expect("JSON request");
        assert_eq!(request["method"], method, "unexpected peer stage");
        let mut reply = if method == "ping" {
            json!({"result":{"type":"pong","version":"fixture","protocol":support::PROTOCOL_VERSION,"capabilities":{"pane_input_write_ack":true,"live_handoff":false}}})
        } else {
            assert_eq!(request["params"]["target"], "w1:p1");
            assert_eq!(request["params"]["text"], "failure prompt");
            assert_eq!(request["params"]["submit"], true, "send submits by default");
            response.clone()
        };
        reply["id"] = request["id"].clone();
        stream
            .get_mut()
            .write_all(format!("{reply}\n").as_bytes())
            .await
            .expect("scripted reply");
    }
}

#[tokio::test]
async fn guarded_submit_cli_reports_exit_8_and_9_with_scripted_evidence() {
    for (code, outcome, reason, retried) in [
        (
            exit::UNCONFIRMED,
            "unconfirmed",
            "typed_not_submitted",
            true,
        ),
        (exit::ABANDONED, "abandoned", "operator_active", false),
    ] {
        let dir = SocketDir::reserve("gs");
        let socket = dir.path().join("s");
        let listener = UnixListener::bind(&socket).expect("isolated peer socket");
        let response = json!({"id":"cli:agent:send","result":{
            "type":"guarded_submit", "pane_id":"w1:p1", "outcome":outcome, "reason":reason, "retried":retried,
            "attempt":{
                "attempt_id":format!("scripted-{code}"), "pane":"w1:p1", "correlation_ids":[],
                "wake":false, "state":outcome, "reason":reason, "queued_at_ms":1,
                "typed_at_ms":2, "submit_sent_at_ms":if retried {Some(3)} else {None},
                "finished_at_ms":4, "retried":retried
            }
        }});
        let mut command = support::environment::Command::new(env!("CARGO_BIN_EXE_flk"));
        command
            .args(["agent", "send", "w1:p1", "failure prompt"])
            .env("FLOCK_SOCKET_PATH", &socket);
        // Async process ownership kills this child if the ceiling drops output().
        let mut command = tokio::process::Command::from(command.into_std());
        command.kill_on_drop(true);
        let (_, output) = tokio::time::timeout(CEILING, async {
            tokio::join!(peer(listener, response.clone()), command.output())
        })
        .await
        .expect("scripted guarded submit exceeded its failure ceiling");
        let output = output.expect("real flk CLI");
        assert_eq!(
            output.status.code(),
            Some(code),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stdout.is_empty(),
            "default uncertain effects use stderr"
        );
        let actual: Value = serde_json::from_slice(&output.stderr).expect("effect evidence JSON");
        assert_eq!(actual["result"]["outcome"], outcome);
        assert_eq!(actual["result"]["reason"], reason);
        assert!(actual["result"]["attempt"]["attempt_id"].is_string());
        assert_eq!(
            actual, response,
            "the CLI must preserve every evidence field"
        );
    }
}
