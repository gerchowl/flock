//! Real authenticated wire commands exercise the socket structured launch funnel.
use super::*;
use crate::web::wire::tests::{app_sessions, Received, Server, Socket};

async fn command(
    fixture: &mut AdmissionFixture,
    server: &mut Server,
    socket: &mut Socket,
    id: &str,
    verb: &str,
    args: Value,
) -> Value {
    socket
        .send(json!({"t":"cmd", "cmd_id":id, "verb":verb,
        "args":args, "resume_only":false}))
        .await;
    let wake = server.wake.clone();
    tokio::time::timeout(LIMIT, async {
        loop {
            let changed = wake.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            fixture.app.service_structured_workers(Instant::now());
            Arc::get_mut(&mut server.core)
                .unwrap()
                .service_commands(&mut fixture.app);
            tokio::select! {
                frame = socket.receive() => match frame {
                    Received::Text(value) => return value,
                    other => panic!("wire closed before ack: {other:?}"),
                },
                _ = changed => (),
            }
        }
    })
    .await
    .expect("wire command producer stayed silent")
}

use std::sync::Arc;

#[tokio::test]
async fn app_operate_wire_stop_resume_preserves_session_and_lists_offline() {
    let _guard = crate::config::test_config_env_guard();
    let mut fixture = AdmissionFixture::new("init-restricted", 120_000);
    let (mut server, _, owner) = app_sessions::fixture().await;
    fixture.app.client_auth = owner.clone();
    let token = app_sessions::mint(&mut fixture.app).await;
    let renewed = app_sessions::mint(&mut fixture.app).await;
    let store = owner
        .get(
            server._root.path(),
            Arc::new(
                crate::structured::permissions::auth::tokens::app_sessions::SystemClock::new(),
            ),
        )
        .unwrap();
    let first_info = store.authenticate(&token).unwrap().info;
    let first_identity = first_info.name;
    let principal = first_info.principal;
    let second_identity = store.authenticate(&renewed).unwrap().info.name;
    assert_ne!(first_identity, second_identity);
    let root = fixture.fixture.root.path().to_owned();
    let audit_path = root.join("wire-audit.jsonl");
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(
            fs::File::create(&audit_path).unwrap(),
        ))
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);

    let original = fs::read_to_string(root.join("claude")).unwrap();
    let script = format!(
        "{}\n{}",
        original.lines().next().unwrap(),
        r#"
import json, os, pathlib, sys
root = pathlib.Path(__file__).parent
if sys.argv[1:] == ['--version']:
    print('2.1.296 (Claude Code)', flush=True)
    sys.exit(0)
flag = '--resume' if '--resume' in sys.argv else '--session-id'
session = sys.argv[sys.argv.index(flag) + 1]
(root / 'observed-launch.pending').write_text(json.dumps(sys.argv[1:]))
os.replace(root / 'observed-launch.pending', root / 'observed-launch.json')
init = json.loads((root / 'restricted-init.json').read_text())
init['session_id'] = session
print(json.dumps(init), flush=True)
sys.stdin.read()
"#
    );
    fixture.fixture.file("claude", script.as_bytes(), 0o700);
    let (id, generation) = fixture.start().await;
    let session = fixture
        .app
        .state
        .structured_agents
        .get(&id)
        .unwrap()
        .0
        .session_id
        .clone();
    let notify = fixture.app.render_notify.clone();
    crate::structured::workers::tests::wait(&notify, || {
        fixture.app.service_structured_workers(Instant::now());
        assert!(
            fixture
                .app
                .structured_workers
                .report(&id, generation)
                .is_none(),
            "fresh producer exited"
        );
        fixture
            .app
            .state
            .structured_agents
            .get(&id)
            .unwrap()
            .0
            .harness_established
    })
    .await;
    let mut socket = Socket::open(&server).await;
    assert_eq!(socket.hello(2, &token.0).await["t"], "welcome");
    let stopped = command(
        &mut fixture,
        &mut server,
        &mut socket,
        "stop",
        "agent.stop",
        json!({"target":"fixture-launch"}),
    )
    .await;
    assert_eq!(stopped["ok"], true, "{stopped}");
    let notify = fixture.app.render_notify.clone();
    crate::structured::workers::tests::wait(&notify, || {
        fixture.app.service_structured_workers(Instant::now());
        fixture
            .app
            .structured_workers
            .report(&id, generation)
            .is_some()
    })
    .await;
    let listed = fixture
        .request(Method::AgentList(crate::api::schema::EmptyParams {}))
        .await;
    assert_eq!(
        listed["result"]["structured_agents"][0]["lifecycle"], "offline",
        "{listed}"
    );
    fixture.app.state.config.structured.profile = "switched-fixture".into();
    let refused = command(
        &mut fixture,
        &mut server,
        &mut socket,
        "profile-refusal",
        "agent.restart",
        json!({"target":"fixture-launch","reason":"fixture resume"}),
    )
    .await;
    assert_eq!(
        refused["error"]["code"], "structured_profile_mismatch",
        "{refused}"
    );
    fixture.app.state.config.structured.profile = "fixture".into();
    fixture.app.state.config.structured.run_as = crate::structured::launch::RunAs::Agent;
    let refused = command(
        &mut fixture,
        &mut server,
        &mut socket,
        "run-as-refusal",
        "agent.restart",
        json!({"target":"fixture-launch","reason":"fixture resume"}),
    )
    .await;
    assert_eq!(
        refused["error"]["code"], "structured_needs_agent_user",
        "{refused}"
    );
    fixture.app.state.config.structured.run_as = crate::structured::launch::RunAs::Operator;
    fs::remove_file(root.join("observed-launch.json")).unwrap();
    let restarted = command(
        &mut fixture,
        &mut server,
        &mut socket,
        "restart",
        "agent.restart",
        json!({"target":"fixture-launch","reason":"fixture resume"}),
    )
    .await;
    assert_eq!(restarted["ok"], true, "{restarted}");
    assert_eq!(restarted["result"]["agent"]["session_id"], session);
    let generation = fixture.app.state.structured_agents.get(&id).unwrap().2;
    crate::structured::workers::tests::wait(&notify, || {
        fixture.app.service_structured_workers(Instant::now());
        assert!(
            fixture
                .app
                .structured_workers
                .report(&id, generation)
                .is_none(),
            "resumed producer exited"
        );
        fixture
            .app
            .structured_workers
            .runtime(&id, generation)
            .is_some()
            && root.join("observed-launch.json").is_file()
    })
    .await;
    let argv: Vec<String> =
        serde_json::from_slice(&fs::read(root.join("observed-launch.json")).unwrap()).unwrap();
    assert!(
        argv.windows(2).any(|pair| pair == ["--resume", &session]),
        "{argv:?}"
    );
    let running = fixture
        .request(Method::AgentList(crate::api::schema::EmptyParams {}))
        .await;
    assert_eq!(
        running["result"]["structured_agents"][0]["lifecycle"], "live",
        "{running}"
    );
    assert_eq!(
        running["result"]["structured_agents"][0]["session_id"],
        session
    );
    let mut second = Socket::open(&server).await;
    assert_eq!(second.hello(2, &renewed.0).await["t"], "welcome");
    let replay = command(
        &mut fixture,
        &mut server,
        &mut second,
        "stop",
        "agent.stop",
        json!({"target":"fixture-launch"}),
    )
    .await;
    assert_eq!(replay, stopped);
    assert!(fixture
        .app
        .structured_workers
        .runtime(&id, generation)
        .is_some());
    let audits: Vec<Value> = fs::read_to_string(&audit_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["fields"]["message"] == "wire lifecycle admission")
        .collect();
    assert!(audits
        .iter()
        .any(|event| event["fields"]["credential"] == first_identity));
    assert!(audits
        .iter()
        .any(|event| event["fields"]["credential"] == second_identity));
    for (command_id, expected_credential, verb, outcome) in [
        ("stop", &first_identity, "agent.stop", "admitted"),
        ("restart", &first_identity, "agent.restart", "admitted"),
        (
            "profile-refusal",
            &first_identity,
            "agent.restart",
            "structured_profile_mismatch",
        ),
        (
            "run-as-refusal",
            &first_identity,
            "agent.restart",
            "structured_needs_agent_user",
        ),
        ("stop", &second_identity, "agent.stop", "admitted"),
    ] {
        let event = audits
            .iter()
            .find(|event| {
                event["fields"]["cmd_id"] == command_id
                    && event["fields"]["credential"] == *expected_credential
            })
            .expect("lifecycle audit");
        let fields = &event["fields"];
        assert_eq!(fields["target"], "fixture-launch", "{fields}");
        assert_eq!(fields["agent_id"], id, "{fields}");
        assert_eq!(fields["principal"], principal, "{fields}");
        assert_eq!(fields["verb"], verb, "{fields}");
        assert_eq!(fields["outcome"], outcome, "{fields}");
        if verb == "agent.restart" {
            assert_eq!(fields["session_id"], session, "{fields}");
        }
    }
    drop(second);
    drop(socket);
    server.finish().await;
}

