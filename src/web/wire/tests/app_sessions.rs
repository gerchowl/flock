//! Socket mint through the real dispatcher, followed by real WebSocket lifecycle frames.
use super::*;
use crate::{api::EventHub, app::App, structured::identity::UidPolicy};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    sync::atomic::AtomicBool,
};

async fn fixture() -> (Server, App, Owner) {
    let clock = TestClock::new();
    fixture_clocks(clock.clone(), clock).await
}
async fn fixture_clocks(clock: TestClock, ttl: TestClock) -> (Server, App, Owner) {
    fixture_store_clock(clock, Arc::new(AppClock::new(Arc::new(ttl)))).await
}
async fn fixture_store_clock(
    clock: TestClock,
    ttl: Arc<dyn crate::structured::permissions::auth::tokens::app_sessions::Clock>,
) -> (Server, App, Owner) {
    let root = crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let owner = Owner::default();
    let store = owner.get(root.path(), ttl).unwrap();
    let wake = Arc::new(tokio::sync::Notify::new());
    let core = Core::from_store(store, Arc::new(clock.clone()), wake.clone()).unwrap();
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bind = socket.local_addr().unwrap();
    let router = core.router();
    let task = tokio::spawn(async move {
        axum::serve(socket, router).await.unwrap();
    });
    let mut app = App::new(
        &crate::config::Config::default(),
        true,
        None,
        tokio::sync::mpsc::unbounded_channel().1,
        EventHub::default(),
    );
    app.client_auth = owner.clone();
    app.runtime_config.uid_policy = UidPolicy {
        operator: crate::platform::peer::effective_uid(),
        dedicated: Ok(None).into(),
    };
    (
        Server {
            core: Arc::new(core),
            bind,
            task,
            clock,
            _root: root,
            wake,
        },
        app,
        owner,
    )
}
async fn dispatch(app: &mut App, params: Value, alter: impl FnOnce(&mut App)) -> Value {
    let (socket, mut client) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let worker = std::thread::spawn(move || {
        crate::api::server::handle_connection(
            socket,
            &tx,
            &EventHub::default(),
            &Arc::new(AtomicBool::new(true)),
            None,
            &Mutex::default(),
            &crate::api::client_listener::Health::default(),
            &Default::default(),
        )
        .unwrap()
    });
    writeln!(
        client,
        "{}",
        json!({"id":"app-mint-fixture","method":"client.app.mint","params":params})
    )
    .unwrap();
    let message = within(rx.recv())
        .await
        .expect("mint did not reach socket dispatcher");
    app.current_api_peer_pid = message.peer_pid;
    app.current_api_peer_uid = Some(message.peer_uid);
    app.current_api_peer_birth = Some(message.peer_birth);
    alter(app);
    let reply = app.handle_api_request(message.request);
    message.respond_to.send(reply).unwrap();
    let mut line = String::new();
    BufReader::new(client).read_line(&mut line).unwrap();
    worker.join().unwrap();
    serde_json::from_str(&line).unwrap()
}
async fn mint(app: &mut App) -> Bearer {
    let reply = dispatch(app, json!({}), |_| {}).await;
    serde_json::from_value(reply["result"]["issued"]["token"].clone())
        .unwrap_or_else(|_| panic!("mint refused: {reply}"))
}
#[test]
fn socket_mint_is_fixed_scoped_shared_memory_only_and_renews_independently() {
    runtime(async {
        let (server, mut app, owner) = fixture().await;
        let token = mint(&mut app).await;
        let store = owner
            .get(
                server._root.path(),
                Arc::new(AppClock::new(Arc::new(server.clock.clone()))),
            )
            .unwrap();
        let session = store.authenticate(&token).unwrap();
        assert_eq!(
            session.info.scopes,
            [Scope::Read, Scope::Send, Scope::Answer, Scope::Operate].into()
        );
        assert!(TokenStore::open(server._root.path())
            .unwrap()
            .authenticate(&token)
            .is_err());
        let mut ws = Socket::open(&server).await;
        let welcome = ws.hello(WIRE_JSON_VERSION, &token.0).await;
        assert_eq!(
            welcome["capabilities"],
            json!(["read", "send", "answer", "operate"])
        );
        let renewed = mint(&mut app).await;
        assert_ne!(token, renewed);
        assert!(store.authenticate(&token).is_ok());
        let files = TokenStore::open(server._root.path()).unwrap();
        let issued = files
            .issue(
                &app.client_token_authority().unwrap(),
                "browser",
                [Scope::Read].into(),
            )
            .unwrap();
        assert!(
            store.authenticate(&issued.token).is_ok(),
            "file source remains supported"
        );
        assert_eq!(
            files
                .list(&app.client_token_authority().unwrap())
                .unwrap()
                .len(),
            1
        );
        within(server.core.shutdown()).await.unwrap();
        assert!(store.authenticate(&token).is_err());
        assert!(store.authenticate(&renewed).is_err());
        assert!(store.authenticate(&issued.token).is_ok());
        server.finish().await;
    });
}
#[test]
fn socket_mint_refuses_caller_supplied_parameters() {
    runtime(async {
        let (server, mut app, _) = fixture().await;
        for params in [
            json!({"scopes":["read"]}),
            json!({"pid":1}),
            json!({"origin_port":1234}),
        ] {
            assert_eq!(
                dispatch(&mut app, params, |_| {}).await["error"]["code"],
                "invalid_params"
            );
        }
        server.finish().await;
    });
}
#[test]
fn socket_mint_refuses_unknown_peers() {
    runtime(async {
        let (server, mut app, _) = fixture().await;
        assert_eq!(
            dispatch(&mut app, json!({}), |app| app.current_api_peer_uid =
                Some(None))
            .await["error"]["code"],
            "operator_only"
        );
        assert_eq!(
            dispatch(&mut app, json!({}), |app| app.current_api_peer_pid = None).await["error"]
                ["code"],
            "operator_only"
        );
        server.finish().await;
    });
}
#[test]
fn socket_mint_refuses_leased_agent_peers() {
    runtime(async {
        let (server, mut app, _) = fixture().await;
        let mut lease = app
            .session_leases
            .reserve("agent_atlas_app", "app-fixture", None)
            .unwrap();
        lease.bind_test_peer(std::process::id());
        assert_eq!(
            dispatch(&mut app, json!({}), |_| {}).await["error"]["code"],
            "operator_only"
        );
        server.finish().await;
    });
}
#[test]
fn socket_mint_start_identity_mismatch_is_typed_before_generic_recheck() {
    runtime(async {
        let (server, mut app, _) = fixture().await;
        let reply = dispatch(&mut app, json!({}), |app| {
            app.current_api_peer_birth =
                Some(Some(app.current_api_peer_birth.flatten().unwrap() ^ 1));
        })
        .await;
        assert_eq!(reply["error"]["code"], "peer_process_changed");
        server.finish().await;
    });
}
#[test]
fn ttl_expires_idle_socket_at_injected_deadline_with_typed_close() {
    runtime(async {
        let (server, mut app, _) = fixture().await;
        let token = mint(&mut app).await;
        let mut ws = Socket::open(&server).await;
        ws.hello(WIRE_JSON_VERSION, &token.0).await;
        server.clock.deadline(12 * 60 * 60 * 1000).await;
        server.clock.advance(12 * 60 * 60 * 1000);
        assert_eq!(
            close_detail(&mut ws).await,
            (AUTH_REVOKED_CLOSE, "client transport ended".into())
        );
        assert!(server.core.shared.store.authenticate(&token).is_err());
        server.finish().await;
    });
}
#[test]
fn real_minting_child_exit_closes_idle_socket_without_a_command() {
    runtime(async {
        let (server, mut app, _) = fixture().await;
        let request =
            json!({"id":"child-app-mint","method":"client.app.mint","params":{}}).to_string();
        let mut peer = crate::platform::process_exit::tests::Peer::request(Some(&request));
        let socket = peer.take_stream();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let worker = std::thread::spawn(move || {
            crate::api::server::handle_connection(
                socket,
                &tx,
                &EventHub::default(),
                &Arc::new(AtomicBool::new(true)),
                None,
                &Mutex::default(),
                &crate::api::client_listener::Health::default(),
                &Default::default(),
            )
            .unwrap()
        });
        let message = within(rx.recv()).await.unwrap();
        assert_eq!(message.peer_pid, peer.credentials.pid);
        app.current_api_peer_pid = message.peer_pid;
        app.current_api_peer_uid = Some(message.peer_uid);
        app.current_api_peer_birth = Some(message.peer_birth);
        let reply = app.handle_api_request(message.request);
        let value: Value = serde_json::from_str(&reply).unwrap();
        message.respond_to.send(reply).unwrap();
        worker.join().unwrap();
        let token =
            serde_json::from_value::<Bearer>(value["result"]["issued"]["token"].clone()).unwrap();
        let mut ws = Socket::open(&server).await;
        ws.hello(WIRE_JSON_VERSION, &token.0).await;
        server.clock.deadline(12 * 60 * 60 * 1000).await;
        let mut sibling = Socket::open(&server).await;
        sibling.hello(WIRE_JSON_VERSION, &token.0).await;
        peer.exit();
        assert_eq!(
            close_detail(&mut ws).await,
            (AUTH_REVOKED_CLOSE, "client transport ended".into())
        );
        assert_eq!(
            close_detail(&mut sibling).await,
            (AUTH_REVOKED_CLOSE, "client transport ended".into())
        );
        assert!(server.core.shared.store.authenticate(&token).is_err());
        server.finish().await;
    });
}
#[test]
fn shutdown_revokes_both_sockets_and_same_owner_cannot_reuse_tokens() {
    runtime(async {
        let (server, mut app, _) = fixture().await;
        let token = mint(&mut app).await;
        let mut first = Socket::open(&server).await;
        let mut second = Socket::open(&server).await;
        first.hello(WIRE_JSON_VERSION, &token.0).await;
        second.hello(WIRE_JSON_VERSION, &token.0).await;
        server.clock.deadline_count(12 * 60 * 60 * 1000, 2).await;
        within(server.core.shutdown()).await.unwrap();
        assert!(matches!(
            first.receive().await,
            Received::Close(AUTH_REVOKED_CLOSE)
        ));
        assert!(matches!(
            second.receive().await,
            Received::Close(AUTH_REVOKED_CLOSE)
        ));
        assert!(server.core.shared.store.authenticate(&token).is_err());
        server.finish().await;
    });
}

