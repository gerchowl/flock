use super::*;
use crate::{
    api::schema::{Method, MsgSendParams, Request},
    mesh::{hello::with_store, identity::NodeIdentity, key::MessageKey, runtime_store::TestStore},
    structured::workers::tests::global::{max_policy, AppQueue},
};
use serde_json::{json, Value};
use std::{
    future::Future,
    time::{Duration, Instant},
};
fn run(future: impl Future<Output = ()>) {
    let (_guard, _scrub) = crate::config::test_config_env_guard();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future);
}
fn node(r: &mut AppQueue) {
    r.app.node_id = Some(NodeIdentity::load().unwrap().node_id());
    r.app.runtime_config.uid_policy = crate::structured::identity::UidPolicy {
        operator: crate::platform::peer::effective_uid(),
        dedicated: Ok(None).into(),
    };
}
fn send(r: &mut AppQueue, body: &str) -> Receiver<String> {
    let (respond_to, rx) = mpsc::channel();
    let params: MsgSendParams = serde_json::from_value(json!({
        "to":{"type":"agent","agent":r.case.id}, "body":body,
        "correlation_id":"refusal-round","intent":"fyi"
    }))
    .unwrap();
    let pid = r
        .app
        .terminal_runtimes
        .values()
        .find_map(crate::terminal::TerminalRuntime::child_pid)
        .unwrap_or_else(std::process::id);
    r.app
        .handle_api_request_message(crate::api::ApiRequestMessage {
            request: Request {
                id: "refusal-round".into(),
                method: Method::MsgSend(params),
            },
            respond_to,
            peer_pid: Some(pid),
            peer_birth: crate::platform::process_start_time(pid),
            peer_uid: Some(crate::platform::peer::effective_uid()),
        });
    rx
}
async fn answer(r: &mut AppQueue, rx: &Receiver<String>) -> Value {
    let mut result = None;
    r.until(|_| {
        result = rx.try_recv().ok();
        result.is_some()
    })
    .await;
    serde_json::from_str(&result.unwrap()).unwrap()
}
fn status(r: &AppQueue) -> crate::mesh::store::Status {
    with_store(|s| {
        s.status(
            r.app.node_id.as_deref().unwrap(),
            "refusal-round",
            crate::app::notifications::now_ms() as i64,
        )
        .map_err(|e| e.to_string())
    })
    .unwrap()
    .unwrap()
}
#[test]
fn local_custody_busy_refusal_cannot_resurrect_and_retry_delivers_once() {
    run(async {
        let _store = TestStore::new();
        let mut r = AppQueue::new(2301, |_| max_policy()).await;
        node(&mut r);
        r.until(|r| r.app.queue_observation(&r.case.id).is_some())
            .await;
        r.app.state.workspaces = vec![crate::workspace::Workspace::test_new("custody-caller")];
        r.app.state.ensure_test_terminals();
        let pane = r.app.state.workspaces[0].focused_pane_id().unwrap();
        let terminal = r.app.state.terminal_id_for_pane(0, pane).unwrap();
        r.app
            .state
            .terminals
            .get_mut(&terminal)
            .unwrap()
            .launch_argv = Some(vec!["claude".into()]);
        crate::test_support::attest_pane_caller(&mut r.app, pane);
        for n in 0..112 {
            let (tx, _rx) = mpsc::channel();
            r.app
                .structured_sends
                .waiting
                .insert((r.case.id.clone(), r.case.generation, n.to_string()), tx);
        }
        let rx = send(&mut r, "refused K1");
        let response: Value =
            serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).unwrap()).unwrap();
        assert_eq!(response["error"]["code"], "session_busy");
        let refused = status(&r).reference.key;
        r.stop().await;
        r.restart(|r| {
            r.app.structured_sends = Default::default();
            r.app.mailboxes = Default::default();
            r.app.restore_mesh_mail().unwrap();
        })
        .await;
        let retry = send(&mut r, "retry K2");
        let response = answer(&mut r, &retry).await;
        assert!(response["result"]["item_id"].is_string(), "{response}");
        let admitted: MessageKey =
            serde_json::from_value(response["result"]["message_key"].clone()).unwrap();
        assert_ne!(admitted, refused);
        r.until(|r| !r.inputs().is_empty()).await;
        let input = r.inputs();
        assert_eq!(input.len(), 1);
        assert!(
            input[0]["input"]["message"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("retry K2"),
            "{input:?}"
        );
        assert_eq!(
            with_store(|s| s.get(&refused).map_err(|e| e.to_string()))
                .unwrap()
                .unwrap()
                .state,
            "refused"
        );
        r.stop().await;
    });
}
#[test]
fn local_custody_owner_refusal_is_terminal_before_error_response() {
    run(async {
        let _store = TestStore::new();
        let mut r = AppQueue::new(2302, |case| {
            let journal =
                crate::structured::queue::Journal::open(case.fixture.root.path(), &case.id)
                    .unwrap();
            drop(journal);
            let path = case
                .fixture
                .root
                .path()
                .join("sessions")
                .join(&case.id)
                .join("queue.jsonl");
            std::fs::write(
                path,
                format!(
                    "{}\n",
                    json!({"kind":"queue_identity","agent_id":case.id,"format_version":999})
                ),
            )
            .unwrap();
            max_policy()
        })
        .await;
        node(&mut r);
        let rx = send(&mut r, "owner refuses");
        let captured = super::super::mesh_mail::load_mesh_mail(
            r.app.node_id.clone().unwrap(),
            Vec::new(),
            false,
            r.app.mesh_recovery_recipients(),
        )
        .unwrap();
        let response = answer(&mut r, &rx).await;
        assert_eq!(
            response["error"]["code"], "queue_journal_newer",
            "{response}"
        );
        assert_eq!(status(&r).state, "refused");
        r.app.apply_mesh_mail(captured);
        assert!(
            r.app.structured_sends.waiting.is_empty(),
            "a stale inbox snapshot must not offer a refused key"
        );
        assert!(r.inputs().is_empty());
        r.stop().await;
    });
}
#[test]
fn local_custody_unknown_effect_keeps_inbox_and_returns_inspection_keys() {
    run(async {
        let _store = TestStore::new();
        let mut r = AppQueue::new(2303, |_| max_policy()).await;
        node(&mut r);
        r.until(|r| r.app.queue_observation(&r.case.id).is_some())
            .await;
        let rx = send(&mut r, "unknown effect");
        let item_id = r
            .app
            .structured_sends
            .waiting
            .keys()
            .next()
            .unwrap()
            .2
            .clone();
        let key = status(&r).reference.key;
        // Exercise the existing owner-completion seam for a possibly committed write.
        r.app.structured_sends.complete(
            r.case.id.clone(),
            r.case.generation,
            item_id.clone(),
            Err(Failure {
                code: ErrorCode::InputWriteUnknown,
                source: None,
            }),
        );
        r.app.service_structured_workers(Instant::now());
        let response = answer(&mut r, &rx).await;
        assert_eq!(response["error"]["code"], "input_write_unknown");
        assert_eq!(response["error"]["data"]["message_key"], json!(key));
        assert_eq!(response["error"]["data"]["item_id"], item_id);
        assert_eq!(
            with_store(|s| s.get(&key).map_err(|e| e.to_string()))
                .unwrap()
                .unwrap()
                .state,
            "inbox"
        );
        r.until(|r| !r.inputs().is_empty()).await;
        assert_eq!(r.inputs().len(), 1);
        r.stop().await;
    });
}