#[tokio::test]
async fn app_without_operate_and_remote_wire_lifecycle_refuse() {
    let _guard = crate::config::test_config_env_guard();
    let mut fixture = AdmissionFixture::new("init-restricted", 120_000);
    let (mut server, _, owner) = app_sessions::fixture().await;
    fixture.app.client_auth = owner.clone();
    let token = app_sessions::mint(&mut fixture.app).await;
    let (id, _) = fixture.start().await;
    let mut socket = Socket::open(&server).await;
    assert_eq!(socket.hello(2, &token.0).await["t"], "welcome");
    for verb in ["agent.stop", "agent.restart"] {
        let ack = command(
            &mut fixture,
            &mut server,
            &mut socket,
            verb,
            verb,
            json!({"target":id,"node_id":"remote-fixture","reason":"fixture resume"}),
        )
        .await;
        assert_eq!(ack["error"]["code"], "unsupported_for_agent", "{ack}");
    }
    for (command_id, args, code) in [
        (
            "active",
            json!({"target":id,"reason":"fixture resume"}),
            "session_busy",
        ),
        (
            "empty-reason",
            json!({"target":id,"reason":""}),
            "invalid_params",
        ),
        (
            "unsupported-when",
            json!({"target":id,"reason":"fixture resume","when":"now"}),
            "invalid_params",
        ),
        (
            "continuation",
            json!({"target":id,"reason":"fixture resume","continue_with":"fixture turn"}),
            "runtime_unsupported",
        ),
    ] {
        let ack = command(
            &mut fixture,
            &mut server,
            &mut socket,
            command_id,
            "agent.restart",
            args,
        )
        .await;
        assert_eq!(ack["error"]["code"], code, "{ack}");
    }
    let store = owner
        .get(
            server._root.path(),
            Arc::new(
                crate::structured::permissions::auth::tokens::app_sessions::SystemClock::new(),
            ),
        )
        .unwrap();
    let mut session = store.authenticate(&token).unwrap();
    store.answer_only_app_for_test(&mut session);
    for verb in ["agent.stop", "agent.restart"] {
        let ack = command(
            &mut fixture,
            &mut server,
            &mut socket,
            &format!("denied-{verb}"),
            verb,
            json!({"target":id,"reason":"fixture resume"}),
        )
        .await;
        assert_eq!(ack["error"]["code"], "not_authorized", "{ack}");
    }
    drop(socket);
    server.finish().await;
}

