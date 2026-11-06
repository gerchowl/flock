//! Fresh subscription replays real worker records and pins each ordered proof.
use super::*;
use std::time::{SystemTime, UNIX_EPOCH};
#[test]
fn clean_script_checks_fresh_replay_persistence_cursor_and_causal_controls() {
    runtime(async {
        let server = Server::with_setup(
            |clock| Arc::new(clock),
            |core| {
                Arc::get_mut(&mut core.shared).unwrap().epoch =
                    "00000000000000000000000000000001".into()
            },
        )
        .await;
        let wall = || {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis()
        };
        let bootstrap_start = wall();
        let mut case = Case::with_server(246705, server, "queue").await;
        let assistant_start = wall();
        let event = json!({"op":"event","event":{"type":"assistant","message":{}}});
        within(case.driver.write_all(format!("{event}\n").as_bytes()))
            .await
            .unwrap();
        case.until(|c| c.events().len() == 3).await;
        let (record, _, _) = case
            .app
            .state
            .structured_agents
            .get(&case.worker.id)
            .unwrap();
        let history = case.worker.fixture.root.path().join(format!(
            "sessions/{}/{}.jsonl",
            case.worker.id, record.session_id
        ));
        let starts = json!({"1":bootstrap_start,"2":bootstrap_start,"3":assistant_start});
        let script = crate::structured_wire::fixture::FixtureLine::parse_script(include_str!(
            "../../../../../web/client/test/fixtures/wire/clean.jsonl"
        ))
        .unwrap();
        for (control, failure) in [
            ("persisted", "step 9 (send): persisted timestamp"),
            ("window", "step 9 (send): invalid observed window"),
            ("fresh", "step 8 (expect): sub.after must be absent, got 0"),
            (
                "synced",
                "step 13 (observe): connection is Syncing, expected Live",
            ),
            ("", ""),
        ] {
            let child = tokio::process::Command::new("bun")
                .arg(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/web/client/tests/wire/endpoint-clean.ts"
                ))
                .arg(format!("ws://{}/v1/ws", case.server.bind))
                .arg(&history)
                .arg(starts.to_string())
                .arg(env!("CARGO_PKG_VERSION"))
                .arg(control)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap())
                .env("HOME", case.server._root.path())
                .kill_on_drop(true)
                .output();
            tokio::pin!(child);
            let owner = case.server.wake.clone();
            let worker = case.notify.clone();
            let output = within(async {
                loop {
                    let changed = owner.notified();
                    let produced = worker.notified();
                    tokio::pin!(changed, produced);
                    changed.as_mut().enable();
                    produced.as_mut().enable();
                    case.service();
                    assert!(
                        case.app
                            .structured_workers
                            .report(&case.worker.id, case.worker.generation)
                            .is_none(),
                        "worker exited during trace"
                    );
                    tokio::select! { biased;
                        output = &mut child => break output.unwrap(),
                        _ = &mut changed => (), _ = &mut produced => (),
                    }
                }
            })
            .await;
            let error = String::from_utf8_lossy(&output.stderr);
            if !control.is_empty() {
                assert_eq!(output.status.code(), Some(1), "{control}: {error}");
                assert!(
                    error.contains(&format!("error: clean: {failure}")),
                    "{control}: {error}"
                );
                continue;
            }
            if !output.status.success() {
                let error = error.into_owned();
                case.stop().await;
                panic!("{error}");
            }
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
        case.stop().await;
    });
}