#[test]
fn local_custody_failed_refusal_settlement_returns_unknown_with_keys() {
    run(async {
        let _store = TestStore::new();
        let mut r = AppQueue::new(2306, |_| max_policy()).await;
        node(&mut r);
        r.until(|r| r.app.queue_observation(&r.case.id).is_some())
            .await;
        let rx = send(&mut r, "settlement unavailable");
        let item_id = r
            .app
            .structured_sends
            .waiting
            .keys()
            .next()
            .unwrap()
            .2
            .clone();
        let key = status(&r).reference.key;
        let custody_path = with_store(|s| Ok(s.path().to_owned())).unwrap();
        crate::mesh::runtime_store::suspend().unwrap();
        r.app.structured_sends.complete(
            r.case.id.clone(),
            r.case.generation,
            item_id.clone(),
            Err(Failure {
                code: ErrorCode::QueueItemTooLarge,
                source: None,
            }),
        );
        r.app.service_structured_workers(Instant::now());
        let response = answer(&mut r, &rx).await;
        assert_eq!(
            response["error"]["code"], "input_write_unknown",
            "a failed terminal settlement cannot promise refusal"
        );
        assert_eq!(response["error"]["data"]["message_key"], json!(key));
        assert_eq!(response["error"]["data"]["item_id"], item_id);
        let reopened = crate::mesh::store::Store::open(
            &custody_path,
            crate::app::notifications::now_ms() as i64,
        )
        .unwrap();
        assert_eq!(reopened.get(&key).unwrap().unwrap().state, "inbox");
        drop(reopened);
        r.stop().await;
    });
}