async fn close_detail(ws: &mut Socket) -> (u16, String) {
    within(async {
        assert_eq!(ws.0.read_u8().await.unwrap(), 0x88);
        let size = ws.0.read_u8().await.unwrap();
        assert!(size < 126, "bounded unmasked server close");
        let mut bytes = vec![0; size as usize];
        ws.0.read_exact(&mut bytes).await.unwrap();
        (
            u16::from_be_bytes([bytes[0], bytes[1]]),
            String::from_utf8(bytes[2..].to_vec()).unwrap(),
        )
    })
    .await
}
#[test]
fn expiry_flushes_inflight_auth_revoked_ack_before_the_close() {
    runtime(async {
        let (mut server, mut app, _) = fixture().await;
        let token = mint(&mut app).await;
        let mut ws = Socket::open(&server).await;
        ws.hello(WIRE_JSON_VERSION, &token.0).await;
        server.clock.deadline(12 * 60 * 60 * 1000).await;
        ws.send(json!({"t":"cmd","cmd_id":"expired-app-command","resume_only":false,"verb":"agent.send","args":{"target":"fixture","text":"hello","submit":true}})).await;
        within(server.wake.notified()).await;
        server.clock.advance(12 * 60 * 60 * 1000);
        Arc::get_mut(&mut server.core)
            .unwrap()
            .service_commands(&mut app);
        let ack = ws.text().await;
        assert_eq!(ack["t"], "ack");
        assert_eq!(ack["error"]["code"], "auth_revoked");
        assert_eq!(
            close_detail(&mut ws).await,
            (AUTH_REVOKED_CLOSE, "client transport ended".into())
        );
        server.finish().await;
    });
}

