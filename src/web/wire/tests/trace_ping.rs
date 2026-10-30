//! Every heartbeat-script step, plus proof that the actual endpoint consumed the pong.
use super::*;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
#[test]
fn ping_script_checks_server_nonce_real_pong_and_causal_controls() {
    runtime(async {
        let script = crate::structured_wire::fixture::FixtureLine::parse_script(include_str!(
            "../../../../web/client/test/fixtures/wire/ping-pong.jsonl"
        ))
        .unwrap();
        for (control, failure) in [
            ("nonce", "step 7 (send): frame.nonce"),
            ("pong", "step 8 (expect): no client frame; expected pong"),
            ("", ""),
        ] {
            let server = Server::with_setup(
                |clock| Arc::new(clock),
                |core| {
                    Arc::get_mut(&mut core.shared).unwrap().epoch =
                        "00000000000000000000000000000001".into();
                    core.shared
                        .next
                        .store(if control == "nonce" { 2 } else { 1 }, Ordering::SeqCst);
                },
            )
            .await;
            let path = server._root.path().join("client-tokens/one.token");
            let mut token: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            token["principal"] = json!("0123456789abcdef0123456789abcdef");
            token["scopes"] = json!(["read", "send"]);
            fs::write(path, token.to_string()).unwrap();
            let mut child = tokio::process::Command::new("bun")
                .arg(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/web/client/tests/wire/endpoint-ping.ts"
                ))
                .arg(format!("ws://{}/v1/ws", server.bind))
                .arg(env!("CARGO_PKG_VERSION"))
                .arg(control)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap())
                .env("HOME", server._root.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let mut input = child.stdin.take().unwrap();
            let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
            let mut stage = 0;
            let mut facts = None;
            let output = within(async {
                while let Some(line) = lines.next_line().await.unwrap() {
                    assert!(!server.task.is_finished(), "server exited during trace");
                    let value: Value = serde_json::from_str(&line).unwrap();
                    match (stage, value["op"].as_str()) {
                        (0, Some("ping")) => {
                            server.clock.deadline(15_000).await;
                            server.clock.advance(15_000);
                        }
                        (1, Some("pong")) => {
                            // Only the matching real pong renews expiry from 45s to 60s.
                            server.clock.deadline(60_000).await;
                        }
                        (2, None) if value.is_array() => {
                            facts = Some(value);
                            stage = 3;
                            continue;
                        }
                        _ => panic!("unexpected clock stage {stage}: {value}"),
                    }
                    stage += 1;
                    input.write_all(b"ok\n").await.unwrap();
                }
                child.wait_with_output().await.unwrap()
            })
            .await;
            let error = String::from_utf8_lossy(&output.stderr);
            if control.is_empty() {
                assert!(output.status.success(), "{error}");
                assert_eq!(stage, 3, "clock and pong proof must complete");
                let facts = facts.unwrap();
                let facts = facts.as_array().unwrap();
                assert_eq!(facts.len(), script.len());
                for (index, (fact, step)) in facts.iter().zip(&script).enumerate() {
                    let step = serde_json::to_value(step).unwrap();
                    assert_eq!(fact["index"], index);
                    assert_eq!(fact["checked"], true);
                    assert_eq!(fact["step"], step["step"]);
                    assert_eq!(
                        fact["source"],
                        match step["step"].as_str().unwrap() {
                            "given" => "setup",
                            "client" | "open" => "client",
                            "send" => "server",
                            "quiet" => "clock",
                            _ => "observation",
                        }
                    );
                }
            } else {
                assert_eq!(stage, 1);
                assert!(facts.is_none());
                assert_eq!(output.status.code(), Some(1), "{control}: {error}");
                assert!(
                    error.contains(&format!("error: ping-pong: {failure}")),
                    "{control}: {error}"
                );
            }
            server.finish().await;
        }
    });
}
