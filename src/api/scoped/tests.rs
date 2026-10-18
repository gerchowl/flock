use super::*;
use crate::{
    api::{
        schema::{EmptyParams, Method},
        EventHub,
    },
    structured::lease::Lease,
};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    sync::{atomic::AtomicBool, Mutex},
    time::Duration,
};

pub(crate) fn live_scope() -> (SessionLeases, Lease, Scope, Credentials) {
    let (socket, _other) = UnixStream::pair().unwrap();
    let peer = crate::platform::peer::credentials(&socket);
    let leases = SessionLeases::default();
    let mut lease = leases
        .reserve("fixture-agent", "fixture-session", None)
        .unwrap();
    lease.bind_test_peer(peer.pid.unwrap());
    let mut scope = Scope(Arc::new(ScopeData {
        credential: Some(lease.credential()),
        peer,
        leases: Arc::default(),
        principal: Arc::default(),
        proof: Arc::default(),
        fact_reader: None,
        probe: true,
        continuation: false,
    }));
    let policy = UidPolicy {
        operator: peer.uid.unwrap(),
        dedicated: Ok(None).into(),
    };
    assert_eq!(scope.probe_reply(&leases, &policy), Some("scoped_ok"));
    Arc::get_mut(&mut scope.0).unwrap().probe = false;
    (leases, lease, scope, peer)
}
fn request() -> Request {
    Request {
        id: "scope-fixture".into(),
        method: Method::ServerStop(EmptyParams::default()),
    }
}

#[test]
fn scoped_envelope_cannot_be_downgraded_by_absence_or_malformed_fields() {
    let (_, lease, _, _) = live_scope();
    let wire = serde_json::to_string(&ScopedRequest {
        request: request(),
        credential: Some(lease.credential()),
    })
    .unwrap();
    assert!(decode(&wire).unwrap().1.unwrap().is_some());
    assert!(
        decode(&serde_json::json!({"request":request()}).to_string())
            .unwrap()
            .1
            .unwrap()
            .is_none()
    );
    assert!(decode(
        &serde_json::json!({"request":request(),"method":"server.stop","id":"mixed"}).to_string()
    )
    .is_err());
    assert!(decode(
        &serde_json::json!({"credential":null,"method":"server.stop","id":"mixed"}).to_string()
    )
    .is_err());
    assert!(decode(&serde_json::to_string(&request()).unwrap())
        .unwrap()
        .1
        .is_none());
}

#[test]
fn scoped_transport_proof_never_promotes_after_release_or_for_an_unknown_uid() {
    let (leases, lease, scope, peer) = live_scope();
    assert!(matches!(scope.caller(), Caller::Agent { .. }));
    let pending = Scope(Arc::new(ScopeData {
        probe: true,
        leases: Arc::default(),
        ..scope.0.as_ref().clone()
    }));
    let unknown = UidPolicy {
        operator: peer.uid.unwrap().wrapping_add(1),
        dedicated: Ok(None).into(),
    };
    assert_eq!(
        pending.probe_reply(&leases, &unknown),
        Some("scoped_refused")
    );
    let dedicated = UidPolicy {
        operator: peer.uid.unwrap().wrapping_add(1),
        dedicated: Ok(peer.uid).into(),
    };
    assert_eq!(pending.probe_reply(&leases, &dedicated), Some("scoped_ok"));
    assert!(matches!(pending.caller(), Caller::Agent { .. }));
    drop(lease);
    assert_eq!(scope.caller(), Caller::Uncertain);
}