#[test]
fn shutdown_revoke_failure_still_stops_drains_and_closes_live_sessions() {
    let logs = crate::logging::capture_logs(|| {
        runtime(async {
            let (server, mut app, _) = fixture().await;
            let token = mint(&mut app).await;
            let mut socket = Socket::open(&server).await;
            socket.hello(WIRE_JSON_VERSION, &token.0).await;
            server.clock.deadline(12 * 60 * 60 * 1000).await;
            // A non-private directory deterministically refuses token mutations.
            let path = server._root.path().join("client-tokens");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();
            assert!(within(server.core.shutdown()).await.is_err());
            assert!(*server.core.shared.stop.borrow());
            assert!(server.core.shared.clients.lock().unwrap().is_empty());
            assert_eq!(
                server.core.shared.slots.available_permits(),
                CONNECTIONS as usize
            );
            assert!(matches!(
                socket.receive().await,
                Received::Close(AUTH_REVOKED_CLOSE)
            ));
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
            server.finish().await;
        })
    });
    assert!(logs.contains("client.app.revocation_failed"), "{logs}");
}

#[test]
fn socket_mint_reports_remaining_lifetime_from_the_store_clock() {
    runtime(async {
        let (server, mut app, owner) = fixture().await;
        server.clock.advance(1731);
        let issue_time = Duration::ZERO;
        let reply = dispatch(&mut app, json!({}), |_| {}).await;
        let issued = &reply["result"]["issued"];
        let token: Bearer = serde_json::from_value(issued["token"].clone()).unwrap();
        let store = owner
            .get(
                server._root.path(),
                Arc::new(AppClock::new(Arc::new(server.clock.clone()))),
            )
            .unwrap();
        let session = store.authenticate(&token).unwrap();
        let expected = u64::try_from(
            session
                .app()
                .unwrap()
                .expires
                .checked_sub(issue_time)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        assert_eq!(issued["info"]["expires_in_ms"].as_u64(), Some(expected));
        within(server.core.shutdown()).await.unwrap();
        server.finish().await;
    });
}

#[test]
fn suspend_gap_expires_a_mint_and_its_idle_socket_without_awake_time() {
    runtime(async {
        let ttl = TestClock::new();
        let (server, mut app, owner) = fixture_clocks(TestClock::new(), ttl.clone()).await;
        let awake = server.clock.now();
        let reply = dispatch(&mut app, json!({}), |_| {}).await;
        let issued = &reply["result"]["issued"];
        let token: Bearer = serde_json::from_value(issued["token"].clone()).unwrap();
        let ttl_ms = u64::try_from(
            crate::structured::permissions::auth::tokens::app_sessions::TTL.as_millis(),
        )
        .unwrap();
        assert_eq!(issued["info"]["expires_in_ms"], ttl_ms);
        let store = owner
            .get(
                server._root.path(),
                Arc::new(AppClock::new(Arc::new(ttl.clone()))),
            )
            .unwrap();
        let mut socket = Socket::open(&server).await;
        socket.hello(WIRE_JSON_VERSION, &token.0).await;
        server.clock.deadline(15_000).await;
        ttl.advance(ttl_ms);
        assert_eq!(
            server.clock.now(),
            awake,
            "no awake time passed during suspend"
        );
        assert!(store.authenticate(&token).is_err());
        assert!(matches!(
            socket.receive().await,
            Received::Close(AUTH_REVOKED_CLOSE)
        ));
        let renewed = dispatch(&mut app, json!({}), |_| {}).await;
        assert_eq!(renewed["result"]["issued"]["info"]["expires_in_ms"], ttl_ms);
        within(server.core.shutdown()).await.unwrap();
        server.finish().await;
    });
}

struct FailedClock(bool);
impl crate::structured::permissions::auth::tokens::app_sessions::Clock for FailedClock {
    fn now(&self) -> io::Result<Duration> {
        if self.0 {
            Err(io::Error::other("clock fixture unavailable"))
        } else {
            Ok(Duration::ZERO)
        }
    }
    fn until(
        &self,
        _: Duration,
    ) -> crate::structured::permissions::auth::tokens::app_sessions::Wait<'_> {
        Box::pin(async { Err(io::Error::other("timer fixture unavailable")) })
    }
}
#[test]
fn unavailable_ttl_clock_refuses_socket_mint_without_a_fallback() {
    runtime(async {
        let (server, mut app, _) =
            fixture_store_clock(TestClock::new(), Arc::new(FailedClock(true))).await;
        let reply = dispatch(&mut app, json!({}), |_| {}).await;
        assert_eq!(reply["error"]["code"], "internal_error");
        assert!(reply.get("result").is_none());
        within(server.core.shutdown()).await.unwrap();
        server.finish().await;
    });
}
#[test]
fn unavailable_expiry_timer_closes_without_claiming_credential_revocation() {
    runtime(async {
        let (server, mut app, owner) =
            fixture_store_clock(TestClock::new(), Arc::new(FailedClock(false))).await;
        let token = mint(&mut app).await;
        let mut socket = Socket::open(&server).await;
        socket.send(json!({"t":"hello","protocol":WIRE_JSON_VERSION,"client":{"name":"fixture","version":"1"},"token":token.0})).await;
        assert!(matches!(
            socket.receive().await,
            Received::Close(GOING_AWAY_CLOSE)
        ));
        let store = owner
            .get(server._root.path(), Arc::new(FailedClock(false)))
            .unwrap();
        assert!(store.authenticate(&token).is_ok());
        within(server.core.shutdown()).await.unwrap();
        server.finish().await;
    });
}