#[tokio::test]
async fn structured_peer_cannot_mint_wire_operator_or_resume_socket_record() {
    let _guard = crate::config::test_config_env_guard();
    let mut fixture = AdmissionFixture::new("init-restricted", 120_000);
    let (id, _) = fixture.start().await;
    let mut lease = fixture
        .app
        .session_leases
        .reserve("agent_atlas_wirecaller", "wire-agent-fixture", None)
        .unwrap();
    lease.bind_test_peer(std::process::id());
    let mint: Method =
        serde_json::from_value(json!({"method":"client.app.mint", "params":{}})).unwrap();
    let denied = fixture.request(mint).await;
    assert_eq!(denied["error"]["code"], "operator_only", "{denied}");
    let params =
        serde_json::from_value(json!({"runtime":"structured", "name":id,"argv":["claude"]}))
            .unwrap();
    let denied = fixture.request(Method::AgentStart(params)).await;
    assert_eq!(denied["error"]["code"], "operator_only", "{denied}");
}

#[tokio::test]
async fn structured_lifecycle_refusals_audit_target_session_and_principal() {
    let _guard = crate::config::test_config_env_guard();
    let mut fixture = AdmissionFixture::new("init-restricted", 120_000);
    let (mut server, _, owner) = app_sessions::fixture().await;
    fixture.app.client_auth = owner.clone();
    let token = app_sessions::mint(&mut fixture.app).await;
    let store = owner
        .get(
            server._root.path(),
            Arc::new(
                crate::structured::permissions::auth::tokens::app_sessions::SystemClock::new(),
            ),
        )
        .unwrap();
    let info = store.authenticate(&token).unwrap().info;
    let (id, _) = fixture.start().await;
    let session_id = fixture
        .app
        .state
        .structured_agents
        .get(&id)
        .unwrap()
        .0
        .session_id
        .clone();
    let audit_path = fixture.fixture.root.path().join("refusal-audit.jsonl");
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(
            fs::File::create(&audit_path).unwrap(),
        ))
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let mut socket = Socket::open(&server).await;
    assert_eq!(socket.hello(2, &token.0).await["t"], "welcome");
    let remote = "00000000-0000-4000-8000-000000003153";
    let mut peer = crate::peers::PeerSummaryState::new(&crate::config::PeerConfig {
        name: "remote.example".into(),
        ..Default::default()
    });
    peer.structured_agents
        .push(crate::api::schema::PeerStructuredAgentSummary {
            agent_id: remote.into(),
            agent: None,
            status: crate::api::schema::AgentStatus::Idle,
            runtime: crate::structured::registry::StructuredRuntime::Structured,
        });
    fixture.app.state.peer_summaries.push(peer);
    for verb in ["agent.stop", "agent.restart"] {
        let refused = command(
            &mut fixture,
            &mut server,
            &mut socket,
            &format!("remote-{verb}"),
            verb,
            json!({"target":remote,"reason":"fixture resume"}),
        )
        .await;
        assert_eq!(
            refused["error"]["code"], "unsupported_for_agent",
            "{refused}"
        );
    }
    let mut reduced = store.authenticate(&token).unwrap();
    store.answer_only_app_for_test(&mut reduced);
    for verb in ["agent.stop", "agent.restart"] {
        let refused = command(&mut fixture, &mut server, &mut socket, verb, verb,
            json!({"target":"fixture-launch","reason":"fixture resume",
                "agent_id":"forged-agent", "session_id":"forged-session", "principal":"forged-principal"})).await;
        assert_eq!(refused["error"]["code"], "not_authorized", "{refused}");
    }
    let audits: Vec<Value> = fs::read_to_string(&audit_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["fields"]["message"] == "wire lifecycle admission")
        .collect();
    assert_eq!(audits.len(), 4);
    for event in audits {
        let fields = &event["fields"];
        assert_eq!(fields["credential"], info.name, "{fields}");
        assert_eq!(fields["principal"], info.principal, "{fields}");
        if fields["target"] == remote {
            assert_eq!(fields["agent_id"], remote, "{fields}");
            assert_eq!(fields["session_id"], "", "{fields}");
            assert_eq!(fields["outcome"], "unsupported_for_agent", "{fields}");
            continue;
        }
        assert_eq!(fields["target"], "fixture-launch", "{fields}");
        assert_eq!(fields["agent_id"], id, "{fields}");
        assert_eq!(fields["outcome"], "not_authorized", "{fields}");
        if fields["verb"] == "agent.restart" {
            assert_eq!(fields["session_id"], session_id, "{fields}");
        }
    }
    drop(socket);
    server.finish().await;
}