#[test]
fn scoped_socket_rejects_missing_and_wrong_credentials_before_a_fast_path() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut secrets = Vec::new();
    let logs = crate::logging::capture_logs_across_threads(|| {
        runtime.block_on(async {
    for mode in ["valid","busy","missing","wrong","malformed"] {
        let (server,mut client)=UnixStream::pair().unwrap();
        client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (tx,rx)=tokio::sync::mpsc::unbounded_channel();
        let mut app=crate::app::App::new(&crate::config::Config::default(),true,None,rx,EventHub::default());
        let peer=crate::platform::peer::credentials(&client);
        app.runtime_config.uid_policy=UidPolicy {operator:peer.uid.unwrap(),dedicated:Ok(None).into()};
        let mut lease=app.session_leases.reserve("fixture-agent","fixture-session",None).unwrap();
        lease.bind_test_peer(peer.pid.unwrap());
        let credential=lease.credential();
        let secret=serde_json::to_value(&credential).unwrap().as_str().unwrap().to_owned();
        secrets.push(secret.clone());
        let payload=match mode {
            "valid"|"busy"=>serde_json::json!({"request":{"id":"scope","method":"ping","params":{}},"credential":secret}),
            "missing"=>serde_json::json!({"request":{"id":"scope","method":"ping","params":{}}}),
            "wrong"=>serde_json::json!({"request":{"id":"scope","method":"ping","params":{}},"credential":"wrong"}),
            _=>serde_json::json!({"request":{"id":"scope","method":"ping","params":{"unexpected":secret}},"credential":secret,"extra":true}),
        };
        let policy = app.runtime_config.uid_policy.clone();
        let task=std::thread::spawn(move || crate::api::server::handle_connection(server,&tx,&EventHub::default(),&Arc::new(AtomicBool::new(true)),None,&Mutex::default(),&policy).unwrap());
        writeln!(client,"{payload}").unwrap();
        if mode!="malformed" {
            let msg=tokio::time::timeout(Duration::from_secs(5),app.api_rx.recv()).await.unwrap().unwrap();
            if mode == "busy" {
                app.session_leases.with_registry_locked(|| assert!(app.respond_scope_probe(&msg)));
            } else {
                assert!(app.respond_scope_probe(&msg));
            }
        }
        let mut response=String::new();
        BufReader::new(&mut client).read_line(&mut response).unwrap();
        task.join().unwrap();
        assert!(!response.contains(&secret));
        assert_eq!(serde_json::from_str::<serde_json::Value>(&response).unwrap().get("error").is_none(),mode=="valid","{response}");
        if mode == "busy" {
            assert_eq!(serde_json::from_str::<serde_json::Value>(&response).unwrap()["error"]["code"], "lease_busy");
        }
        assert!(!app.state.should_quit);
        let debug=format!("{:?}",ScopedRequest {request:request(),credential:Some(credential)});
        assert!(!debug.contains(&secret));
    }
    })
    });
    for secret in secrets {
        assert!(!logs.contains(&secret));
    }
}

#[test]
fn scoped_continuations_stop_after_revocation_and_context_is_restored() {
    let (_, lease, scope, _) = live_scope();
    let guard = Guard(CURRENT.with(|slot| slot.borrow_mut().replace(scope)));
    let (mut socket, _peer) = UnixStream::pair().unwrap();
    assert!(!crate::api::server::should_stop_connection(
        &mut socket,
        &Arc::new(AtomicBool::new(true))
    )
    .unwrap());
    drop(lease);
    assert!(crate::api::server::should_stop_connection(
        &mut socket,
        &Arc::new(AtomicBool::new(true))
    )
    .unwrap());
    drop(guard);
    assert!(Scope::current().is_none());
}

