//! The complete token-revoked script through real credential and command admission.
use super::*;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
#[test]
fn revocation_script_checks_ack_close_outbox_and_causal_controls() {
    runtime(async {
        let script = crate::structured_wire::fixture::FixtureLine::parse_script(include_str!(
            "../../../../web/client/test/fixtures/wire/token-revoked.jsonl"
        ))
        .unwrap();
        for (control, failure) in [
            ("valid", "step 9 (send): frame.error.code"),
            ("close", "step 10 (token_refused): actual endpoint close"),
            (
                "outbox",
                "step 11 (observe): command c1 is sent, expected failed",
            ),
            ("", ""),
        ] {
            let mut server = Server::with_setup(
                |clock| Arc::new(clock),
                |core| {
                    Arc::get_mut(&mut core.shared).unwrap().epoch =
                        "00000000000000000000000000000001".into()
                },
            )
            .await;
            let path = server._root.path().join("client-tokens/one.token");
            let mut token: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            token["principal"] = json!("0123456789abcdef0123456789abcdef");
            fs::write(&path, token.to_string()).unwrap();
            let (_, rx) = mpsc::unbounded_channel();
            let mut app = crate::app::App::new(
                &crate::config::Config::default(),
                true,
                None,
                rx,
                crate::api::EventHub::default(),
            );
            let mut child = tokio::process::Command::new("bun")
                .arg(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/web/client/tests/wire/endpoint-revocation.ts"
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
            let wake = server.wake.clone();
            let output = within(async {
                loop {
                    let changed = wake.notified();
                    tokio::pin!(changed);
                    changed.as_mut().enable();
                    assert!(!server.task.is_finished(), "server exited during trace");
                    Arc::get_mut(&mut server.core)
                        .unwrap()
                        .service_commands(&mut app);
                    tokio::select! { biased;
                        line = lines.next_line() => {
                            let Some(line) = line.unwrap() else { break };
                            let value: Value = serde_json::from_str(&line).unwrap();
                            match (stage, value["op"].as_str()) {
                                (0, Some("revoke")) => {
                                    if control != "valid" { fs::remove_file(&path).unwrap(); }
                                    input.write_all(b"ok\n").await.unwrap();
                                }
                                (1, None) if value.is_array() => facts = Some(value),
                                _ => panic!("unexpected revocation stage {stage}: {value}"),
                            }
                            stage += 1;
                        }
                        _ = &mut changed => (),
                    }
                }
                child.wait_with_output().await.unwrap()
            })
            .await;
            assert_eq!(
                path.exists(),
                control == "valid",
                "actual credential mutation"
            );
            let error = String::from_utf8_lossy(&output.stderr);
            if control.is_empty() {
                assert!(output.status.success(), "{error}");
                assert_eq!(stage, 2);
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
                    error.contains(&format!("error: token-revoked: {failure}")),
                    "{control}: {error}"
                );
            }
            server.finish().await;
        }
    });
}
