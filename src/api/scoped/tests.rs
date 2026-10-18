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
        probe: true,
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
    for mode in ["valid","missing","wrong","malformed"] {
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
            "valid"=>serde_json::json!({"request":{"id":"scope","method":"ping","params":{}},"credential":secret}),
            "missing"=>serde_json::json!({"request":{"id":"scope","method":"ping","params":{}}}),
            "wrong"=>serde_json::json!({"request":{"id":"scope","method":"ping","params":{}},"credential":"wrong"}),
            _=>serde_json::json!({"request":{"id":"scope","method":"ping","params":{"unexpected":secret}},"credential":secret,"extra":true}),
        };
        let task=std::thread::spawn(move || crate::api::server::handle_connection(server,&tx,&EventHub::default(),&Arc::new(AtomicBool::new(true)),None,&Mutex::default()).unwrap());
        writeln!(client,"{payload}").unwrap();
        if mode!="malformed" {
            let msg=tokio::time::timeout(Duration::from_secs(5),app.api_rx.recv()).await.unwrap().unwrap();
            assert!(app.respond_scope_probe(&msg));
        }
        let mut response=String::new();
        BufReader::new(&mut client).read_line(&mut response).unwrap();
        task.join().unwrap();
        assert!(!response.contains(&secret));
        assert_eq!(serde_json::from_str::<serde_json::Value>(&response).unwrap().get("error").is_none(),mode=="valid","{response}");
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