#[test]
fn legacy_invalid_request_keeps_serde_detail_but_scoped_detail_is_redacted() {
    let legacy = decode(r#"{"id":"x","method":"unknown-fixture-method","params":{}}"#)
        .unwrap_err()
        .unwrap();
    assert!(legacy.to_string().contains("unknown-fixture-method"));
    assert!(decode(r#"{"request":{"id":"x","method":"secret-canary","params":{}},"credential":"credential-canary"}"#).unwrap_err().is_none());
}

#[tokio::test]
async fn queued_busy_admission_returns_typed_refusal_before_effect() {
    let (leases, _lease, scope, _) = live_scope();
    let (_, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = crate::app::App::new(
        &crate::config::Config::default(),
        true,
        None,
        rx,
        EventHub::default(),
    );
    app.current_api_scope = Some(scope);
    let response = leases.with_registry_locked(|| app.handle_api_request(request()));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&response).unwrap()["error"]["code"],
        "lease_busy"
    );
    assert!(!app.state.should_quit);
}

#[test]
fn admitted_policy_identity_does_not_repeat_a_busy_registry_walk() {
    let (leases, _lease, scope, _) = live_scope();
    leases.with_registry_locked(|| {
        assert_eq!(scope.refusal(), Some(ErrorCode::LeaseBusy));
        assert!(matches!(scope.principal(), Caller::Agent { .. }));
    });
}

#[test]
fn established_feed_survives_contention_but_revocation_still_closes_it() {
    let (leases, lease, scope, _) = live_scope();
    let _guard = Guard(CURRENT.with(|slot| slot.borrow_mut().replace(scope.clone())));
    let (mut socket, _peer) = UnixStream::pair().unwrap();
    let running = Arc::new(AtomicBool::new(true));
    leases.with_registry_locked(|| {
        assert_eq!(scope.refusal(), Some(ErrorCode::LeaseBusy));
        assert!(!crate::api::server::should_stop_connection(&mut socket, &running).unwrap());
    });
    with_revocation_under_contention(&leases, lease, || {
        assert!(crate::api::server::should_stop_connection(&mut socket, &running).unwrap());
    });
}

/// Inject failed fresh process facts at the same continuation verification boundary.
pub(crate) fn ancestry_lost(scope: &Scope) -> Scope {
    Scope(Arc::new(ScopeData {
        fact_reader: Some(|pid| {
            let (parent, birth) = crate::platform::live_process_facts(pid)?;
            Some((parent, birth.wrapping_add(1)))
        }),
        ..scope.0.as_ref().clone()
    }))
}

#[test]
fn established_feed_closes_on_fresh_ancestry_loss() {
    let (leases, _lease, scope, _) = live_scope();
    let _guard = Guard(CURRENT.with(|slot| slot.borrow_mut().replace(scope.clone())));
    let (mut socket, _peer) = UnixStream::pair().unwrap();
    let running = Arc::new(AtomicBool::new(true));
    leases.with_registry_locked(|| {
        assert!(!crate::api::server::should_stop_connection(&mut socket, &running).unwrap());
        CURRENT.with(|slot| *slot.borrow_mut() = Some(ancestry_lost(&scope)));
        assert!(crate::api::server::should_stop_connection(&mut socket, &running).unwrap());
    });
}

/// Revoke from the owner thread while the table is held; release after unlock.
pub(crate) fn with_revocation_under_contention(
    leases: &SessionLeases,
    lease: Lease,
    check: impl FnOnce(),
) {
    std::thread::scope(|threads| {
        let (start, started) = std::sync::mpsc::sync_channel(1);
        let (revoked, published) = std::sync::mpsc::sync_channel(1);
        let (finish, unlocked) = std::sync::mpsc::sync_channel(1);
        let worker = threads.spawn(move || {
            started.recv_timeout(Duration::from_secs(5)).unwrap();
            lease.revoke_for_test();
            revoked.send(()).unwrap();
            unlocked.recv_timeout(Duration::from_secs(5)).unwrap();
            drop(lease);
        });
        leases.with_registry_locked(|| {
            start.send(()).unwrap();
            published.recv_timeout(Duration::from_secs(5)).unwrap();
            check();
        });
        finish.send(()).unwrap();
        worker.join().unwrap();
    });
}

fn read_reply(id: &str) -> String {
    serde_json::json!({"id":id,"result":{"type":"pane_read","read":{
        "pane_id":"fixture-pane","workspace_id":"fixture-workspace","tab_id":"fixture-tab",
        "source":"recent","format":"text","text":"hit","revision":1,"truncated":false
    }}})
    .to_string()
}

fn next_app_message(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::api::ApiRequestMessage>,
) -> crate::api::ApiRequestMessage {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap()
        })
}

