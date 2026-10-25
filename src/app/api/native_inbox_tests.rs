use crate::{
    api::{schema::*, scoped::tests::live_scope},
    app::App,
};
use serde_json::{json, Value};

#[tokio::test]
async fn native_inbox_list_read_mixed_batch_is_owned_and_consume_once() {
    let (_env, _scrub) = crate::config::test_config_env_guard();
    let _store = crate::mesh::runtime_store::TestStore::new();
    let (leases, lease, scope, peer) = live_scope();
    let (_, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &crate::config::Config::default(),
        true,
        None,
        rx,
        crate::api::EventHub::default(),
    );
    app.session_leases = leases;
    app.runtime_config.uid_policy = crate::structured::identity::UidPolicy {
        operator: peer.uid.unwrap(),
        dedicated: Ok(None).into(),
    };
    app.state
        .structured_agents
        .register(crate::structured::registry::Record {
            runtime: crate::structured::registry::StructuredRuntime::Structured,
            agent_id: "fixture-agent".into(),
            name: Some("recipient".into()),
            harness: "claude".into(),
            session_id: "fixture-session".into(),
            cwd: std::env::temp_dir(),
            model: None,
        })
        .unwrap();
    let invoke = |app: &mut App, method: &str, pane: Option<&str>| -> Value {
        let (tx, rx) = std::sync::mpsc::channel();
        app.handle_api_request_message(crate::api::ApiRequestMessage {
            request: serde_json::from_value(
                json!({"id":"inbox", "method":method,"params":{"pane":pane}}),
            )
            .unwrap(),
            respond_to: tx,
            peer_pid: peer.pid,
            peer_uid: peer.uid,
            peer_birth: peer.birth,
            scope: Some(scope.clone()),
        });
        serde_json::from_str(&rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap()).unwrap()
    };
    app.state.config.msg.inbox_retention_ms = 100_000;
    let mut keys = Vec::new();
    for correlation in ["expired", "live"] {
        let message = crate::app::mailboxes::PendingMessage {
            message_key: None,
            correlation_id: correlation.into(),
            body: "body".into(),
            from_pane: None,
            from_agent: Some("sender".into()),
            from_host: None,
            from_repo: None,
            to_pane: "fixture-agent".into(),
            to_repo: None,
            in_reply_to: None,
            enqueued_at_ms: super::messages::now_ms(),
            delivery_attempts: 0,
            intent: MsgIntent::Fyi,
        };
        let envelope = super::mesh_mail::envelope(
            "origin.example",
            "fixture-agent".into(),
            &super::mesh_mail::Payload {
                message,
                peer: None,
                host: None,
                direct: false,
            },
        )
        .unwrap();
        crate::mesh::hello::with_store(|s| {
            let now = super::messages::now_ms() as i64;
            s.accept_local(
                &envelope,
                crate::mesh::store::CUSTODY_TTL_MS,
                "fixture-agent",
                "fixture-session",
                now,
            )
            .unwrap();
            s.settle_queue_admission(&envelope.key, None, now).unwrap();
            if correlation == "expired" {
                s.set_paused(false, now + 100_001).unwrap();
            }
            Ok(())
        })
        .unwrap();
        keys.push(envelope.key);
    }
    for method in ["msg.list", "msg.read"] {
        let denied = invoke(&mut app, method, Some("another-agent"));
        assert_eq!(denied["error"]["code"], "not_authorized");
        assert_eq!(
            crate::api::error_code::ErrorCode::NotAuthorized.retry(),
            crate::cli::exit::Retry::Never
        );
    }
    let listed = invoke(&mut app, "msg.list", None);
    assert_eq!(
        listed["result"]["messages"].as_array().unwrap().len(),
        1,
        "{listed}"
    );
    let read = invoke(&mut app, "msg.read", None);
    assert_eq!(
        read["result"]["messages"][0]["correlation_id"], "live",
        "{read}"
    );
    assert_eq!(
        invoke(&mut app, "msg.read", None)["result"]["messages"],
        json!([])
    );
    assert_eq!(
        invoke(&mut app, "msg.list", None)["result"]["messages"],
        json!([])
    );
    crate::mesh::hello::with_store(|s| {
        for key in &keys {
            assert_eq!(s.get(key).unwrap().unwrap().state, "delivered");
        }
        Ok(())
    })
    .unwrap();
    let drops = app
        .event_hub
        .ring_snapshot_after(0)
        .into_iter()
        .filter(|(_, e)| {
            matches!(
        &e.data, EventData::MessageDelivered { correlation_id, delivered: false, outcome, .. }
        if correlation_id == "expired" && outcome == "dropped_undeliverable")
        })
        .count();
    assert_eq!(drops, 1);
    drop(lease);
    assert_eq!(
        invoke(&mut app, "msg.read", None)["error"]["code"],
        "not_authorized"
    );
}
