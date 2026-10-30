//! A cold retained file drives the complete script through owner lookup and the client.
use super::*;
use std::io::Write;
#[test]
fn gap_script_checks_cold_owner_install_replay_cursor_and_causal_controls() {
    runtime(async {
        let script = crate::structured_wire::fixture::FixtureLine::parse_script(include_str!(
            "../../../../../web/client/test/fixtures/wire/agent-gap.jsonl"
        ))
        .unwrap();
        for (control, failure) in [
            ("gap", "step 9 (send): frame.to"),
            ("stamp", "step 10 (send): frame.ts_ms"),
            (
                "cursor",
                "step 12 (observe): stream s1 is Current@4, expected Current@5",
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
            let token_path = server._root.path().join("client-tokens/one.token");
            let mut token: Value = serde_json::from_slice(&fs::read(&token_path).unwrap()).unwrap();
            token["principal"] = json!("0123456789abcdef0123456789abcdef");
            fs::write(token_path, token.to_string()).unwrap();
            let mut registry = Registry::default();
            registry
                .register(Record {
                    runtime: StructuredRuntime::Structured,
                    agent_id: "fixture".into(),
                    session_id: "history".into(),
                    name: None,
                    harness: "claude".into(),
                    cwd: server._root.path().into(),
                    model: None,
                })
                .unwrap();
            let mut workers = Workers::new(server.wake.clone());
            let sessions = server._root.path().join("sessions");
            crate::platform::structured::ensure_private_dir(&sessions).unwrap();
            let directory = sessions.join("fixture");
            crate::platform::structured::ensure_private_dir(&directory).unwrap();
            let path = directory.join("history.jsonl");
            let mut file = crate::platform::structured::create_private_file(&path).unwrap();
            let mut stored = record(if control == "gap" { 4 } else { 5 });
            stored.received_at_ms = if control == "stamp" { 2 } else { 1 };
            stored.raw = json!({"type":"assistant","message":{}}).to_string();
            writeln!(file, "{}", serde_json::to_string(&stored).unwrap()).unwrap();
            file.sync_all().unwrap();
            drop(file);
            assert!(
                workers.history(&registry, "fixture").is_none(),
                "must start cold"
            );
            let child = tokio::process::Command::new("bun")
                .arg(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/web/client/tests/wire/endpoint-gap.ts"
                ))
                .arg(format!("ws://{}/v1/ws", server.bind))
                .arg(env!("CARGO_PKG_VERSION"))
                .arg(control)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap())
                .env("HOME", server._root.path())
                .kill_on_drop(true)
                .output();
            tokio::pin!(child);
            let wake = server.wake.clone();
            let output = within(async {
                loop {
                    let changed = wake.notified();
                    tokio::pin!(changed);
                    changed.as_mut().enable();
                    assert!(!server.task.is_finished(), "server exited during trace");
                    Arc::get_mut(&mut server.core).unwrap().service(
                        &registry,
                        &mut workers,
                        server._root.path(),
                    );
                    tokio::select! { biased;
                        output = &mut child => break output.unwrap(),
                        _ = &mut changed => (),
                    }
                }
            })
            .await;
            assert!(
                workers.history(&registry, "fixture").is_some(),
                "real owner lookup installs retained history"
            );
            let error = String::from_utf8_lossy(&output.stderr);
            if !control.is_empty() {
                assert_eq!(output.status.code(), Some(1), "{control}: {error}");
                assert!(
                    error.contains(&format!("error: agent-gap: {failure}")),
                    "{control}: {error}"
                );
            } else {
                assert!(output.status.success(), "{error}");
                let facts: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
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
            }
            let persisted: StreamRecord = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
            assert_eq!(persisted.seq, stored.seq);
            assert_eq!(persisted.raw, stored.raw);
            assert_eq!(persisted.received_at_ms, stored.received_at_ms);
            server.finish().await;
        }
    });
}
