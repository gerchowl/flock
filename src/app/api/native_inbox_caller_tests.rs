use super::*;
use crate::api::{
    schema::Request,
    scoped::{tests::live_scope, Scope},
};
use serde_json::{json, Value};
fn register(app: &mut App, id: &str, name: &str, session: &str) {
    app.state
        .structured_agents
        .register(crate::structured::registry::Record {
            runtime: crate::structured::registry::StructuredRuntime::Structured,
            agent_id: id.into(),
            name: Some(name.into()),
            harness: "claude".into(),
            session_id: session.into(),
            cwd: std::env::temp_dir().join("native-inbox-fixture"),
            model: None,
        })
        .unwrap();
}
fn app() -> App {
    let (_, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &crate::config::Config::default(),
        true,
        None,
        rx,
        crate::api::EventHub::default(),
    );
    app.runtime_config.uid_policy.dedicated = Ok(None).into();
    register(&mut app, "fixture-agent", "native-name", "fixture-session");
    register(&mut app, "other-agent", "other-name", "other-session");
    app
}
fn call(app: &mut App, scope: Option<&Scope>, verb: &str, pane: Option<&str>) -> Value {
    let request: Request =
        serde_json::from_value(json!({"id":"inbox-test","method":verb,"params":{"pane":pane}}))
            .unwrap();
    let (respond_to, reply) = std::sync::mpsc::channel();
    let (socket, _other) = std::os::unix::net::UnixStream::pair().unwrap();
    let peer = crate::platform::peer::credentials(&socket);
    app.handle_api_request_message(crate::api::ApiRequestMessage {
        request,
        respond_to,
        scope: scope.cloned(),
        peer_pid: peer.pid,
        peer_birth: peer.birth,
        peer_uid: peer.uid,
    });
    serde_json::from_str(
        &reply
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap(),
    )
    .unwrap()
}
fn message(id: &str) -> crate::app::mailboxes::PendingMessage {
    serde_json::from_value(
        json!({"correlation_id":id,"body":"fixture body", "to_pane":"fixture-agent",
        "enqueued_at_ms":super::messages::now_ms(),"delivery_attempts":0,"intent":"fyi"}),
    )
    .unwrap()
}
fn seed(agent: &str, session: &str, retention: u64) -> String {
    crate::mesh::hello::with_store(|s| {
        let mut e = crate::mesh::sign::tests::signed();
        e.correlation_id = e.key.message_id.clone();
        e.body = serde_json::to_vec(&super::mesh_mail::Payload {
            message: message(&e.correlation_id),
            peer: None,
            host: None,
            direct: false,
        })
        .unwrap();
        let now = super::messages::now_ms() as i64;
        s.accept_local(&e, crate::mesh::store::CUSTODY_TTL_MS, agent, session, now)
            .unwrap();
        s.settle_queue_admission(&e.key, None, now).unwrap();
        s.native_inbox_delivered(&e.key, retention, now).unwrap();
        Ok(e.correlation_id)
    })
    .unwrap()
}
#[tokio::test]
async fn native_inbox_caller_ownership_and_directory_ambiguity() {
    let (_env, _scrub) = crate::config::test_config_env_guard();
    let _store = crate::mesh::runtime_store::TestStore::new();
    let mut app = app();
    let (leases, _lease, scope, _) = live_scope();
    app.session_leases = leases;
    let own = seed("fixture-agent", "fixture-session", 60_000);
    seed("other-agent", "other-session", 60_000);
    for verb in ["msg.list", "msg.read"] {
        assert_eq!(
            call(&mut app, Some(&scope), verb, Some("other-name"))["error"]["code"],
            "inbox_not_owner"
        );
        assert_eq!(
            call(&mut app, Some(&scope), verb, Some("missing"))["error"]["code"],
            "inbox_not_owner"
        );
    }
    assert_eq!(
        call(&mut app, Some(&scope), "msg.list", Some("native-name"))["result"]["messages"][0]
            ["correlation_id"],
        own
    );
    register(&mut app, "native-name", "collision", "collision-session");
    for verb in ["msg.list", "msg.read"] {
        assert_eq!(
            call(&mut app, Some(&scope), verb, Some("native-name"))["error"]["code"],
            "agent_target_ambiguous"
        );
    }
    assert_eq!(
        call(&mut app, Some(&scope), "msg.read", None)["result"]["messages"][0]["correlation_id"],
        own
    );
}
#[tokio::test]
async fn native_inbox_caller_invalid_and_revoked_credentials_are_uniform() {
    let (_env, _scrub) = crate::config::test_config_env_guard();
    let _store = crate::mesh::runtime_store::TestStore::new();
    let mut app = app();
    let (leases, lease, scope, _) = live_scope();
    app.session_leases = leases;
    assert!(call(&mut app, Some(&scope), "msg.read", None)["result"]["messages"].is_array());
    let unknown = crate::api::scoped::tests::unknown_credential(&scope);
    let refused = call(&mut app, Some(&unknown), "msg.read", Some("other-name"));
    drop(lease);
    for verb in ["msg.list", "msg.read"] {
        for target in [None, Some("fixture-agent"), Some("missing")] {
            assert_eq!(call(&mut app, Some(&scope), verb, target), refused);
        }
    }
    assert_eq!(refused["error"]["code"], "not_authorized");
}
#[tokio::test]
async fn native_inbox_caller_mixed_expiry_consumes_once_and_operator_list_is_unchanged() {
    let (_env, _scrub) = crate::config::test_config_env_guard();
    let _store = crate::mesh::runtime_store::TestStore::new();
    let mut app = app();
    let live = seed("fixture-agent", "fixture-session", 60_000);
    app.mailboxes.enqueue(message("legacy"));
    let global = call(&mut app, None, "msg.list", None);
    assert_eq!(global["result"]["messages"].as_array().unwrap().len(), 1);
    assert_eq!(global["result"]["messages"][0]["correlation_id"], "legacy");
    let (leases, _lease, scope, _) = live_scope();
    app.session_leases = leases;
    let listed = call(&mut app, Some(&scope), "msg.list", None);
    assert_eq!(listed["result"]["messages"].as_array().unwrap().len(), 1);
    assert_eq!(listed["result"]["messages"][0]["correlation_id"], live);
    let expired = seed("fixture-agent", "fixture-session", 0);
    let read = call(&mut app, Some(&scope), "msg.read", None);
    assert_eq!(read["result"]["messages"][0]["correlation_id"], live);
    assert_eq!(read["result"]["messages"].as_array().unwrap().len(), 1);
    assert!(
        call(&mut app, Some(&scope), "msg.read", None)["result"]["messages"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(app.event_hub.ring_snapshot_after(0).iter().filter(|(_, e)| matches!(&e.data,
        crate::api::schema::EventData::MessageDelivered { correlation_id, delivered:false, outcome,.. }
        if correlation_id == &expired && outcome == "dropped_undeliverable")).count(), 1);
}
