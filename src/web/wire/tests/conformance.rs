//! Server selection follows authored coverage, including multi-label headers.
use super::*;
use crate::structured_wire::fixture::{FixtureCoverage, FixtureLine, FixtureStep};

fn selected() -> Vec<(String, Vec<FixtureLine>)> {
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/client/test/fixtures/wire");
    let mut scripts = Vec::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        let lines = FixtureLine::parse_script(&fs::read_to_string(&path).unwrap()).unwrap();
        if matches!(&lines[0].step, FixtureStep::Given { covers, .. } if covers.contains(&FixtureCoverage::S7Min))
        {
            scripts.push((
                path.file_stem().unwrap().to_str().unwrap().to_owned(),
                lines,
            ));
        }
    }
    scripts.sort_by(|a, b| a.0.cmp(&b.0));
    scripts
}

#[test]
fn server_conformance_selection_contains_no_client_only_fixture() {
    let scripts = selected();
    assert!(!scripts.is_empty());
    assert!(scripts.iter().all(|(name, lines)| {
        name != "fleet-lite"
            && matches!(&lines[0].step, FixtureStep::Given { covers, .. } if !covers.contains(&FixtureCoverage::ClientOnly))
    }));
    for required in ["heartbeat-missed", "unknown-subscription"] {
        assert!(
            scripts.iter().any(|(name, _)| name == required),
            "multi-label header {required}"
        );
    }
}

#[test]
fn selected_welcome_capabilities_match_the_actual_endpoint() {
    runtime(async {
        for (name, lines) in selected() {
            let Some(expected) = lines.iter().find_map(|line| match &line.step {
                FixtureStep::Send { frame, .. } => {
                    let value = serde_json::to_value(frame).unwrap();
                    (value["t"] == "welcome").then_some(value)
                }
                _ => None,
            }) else {
                continue;
            };
            let server = Server::new().await;
            // ADR-0036 #2461: mint exactly the scopes authored by this scenario.
            let path = server._root.path().join("client-tokens/one.token");
            let mut token: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            token["scopes"] = expected["capabilities"].clone();
            fs::write(path, serde_json::to_vec(&token).unwrap()).unwrap();
            let protocol = lines
                .iter()
                .find_map(|line| match &line.step {
                    FixtureStep::Expect {
                        frame: ClientFrame::Hello { protocol, .. },
                        ..
                    } => Some(*protocol),
                    _ => None,
                })
                .expect("selected welcome has a client hello");
            let mut client = Socket::open(&server).await;
            let actual = client.hello(protocol, &"a".repeat(64)).await;
            server.finish().await;
            // Opaque server identities differ; operation capabilities are semantic.
            for field in ["protocol", "capabilities", "idempotency_window_ms"] {
                assert_eq!(actual[field], expected[field], "{name}: welcome.{field}");
            }
        }
    });
}

#[test]
fn replay_credential_without_answer_gets_typed_scope_refusal() {
    runtime(async {
        let mut server = Server::new().await;
        let path = server._root.path().join("client-tokens/one.token");
        let mut token: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        token["scopes"] = json!(["read", "send"]);
        fs::write(path, token.to_string()).unwrap();
        let (_, rx) = mpsc::unbounded_channel();
        let mut app = crate::app::App::new(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        let mut socket = Socket::open(&server).await;
        assert_eq!(
            socket.hello(1, &"a".repeat(64)).await["capabilities"],
            json!(["read", "send"])
        );
        // Scope admission precedes target lookup; no agent or permission book is needed.
        socket.send(json!({"t":"cmd","cmd_id":"missing-answer","verb":"agent.decide","args":{},"resume_only":false})).await;
        let wake = server.wake.clone();
        let actual = within(async {
            loop {
                let notified = wake.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                Arc::get_mut(&mut server.core)
                    .unwrap()
                    .service_commands(&mut app);
                tokio::select! { biased;
                    frame = socket.text() => break frame,
                    _ = &mut notified => (),
                }
            }
        })
        .await;
        server.finish().await;
        assert_eq!(actual["cmd_id"], "missing-answer");
        assert_eq!(actual["ok"], false);
        assert_eq!(actual["error"]["code"], "scope");
        assert_eq!(actual["error"]["retry"], "never");
    });
}