#[test]
fn feed_rechecks_revocation_after_poll_reply_before_writing() {
    let (_, lease, scope, _) = live_scope();
    let (server, mut client) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (done, completed) = std::sync::mpsc::channel();
    let running = Arc::new(AtomicBool::new(true));
    let worker = std::thread::spawn(move || {
        let _guard = Guard(CURRENT.with(|slot| slot.borrow_mut().replace(scope)));
        let params = serde_json::from_value(serde_json::json!({"subscriptions":[{
            "type":"pane.output_matched","pane_id":"fixture-pane","source":"recent",
            "match":{"type":"substring","value":"hit"}
        }]}))
        .unwrap();
        let result = crate::api::server::stream_subscriptions_with(
            server,
            "feed".into(),
            params,
            &tx,
            &EventHub::default(),
            &running,
            || lease.revoke_for_test(),
        );
        done.send(result).unwrap();
    });
    // Initial subscription probe, then the poll that produces its event.
    for stage in ["probe", "read"] {
        let message = next_app_message(&mut rx);
        assert_eq!(message.request.id, format!("feed:sub:0:{stage}"));
        assert!(matches!(message.request.method, Method::PaneRead(_)));
        message
            .respond_to
            .send(read_reply(&message.request.id))
            .unwrap();
    }
    completed
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    worker.join().unwrap();
    let mut bytes = String::new();
    std::io::Read::read_to_string(&mut client, &mut bytes).unwrap();
    assert_eq!(
        bytes.lines().count(),
        1,
        "revoked poll leaked an event: {bytes}"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(bytes.trim()).unwrap()["result"]["type"],
        "subscription_started"
    );
}

#[test]
fn held_wait_and_event_read_polls_survive_contention_and_end_on_revoke() {
    for events in [false, true] {
        for revoked in [false, true] {
            let (leases, lease, scope, peer) = live_scope();
            let (mut server, _client) = UnixStream::pair().unwrap();
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let (done, completed) = std::sync::mpsc::channel();
            let hub = EventHub::default();
            let worker_hub = hub.clone();
            let worker = std::thread::spawn(move || {
                let _guard = Guard(CURRENT.with(|slot| slot.borrow_mut().replace(scope)));
                let running = Arc::new(AtomicBool::new(true));
                let result = if events {
                    let params =
                        serde_json::from_value(serde_json::json!({"after":0,"wait_ms":5000}))
                            .unwrap();
                    crate::api::events_read::read_events(
                        "read".into(),
                        params,
                        &mut server,
                        &worker_hub,
                        &running,
                        &tx,
                        peer,
                    )
                } else {
                    let params = serde_json::from_value(serde_json::json!({"pane_id":"fixture-pane","source":"recent","match":{"type":"substring","value":"hit"},"timeout_ms":5000})).unwrap();
                    crate::api::wait::wait_for_output(
                        "wait".into(),
                        params,
                        &mut server,
                        &tx,
                        &running,
                    )
                };
                done.send(result).unwrap();
            });
            leases.with_registry_locked(|| {
                let message = next_app_message(&mut rx);
                assert!(matches!(
                    (&message.request.method, events),
                    (Method::WaitHold(_, _), true) | (Method::PaneRead(_), false)
                ));
                let (_, app_rx) = tokio::sync::mpsc::unbounded_channel();
                let mut app = crate::app::App::new(
                    &crate::config::Config::default(),
                    true,
                    None,
                    app_rx,
                    EventHub::default(),
                );
                app.current_api_scope = message.scope.clone();
                app.current_api_peer_pid = message.peer_pid;
                app.current_api_peer_birth = Some(message.peer_birth);
                app.current_api_peer_uid = Some(message.peer_uid);
                app.runtime_config.uid_policy = UidPolicy {
                    operator: peer.uid.unwrap(),
                    dedicated: Ok(None).into(),
                };
                assert_eq!(
                    app.peer_uid_refusal(&message.request),
                    None,
                    "held poll repeated fresh admission"
                );
                if revoked {
                    lease.revoke_for_test();
                }
                if events {
                    hub.push(crate::api::schema::EventEnvelope {
                        event: crate::api::schema::EventKind::WorkspaceClosed,
                        data: crate::api::schema::EventData::WorkspaceClosed {
                            workspace_id: "fixture-workspace".into(),
                        },
                    });
                    message
                        .respond_to
                        .send(serde_json::json!({"result":{"wait_id":"fixture-wait"}}).to_string())
                        .unwrap();
                } else {
                    message
                        .respond_to
                        .send(read_reply(&message.request.id))
                        .unwrap();
                }
                let result = completed
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    result.is_none(),
                    revoked,
                    "held operation returned wrong outcome: {result:?}"
                );
                if let Some(result) = result {
                    assert!(serde_json::from_str::<serde_json::Value>(&result)
                        .unwrap()
                        .get("error")
                        .is_none());
                }
            });
            worker.join().unwrap();
        }
    }
}
