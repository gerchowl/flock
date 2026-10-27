//! Full permission script with real worker, durable history and browser client.
use super::*;
use tokio::io::AsyncBufReadExt;
#[test]
fn permission_script_checks_persisted_timestamps_outbox_and_cursor() {
    runtime(async {
        let server = Server::with_setup(
            |clock| Arc::new(clock),
            |core| {
                Arc::get_mut(&mut core.shared).unwrap().epoch =
                    "00000000000000000000000000000001".into();
            },
        )
        .await;
        let mut case = Case::with_server(246703, server, "queue-permission").await;
        let script = script("permission-decision");
        let open = &script
            .iter()
            .find(|line| line["frame"]["ev"]["state"] == "open")
            .unwrap()["frame"]["ev"];
        let wall = || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        };
        let before = wall();
        let request = json!({"type":"control_request","request_id":open["request_id"],"request":open["request"]});
        within(
            case.driver
                .write_all(format!("{}\n", json!({"op":"event","event":request})).as_bytes()),
        )
        .await
        .unwrap();
        case.until(|c| {
            let handles = c.handles();
            if handles.is_empty() {
                return false;
            }
            assert_eq!(handles, vec![("typed-request".into(), 3)]);
            true
        })
        .await;
        let after = wall();
        assert_eq!(
            case.events().len(),
            4,
            "bootstrap prefix, raw request and durable open"
        );
        let record = case
            .app
            .state
            .structured_agents
            .get(&case.worker.id)
            .unwrap()
            .0;
        let path = case
            .worker
            .fixture
            .root
            .path()
            .join("sessions")
            .join(&case.worker.id)
            .join(format!("{}.jsonl", record.session_id));
        let control = std::env::var("S7_PERMISSION_CONTROL").unwrap_or_default(); // guardrails-ok(hermetic): explicit test-only fail-first control
        let child = tokio::process::Command::new("bun")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/web/client/tests/wire/endpoint-permission.ts"
            ))
            .arg(format!("ws://{}/v1/ws", case.server.bind))
            .arg(&path)
            .arg(before.to_string())
            .arg(after.to_string())
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
                    "worker exited before trace completed"
                );
                tokio::select! { biased;
                    output = &mut child => break output.unwrap(),
                    _ = &mut changed => (), _ = &mut produced => (),
                }
            }
        })
        .await;
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr).into_owned();
            case.stop().await;
            panic!("{error}");
        }
        let facts: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(facts.len(), script.len());
        for (index, (fact, step)) in facts.iter().zip(script).enumerate() {
            assert_eq!(fact["index"], index);
            assert_eq!(fact["checked"], true);
            assert_eq!(fact["step"], step["step"]);
        }
        let mut receipt = String::new();
        within(tokio::io::BufReader::new(&mut case.driver).read_line(&mut receipt))
            .await
            .unwrap();
        let receipt: Value = serde_json::from_str(&receipt).unwrap();
        assert_eq!(receipt["type"], "control_response");
        assert_eq!(receipt["response"]["request_id"], "typed-request");
        assert_eq!(receipt["response"]["response"]["behavior"], "deny");
        assert!(case.handles().is_empty());
        assert_eq!(case.events().last().unwrap()["state"], "closed");
        case.stop().await;
    });
}
