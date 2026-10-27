//! Actual endpoint plus real client execution of recorded-agent script traces.
use super::*;
use crate::structured::transport::{RecordOrigin, StreamRecord};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

async fn replay(name: &str, seq: u64) {
    let record = StreamRecord {
        seq,
        known: Some(true),
        shape: None,
        received_at_ms: 1,
        origin: RecordOrigin::Claude,
        result_subtype: None,
        permission_grant_request_id: None,
        permission_request_seq: None,
        raw: json!({"type":"assistant","message":{}}).to_string(),
    };
    let mut case = super::subscriptions::Case::named("fixture", vec![record]).await;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = root.join(format!("web/client/test/fixtures/wire/{name}.jsonl"));
    let authored: Vec<Value> = fs::read_to_string(&script)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let scopes = &authored
        .iter()
        .find(|line| line["frame"]["t"] == "welcome")
        .unwrap()["frame"]["capabilities"];
    let path = case.server._root.path().join("client-tokens/one.token");
    let mut token: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    token["scopes"] = scopes.clone();
    token["principal"] = json!("0123456789abcdef0123456789abcdef");
    fs::write(path, token.to_string()).unwrap();
    assert_eq!(
        TokenStore::open(case.server._root.path())
            .unwrap()
            .authenticate(&Bearer("a".repeat(64)))
            .unwrap()
            .info
            .principal,
        "0123456789abcdef0123456789abcdef"
    );
    let dropped = std::env::var("S7_TRACE_DROP_STEP").unwrap_or_default(); // guardrails-ok(hermetic): explicit test-only fail-first control for #2467
    let mut child = tokio::process::Command::new("bun")
        .arg(root.join("web/client/tests/wire/endpoint-client.ts"))
        .arg(format!("ws://{}/v1/ws", case.server.bind))
        .arg(&script)
        .arg("a".repeat(64))
        .arg(dropped)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").expect("fixture tools"))
        .env("HOME", case.server._root.path())
        .kill_on_drop(true)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("Bun actual-endpoint trace client");
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut input = child.stdin.take().unwrap();
    let wake = case.server.wake.clone();
    let facts = within(async {
        loop {
            let notified = wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            case.service();
            tokio::select! { biased;
                line = output.next_line() => {
                    let line = line.unwrap().expect("trace client exited before coverage completion");
                    let value: Value = serde_json::from_str(&line).unwrap();
                    if let Some(ms) = value["advance"].as_u64() {
                        case.server.clock.advance(ms);
                        input.write_all(format!("{}\n", json!({"advanced":ms})).as_bytes()).await.unwrap();
                        input.flush().await.unwrap();
                    } else {
                        assert_eq!(value["complete"], true, "unexpected trace control: {value}");
                        break value["facts"].as_array().unwrap().clone();
                    }
                }
                _ = &mut notified => (),
            }
        }
    }).await;
    drop(input);
    assert!(within(child.wait()).await.unwrap().success());
    let lines: Vec<Value> = fs::read_to_string(script)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(facts.len(), lines.len());
    for (index, (fact, line)) in facts.iter().zip(&lines).enumerate() {
        assert_eq!(fact["index"], index);
        assert_eq!(fact["step"], line["step"]);
        assert_eq!(fact["checked"], true);
        if line["step"] == "send" {
            assert_eq!(fact["source"], "server");
        }
    }
    case.server.finish().await;
}

#[test]
fn clean_script_executes_every_step_against_endpoint_and_real_client() {
    runtime(replay("clean", 1));
}

#[test]
fn agent_gap_script_executes_every_step_against_endpoint_and_real_client() {
    runtime(replay("agent-gap", 5));
}
