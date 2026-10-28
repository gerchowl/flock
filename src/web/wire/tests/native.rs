//! Native requests cross the real HTTP listener, kernel capture and app-owner mailbox.
use super::*;
use crate::{app::App, web::local_listener};
use std::os::unix::fs::PermissionsExt;
use tokio::net::UnixStream;

struct Native {
    server: Server,
    app: App,
    listener: local_listener::Listener,
    root: crate::test_support::TempDir,
}
impl Native {
    async fn new() -> Self {
        Self::with_facts(None).await
    }
    async fn with_facts(facts: Option<crate::platform::peer::Credentials>) -> Self {
        Self::using(facts, |clock| Arc::new(clock)).await
    }
    async fn using(
        facts: Option<crate::platform::peer::Credentials>,
        wrap: impl FnOnce(TestClock) -> Arc<dyn Clock>,
    ) -> Self {
        let (server, app, _) = super::app_sessions::fixture_using(false, wrap).await;
        let root = crate::test_support::TempDir::socket();
        let mut router = server.core.local_router();
        if let Some(facts) = facts {
            // Reader-failure seam: only kernel facts vary; the real owner policy still runs.
            router = router.layer(axum::Extension(axum::extract::ConnectInfo(facts)));
        }
        let mut listener =
            local_listener::Listener::prepare(root.path().join("native.sock"), None, router)
                .unwrap();
        listener.activate();
        Self {
            server,
            app,
            listener,
            root,
        }
    }
    fn path(&self) -> PathBuf {
        self.root.path().join("native.sock")
    }
    async fn drive<T>(&mut self, work: impl Future<Output = T>) -> T {
        tokio::pin!(work);
        within(async {
            loop {
                tokio::select! { biased;
                    out = &mut work => return out,
                    _ = self.server.wake.notified() => {
                        let core = Arc::get_mut(&mut self.server.core).unwrap();
                        core.service(&self.app.state.structured_agents, &mut self.app.structured_workers, self.server._root.path());
                        core.service_commands(&mut self.app);
                    },
                }
            }
        }).await
    }
    async fn upgrade(&mut self, uri: &str) -> (Socket<UnixStream>, String) {
        let path = self.path();
        let uri = uri.to_owned();
        self.drive(async move {
            let mut socket = UnixStream::connect(path).await.unwrap();
            socket.write_all(format!("GET {uri} HTTP/1.1\r\nHost: fixture.example\r\nX-Peer-Pid: 1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n").as_bytes()).await.unwrap(); // gitleaks:allow -- RFC 6455 public handshake example nonce
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                header.push(socket.read_u8().await.unwrap());
                assert!(header.len() < 8192);
            }
            (Socket(socket), String::from_utf8(header).unwrap())
        })
        .await
    }
    async fn open(&mut self) -> Socket<UnixStream> {
        let (socket, header) = self.upgrade("/v1/ws").await;
        assert!(header.starts_with("HTTP/1.1 101"), "{header}");
        socket
    }
    async fn mint(&mut self) -> Bearer {
        super::app_sessions::mint(&mut self.app).await
    }
    async fn finish(mut self) {
        self.listener.stop().await;
        self.server.finish().await;
    }
}
async fn close_detail<S: tokio::io::AsyncRead + Unpin>(socket: &mut Socket<S>) -> (u16, String) {
    within(async {
        assert_eq!(socket.0.read_u8().await.unwrap(), 0x88);
        let len = socket.0.read_u8().await.unwrap() as usize;
        let mut bytes = vec![0; len];
        socket.0.read_exact(&mut bytes).await.unwrap();
        (
            u16::from_be_bytes(bytes[..2].try_into().unwrap()),
            String::from_utf8(bytes[2..].to_vec()).unwrap(),
        )
    })
    .await
}
#[tokio::test]
async fn native_mint_hello_and_generated_command_cross_owner_path() {
    let mut f = Native::new().await;
    let token = f.mint().await;
    let mut socket = f.open().await;
    assert_eq!(
        socket.hello(WIRE_JSON_VERSION, &token.0).await["capabilities"],
        json!(["read", "send", "answer", "operate"])
    );
    socket.send(json!({"t":"cmd","cmd_id":"native-refusal","resume_only":false,"verb":"agent.send","args":{"target":"absent","text":"hello","submit":false}})).await;
    let ack = f.drive(socket.text()).await;
    assert_eq!(ack["t"], "ack");
    assert_eq!(ack["error"]["code"], "structured_submit_required");
    socket
        .send(json!({"t":"sub","sub_id":"native-fleet","stream":"fleet"}))
        .await;
    let frame = f.drive(socket.text()).await;
    assert_eq!(frame["sub_id"], "native-fleet");
    assert_eq!(
        std::fs::metadata(f.path()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    f.finish().await;
}
#[tokio::test]
async fn tcp_app_bearer_refuses_with_exact_peer_reason_without_welcome() {
    let mut f = Native::new().await;
    let token = f.mint().await;
    let mut socket = Socket::open(&f.server).await;
    socket
        .send(json!({"t":"hello","protocol":WIRE_JSON_VERSION,"client":{"name":"fixture","version":"1"},"token":token.0}))
        .await;
    assert_eq!(
        close_detail(&mut socket).await,
        (AUTH_REFUSED_CLOSE, "peer_process_changed".into())
    );
    f.finish().await;
}
#[tokio::test]
async fn native_rejects_tokenless_and_wrong_bearers_without_registration() {
    let mut f = Native::new().await;
    for token in [None, Some("f".repeat(64))] {
        let mut socket = f.open().await;
        socket
            .send(json!({"t":"hello","protocol":WIRE_JSON_VERSION,"client":{"name":"fixture","version":"1"},"token":token}))
            .await;
        assert_eq!(close_detail(&mut socket).await.0, AUTH_REFUSED_CLOSE);
    }
    assert!(f.server.core.shared.clients.lock().unwrap().is_empty());
    f.finish().await;
}

#[tokio::test]
async fn native_missing_kernel_facts_have_exact_http_refusals_and_precedence() {
    use crate::platform::peer::{credentials, Credentials};
    let (peer, _client) = std::os::unix::net::UnixStream::pair().unwrap();
    let real = credentials(&peer);
    for (facts, code, message) in [
        (
            Credentials { uid: None, ..real },
            "operator_only",
            "Local operator required",
        ),
        (
            Credentials {
                uid: real.uid.map(|uid| uid.wrapping_add(1)),
                ..real
            },
            "operator_only",
            "Local operator required",
        ),
        (
            Credentials {
                uid: None,
                pid: None,
                birth: None,
            },
            "operator_only",
            "Local operator required",
        ),
        (
            Credentials { pid: None, ..real },
            "peer_process_changed",
            "Peer process identity unavailable or changed",
        ),
        (
            Credentials {
                birth: None,
                ..real
            },
            "peer_process_changed",
            "Peer process identity unavailable or changed",
        ),
        (
            Credentials {
                birth: real.birth.map(|birth| birth.wrapping_add(1)),
                ..real
            },
            "peer_process_changed",
            "Peer process identity unavailable or changed",
        ),
    ] {
        let mut f = Native::with_facts(Some(facts)).await;
        let (mut socket, headers) = f.upgrade("/v1/ws").await;
        assert!(headers.starts_with("HTTP/1.1 403"), "{headers}");
        assert!(
            headers.contains("content-type: application/json"),
            "{headers}"
        );
        assert!(headers.contains("cache-control: no-store"), "{headers}");
        assert!(headers.contains("connection: close"), "{headers}");
        let mut body = String::new();
        within(socket.0.read_to_string(&mut body)).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            json!({"error":{"code":code,"message":message}})
        );
        assert!(f.server.core.shared.clients.lock().unwrap().is_empty());
        f.finish().await;
    }
}
#[tokio::test]
async fn native_extra_paths_never_upgrade() {
    let mut f = Native::new().await;
    for uri in ["/v1/client/exchange", "/", "/other"] {
        let (_, headers) = f.upgrade(uri).await;
        assert!(headers.starts_with("HTTP/1.1 404"), "{headers}");
    }
    f.finish().await;
}
#[tokio::test]
async fn pending_hello_budgets_are_separate_and_expire_together_without_leaks() {
    for tcp_first in [true, false] {
        let mut f = Native::new().await;
        let mut tcp = Vec::new();
        let mut local = Vec::new();
        for _ in 0..PENDING_HELLOS {
            if tcp_first {
                tcp.push(Socket::open(&f.server).await);
            } else {
                local.push(f.open().await);
            }
        }
        f.server.clock.deadline_count(5_000, PENDING_HELLOS).await;
        let token = f.mint().await;
        let mut admitted = if tcp_first {
            let mut s = f.open().await;
            s.hello(WIRE_JSON_VERSION, &token.0).await;
            Some(s)
        } else {
            None
        };
        let _browser = if !tcp_first {
            let files = TokenStore::open(f.server._root.path()).unwrap();
            let issued = files
                .issue(
                    &f.app.client_token_authority().unwrap(),
                    "browser",
                    [Scope::Read].into(),
                )
                .unwrap();
            let mut socket = Socket::open(&f.server).await;
            socket.hello(WIRE_JSON_VERSION, &issued.token.0).await;
            Some(socket)
        } else {
            None
        };
        if tcp_first {
            let (_, header) = Socket::upgrade(&f.server).await;
            assert!(header.starts_with("HTTP/1.1 503"), "{header}");
        } else {
            let (mut socket, header) = f.upgrade("/v1/ws").await;
            assert!(header.starts_with("HTTP/1.1 503"), "{header}");
            let mut body = String::new();
            within(socket.0.read_to_string(&mut body)).await.unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&body).unwrap()["error"]["code"],
                "preauth_capacity"
            );
        }
        f.server.clock.advance(5_000);
        for socket in &mut tcp {
            assert!(matches!(
                socket.receive().await,
                Received::Close(GOING_AWAY_CLOSE)
            ));
        }
        for socket in &mut local {
            assert!(matches!(
                socket.receive().await,
                Received::Close(GOING_AWAY_CLOSE)
            ));
        }
        if let Some(socket) = admitted.as_mut() {
            socket.send(json!({"t":"cmd","cmd_id":"still-alive","verb":"unknown","args":{},"resume_only":false})).await;
            assert_eq!(socket.text().await["error"]["code"], "server_version_gap");
        }
        f.finish().await;
    }
}

fn file_token(f: &mut Native, name: &str) -> Bearer {
    TokenStore::open(f.server._root.path())
        .unwrap()
        .issue(
            &f.app.client_token_authority().unwrap(),
            name,
            [Scope::Read].into(),
        )
        .unwrap()
        .token
}
#[tokio::test]
async fn renewed_mints_have_independent_four_socket_budgets() {
    let mut f = Native::new().await;
    let first = f.mint().await;
    let second = f.mint().await;
    let mut sockets = Vec::new();
    for token in [&first, &second] {
        for _ in 0..CREDENTIAL_CONNECTIONS {
            let mut socket = f.open().await;
            socket.hello(WIRE_JSON_VERSION, &token.0).await;
            sockets.push(socket);
        }
        let mut excess = f.open().await;
        excess.send(json!({"t":"hello","protocol":WIRE_JSON_VERSION,"client":{"name":"fixture","version":"1"},"token":token.0})).await;
        assert!(matches!(
            excess.receive().await,
            Received::Close(GOING_AWAY_CLOSE)
        ));
    }
    assert_eq!(f.server.core.shared.clients.lock().unwrap().len(), 8);
    f.finish().await;
    for socket in &mut sockets {
        assert!(matches!(
            socket.receive().await,
            Received::Close(AUTH_REVOKED_CLOSE)
        ));
    }
}
#[tokio::test]
async fn one_file_credential_has_four_slots_across_both_transports() {
    let mut f = Native::new().await;
    let _mint = f.mint().await;
    let token = file_token(&mut f, "both");
    let mut local = Vec::new();
    let mut tcp = Vec::new();
    for _ in 0..2 {
        let mut socket = f.open().await;
        socket.hello(WIRE_JSON_VERSION, &token.0).await;
        local.push(socket);
        let mut socket = Socket::open(&f.server).await;
        socket.hello(WIRE_JSON_VERSION, &token.0).await;
        tcp.push(socket);
    }
    let mut fifth = f.open().await;
    fifth.send(json!({"t":"hello","protocol":WIRE_JSON_VERSION,"client":{"name":"fixture","version":"1"},"token":token.0})).await;
    assert!(matches!(
        fifth.receive().await,
        Received::Close(GOING_AWAY_CLOSE)
    ));
    let mut fifth = Socket::open(&f.server).await;
    fifth.send(json!({"t":"hello","protocol":WIRE_JSON_VERSION,"client":{"name":"fixture","version":"1"},"token":token.0})).await;
    assert!(matches!(
        fifth.receive().await,
        Received::Close(GOING_AWAY_CLOSE)
    ));
    assert_eq!(f.server.core.shared.clients.lock().unwrap().len(), 4);
    f.finish().await;
    for socket in &mut local {
        assert!(matches!(socket.receive().await, Received::End));
    }
    for socket in &mut tcp {
        assert!(matches!(socket.receive().await, Received::End));
    }
}
#[tokio::test]
async fn global_sixteen_socket_budget_is_shared_and_a_closed_socket_releases_it() {
    let mut f = Native::new().await;
    let _mint = f.mint().await;
    let tokens: Vec<_> = (0..4)
        .map(|n| file_token(&mut f, &format!("file-{n}")))
        .collect();
    let mut local = Vec::new();
    let mut tcp = Vec::new();
    for token in &tokens {
        for _ in 0..2 {
            let mut socket = f.open().await;
            socket.hello(WIRE_JSON_VERSION, &token.0).await;
            local.push(socket);
            let mut socket = Socket::open(&f.server).await;
            socket.hello(WIRE_JSON_VERSION, &token.0).await;
            tcp.push(socket);
        }
    }
    assert_eq!(f.server.core.shared.clients.lock().unwrap().len(), 16);
    let (_, headers) = f.upgrade("/v1/ws").await;
    assert!(headers.starts_with("HTTP/1.1 503"), "{headers}");
    let (_, headers) = Socket::upgrade(&f.server).await;
    assert!(headers.starts_with("HTTP/1.1 503"), "{headers}");
    drop(local.pop());
    let permit = within(f.server.core.shared.slots.clone().acquire_owned())
        .await
        .unwrap();
    drop(permit);
    let mut replacement = f.open().await;
    replacement.hello(WIRE_JSON_VERSION, &tokens[3].0).await;
    f.finish().await;
}

/// The minting child also creates both bridge connections; inherited client fds
/// let the parent observe the server's revocation after that child has exited.
async fn child_sockets(
    f: &mut Native,
    pending: bool,
) -> (
    crate::platform::process_exit::tests::Peer,
    Bearer,
    Vec<Socket<UnixStream>>,
) {
    use std::os::fd::FromRawFd;
    let request =
        json!({"id":"native-child-mint","method":"client.app.mint","params":{}}).to_string();
    let mut child = if pending {
        crate::platform::process_exit::tests::Peer::bridge_pending(&request, &f.path())
    } else {
        crate::platform::process_exit::tests::Peer::bridge(&request, &f.path())
    };
    let socket = child.take_stream();
    let control = socket.try_clone().unwrap();
    let (send, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let api = std::thread::spawn(move || {
        crate::api::server::handle_connection(
            socket,
            &send,
            &crate::api::EventHub::default(),
            &Arc::new(std::sync::atomic::AtomicBool::new(true)),
            None,
            &Mutex::default(),
            &Default::default(),
            &Default::default(),
        )
        .unwrap()
    });
    let request = within(requests.recv()).await.unwrap();
    assert_eq!(request.peer_pid, child.credentials.pid);
    f.app.current_api_peer_pid = request.peer_pid;
    f.app.current_api_peer_uid = Some(request.peer_uid);
    f.app.current_api_peer_birth = Some(request.peer_birth);
    let reply = f.app.handle_api_request(request.request);
    let value: Value = serde_json::from_str(&reply).unwrap();
    let token: Bearer = serde_json::from_value(value["result"]["issued"]["token"].clone()).unwrap();
    request.respond_to.send(reply).unwrap();
    api.join().unwrap();
    control
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let received =
        tokio::task::spawn_blocking(move || crate::server::handoff::recv_fds(&control, 2));
    let descriptors = f.drive(received).await.unwrap().unwrap();
    let mut sockets = Vec::new();
    for descriptor in descriptors {
        // SAFETY: recv_fds transferred unique ownership of each client descriptor.
        let stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(descriptor) };
        stream.set_nonblocking(true).unwrap();
        sockets.push(Socket(UnixStream::from_std(stream).unwrap()));
    }
    (child, token, sockets)
}
pub(super) async fn child_bridge_and_exit() {
    let mut f = Native::new().await;
    let (mut child, token, mut sockets) = child_sockets(&mut f, false).await;
    assert_eq!(f.server.core.shared.clients.lock().unwrap().len(), 2);
    // A different live process cannot bridge with this child's valid bearer.
    let mut wrong = f.open().await;
    wrong.send(json!({"t":"hello","protocol":WIRE_JSON_VERSION,"client":{"name":"fixture","version":"1"},"token":token.0})).await;
    assert_eq!(
        close_detail(&mut wrong).await,
        (AUTH_REFUSED_CLOSE, "peer_process_changed".into())
    );
    let sibling = f.mint().await;
    let mut healthy = f.open().await;
    healthy.hello(WIRE_JSON_VERSION, &sibling.0).await;
    child.exit_unreaped();
    for socket in &mut sockets {
        assert_eq!(close_detail(socket).await.0, AUTH_REVOKED_CLOSE);
    }
    assert!(f.server.core.shared.store.authenticate(&token).is_err());
    assert!(f.server.core.shared.store.authenticate(&sibling).is_ok());
    healthy.send(json!({"t":"cmd","cmd_id":"sibling-live","verb":"unknown","args":{},"resume_only":false})).await;
    assert_eq!(healthy.text().await["error"]["code"], "server_version_gap");
    f.finish().await;
}

#[tokio::test]
async fn native_owner_refuses_agent_and_mesh_peers_before_upgrade() {
    for mesh in [false, true] {
        let mut f = Native::new().await;
        let _stolen = f.mint().await;
        let (peer, _other) = std::os::unix::net::UnixStream::pair().unwrap();
        let facts = crate::platform::peer::credentials(&peer);
        let mut lease = None;
        if mesh {
            f.app
                .inbound
                .attach(
                    facts.pid.unwrap(),
                    facts.birth.unwrap(),
                    crate::platform::process_start_time,
                )
                .unwrap();
        } else {
            let mut reserved = f
                .app
                .session_leases
                .reserve("fixture-agent", "fixture-session", None)
                .unwrap();
            reserved.bind_test_peer(facts.pid.unwrap());
            lease = Some(reserved);
        }
        let (mut socket, headers) = f.upgrade("/v1/ws").await;
        assert!(headers.starts_with("HTTP/1.1 403"), "{headers}");
        assert!(headers.contains("content-type: application/json"));
        assert!(headers.contains("cache-control: no-store"));
        let mut body = String::new();
        within(socket.0.read_to_string(&mut body)).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            json!({"error":{"code":"operator_only","message":"Local operator required"}})
        );
        assert!(f.server.core.shared.clients.lock().unwrap().is_empty());
        drop(lease);
        f.finish().await;
    }
}

#[tokio::test]
async fn ready_child_exit_refuses_hello_while_notification_consumer_is_gated() {
    let mut f = Native::new().await;
    let (child, token, mut sockets) = child_sockets(&mut f, true).await;
    let session = f.server.core.shared.store.authenticate(&token).unwrap();
    let app = session.app().unwrap().clone();
    let notification = app.revoked.clone();
    let (ready, observed) = tokio::sync::oneshot::channel();
    let (release, held) = tokio::sync::oneshot::channel();
    let store = f.server.core.shared.store.clone();
    let consumer = tokio::spawn(async move {
        app.process.wait().await.unwrap();
        ready.send(()).unwrap();
        held.await.unwrap();
        store.retire(&session).unwrap();
    });
    use crate::structured::permissions::auth::tokens::server::PeerStage;
    let child = Arc::new(Mutex::new(child));
    let exiting = child.clone();
    let crossed = Arc::new(AtomicBool::new(false));
    let checked = crossed.clone();
    f.server
        .core
        .shared
        .store
        .on_peer_stage(PeerStage::Checked, move || {
            checked.store(true, Ordering::SeqCst);
        });
    f.server
        .core
        .shared
        .store
        .on_peer_stage(PeerStage::Authenticated, move || {
            exiting.lock().unwrap().exit_unreaped();
            assert!(
                !*notification.borrow(),
                "the exit consumer is held before retirement"
            );
        });
    for socket in &mut sockets {
        socket.send(json!({"t":"hello","protocol":WIRE_JSON_VERSION,"client":{"name":"fixture","version":"1"},"token":token.0})).await;
        assert_eq!(close_detail(socket).await.0, AUTH_REFUSED_CLOSE);
    }
    within(observed).await.unwrap();
    assert!(
        !crossed.load(Ordering::SeqCst),
        "ready exit crossed the registration fence"
    );
    assert!(f.server.core.shared.clients.lock().unwrap().is_empty());
    release.send(()).unwrap();
    within(consumer).await.unwrap();
    f.finish().await;
}

#[tokio::test]
async fn child_exit_races_both_sides_of_fenced_registration() {
    use crate::structured::permissions::auth::tokens::server::PeerStage;
    for registered in [false, true] {
        let mut f = Native::new().await;
        let (child, token, mut sockets) = child_sockets(&mut f, true).await;
        // Keep both sockets pending: an already-welcomed socket would retire the
        // credential on exit and race the registration fence with an exclusive lock.
        // Idle registered fanout and a held exit consumer have separate fixtures.
        let child = Arc::new(Mutex::new(child));
        let exiting = child.clone();
        let shared = f.server.core.shared.clone();
        let fired = Arc::new(AtomicBool::new(false));
        let observed = fired.clone();
        f.server.core.shared.store.on_peer_stage(
            if registered {
                PeerStage::Registered
            } else {
                PeerStage::Authenticated
            },
            move || {
                assert_eq!(
                    shared.clients.lock().unwrap().len(),
                    usize::from(registered)
                );
                exiting.lock().unwrap().exit_unreaped();
                observed.store(true, Ordering::SeqCst);
            },
        );
        let mut racing = sockets.remove(0);
        racing.send(json!({"t":"hello","protocol":WIRE_JSON_VERSION,"client":{"name":"fixture","version":"1"},"token":token.0})).await;
        assert_eq!(
            close_detail(&mut racing).await.0,
            if registered {
                AUTH_REVOKED_CLOSE
            } else {
                AUTH_REFUSED_CLOSE
            }
        );
        assert!(fired.load(Ordering::SeqCst));
        drop(child);
        f.finish().await;
    }
}

fn browser_file(f: &Native) -> Bearer {
    let token = Bearer("a".repeat(64));
    let path = f.server._root.path().join("client-tokens/browser.token");
    fs::write(
        &path,
        json!({"token":token.0,"principal":"b".repeat(32),"scopes":["read"]}).to_string(),
    )
    .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    token
}
fn management_lock(f: &Native) -> std::fs::File {
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(f.server._root.path().join("client-tokens/.lock"))
        .unwrap();
    file.lock().unwrap();
    file
}
#[tokio::test]
async fn native_browser_hello_waits_for_management_without_a_transport_failure() {
    use crate::structured::permissions::auth::tokens::server::PeerStage;
    let mut f = Native::new().await;
    let token = browser_file(&f);
    let lock = management_lock(&f);
    let waited = Arc::new(AtomicBool::new(false));
    let observed = waited.clone();
    f.server
        .core
        .shared
        .store
        .on_peer_stage(PeerStage::Waiting, move || {
            observed.store(true, Ordering::SeqCst);
            drop(lock);
        });
    let mut socket = f.open().await;
    assert_eq!(
        socket.hello(WIRE_JSON_VERSION, &token.0).await["t"],
        "welcome"
    );
    assert!(waited.load(Ordering::SeqCst));
    f.finish().await;
}
#[tokio::test]
async fn management_wait_keeps_the_original_hello_deadline_and_cannot_register_late() {
    use crate::structured::permissions::auth::tokens::server::PeerStage;
    let mut f = Native::new().await;
    let token = browser_file(&f);
    let lock = management_lock(&f);
    let clock = f.server.clock.clone();
    let waited = Arc::new(AtomicBool::new(false));
    let observed = waited.clone();
    f.server
        .core
        .shared
        .store
        .on_peer_stage(PeerStage::Waiting, move || {
            observed.store(true, Ordering::SeqCst);
            clock.advance(HANDSHAKE_DEADLINE_MS);
        });
    let (completed, producer) = tokio::sync::oneshot::channel();
    f.server
        .core
        .shared
        .store
        .on_peer_stage(PeerStage::LockAcquired, move || {
            completed.send(()).unwrap();
        });
    let mut socket = f.open().await;
    socket.send(json!({"t":"hello","protocol":WIRE_JSON_VERSION,"client":{"name":"fixture","version":"1"},"token":token.0})).await;
    assert!(matches!(
        socket.receive().await,
        Received::Close(GOING_AWAY_CLOSE)
    ));
    assert!(waited.load(Ordering::SeqCst));
    assert!(f.server.core.shared.clients.lock().unwrap().is_empty());
    drop(lock);
    within(producer).await.unwrap();
    assert!(f.server.core.shared.clients.lock().unwrap().is_empty());
    f.finish().await;
}

/// Model the timer notification being unpolled while a blocking callback reads now.
struct DeferredTimer(TestClock);
impl Clock for DeferredTimer {
    fn now(&self) -> Instant {
        self.0.now()
    }
    fn until(&self, _end: Instant) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn registration_rechecks_the_elapsed_deadline_before_insertion() {
    use crate::structured::permissions::auth::tokens::server::PeerStage;
    let mut f = Native::using(None, |clock| Arc::new(DeferredTimer(clock))).await;
    let token = browser_file(&f);
    let clock = f.server.clock.clone();
    f.server
        .core
        .shared
        .store
        .on_peer_stage(PeerStage::Checked, move || {
            clock.advance(HANDSHAKE_DEADLINE_MS);
        });
    let clients = f.server.core.shared.clone();
    let (checked, observed) = tokio::sync::oneshot::channel();
    f.server
        .core
        .shared
        .store
        .on_peer_stage(PeerStage::Registered, move || {
            checked
                .send(clients.clients.lock().unwrap().is_empty())
                .unwrap();
        });
    let mut socket = f.open().await;
    socket.send(json!({"t":"hello","protocol":WIRE_JSON_VERSION,"client":{"name":"fixture","version":"1"},"token":token.0})).await;
    assert!(
        within(observed).await.unwrap(),
        "expired callback inserted a client"
    );
    assert_eq!(close_detail(&mut socket).await.0, GOING_AWAY_CLOSE);
    assert!(f.server.core.shared.clients.lock().unwrap().is_empty());
    f.finish().await;
}

#[tokio::test]
async fn waiting_hellos_do_not_starve_established_subscriptions() {
    use crate::structured::permissions::auth::tokens::server::PeerStage;
    for native_waiters in [false, true] {
        let mut f = Native::new().await;
        let token = browser_file(&f);
        let path = f.server._root.path().join("client-tokens/waiter.token");
        fs::write(
            &path,
            json!({"token":"d".repeat(64),"principal":"e".repeat(32),"scopes":["read"]})
                .to_string(),
        )
        .unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        let mut tcp = Socket::open(&f.server).await;
        tcp.hello(1, &token.0).await;
        let mut native = f.open().await;
        native.hello(1, &token.0).await;
        tcp.send(json!({"t":"sub","sub_id":"live","stream":"fleet"}))
            .await;
        assert_eq!(f.drive(tcp.text()).await["t"], "gap");
        assert_eq!(f.drive(tcp.text()).await["t"], "synced");
        native
            .send(json!({"t":"sub","sub_id":"live","stream":"fleet"}))
            .await;
        assert_eq!(f.drive(native.text()).await["t"], "gap");
        assert_eq!(f.drive(native.text()).await["t"], "synced");
        let lock = management_lock(&f);
        let origin = Instant::now();
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut tcp_waiters = Vec::new();
        let mut unix_waiters = Vec::new();
        for _ in 0..4 {
            let (started, waiting) = tokio::sync::oneshot::channel();
            let trace = events.clone();
            for (stage, label) in [
                (PeerStage::LockAcquired, "LockAcquired"),
                (PeerStage::Checked, "Checked"),
                (PeerStage::Registered, "Registered"),
            ] {
                let trace = events.clone();
                f.server.core.shared.store.on_peer_stage(stage, move || {
                    trace.lock().unwrap().push((label.into(), origin.elapsed()));
                });
            }
            f.server
                .core
                .shared
                .store
                .on_peer_stage(PeerStage::Waiting, move || {
                    trace
                        .lock()
                        .unwrap()
                        .push(("Waiting".into(), origin.elapsed()));
                    let _ = started.send(());
                });
            let hello = json!({"t":"hello","protocol":1,"client":{"name":"fixture","version":"1"},"token":"d".repeat(64)});
            if native_waiters {
                let mut socket = f.open().await;
                socket.send(hello).await;
                unix_waiters.push(socket);
            } else {
                let mut socket = Socket::open(&f.server).await;
                socket.send(hello).await;
                tcp_waiters.push(socket);
            }
            within(waiting)
                .await
                .expect("hello never reached its contended kernel wait");
        }
        f.app
            .state
            .structured_agents
            .register(crate::structured::registry::Record {
                runtime: crate::structured::registry::StructuredRuntime::Structured,
                agent_id: "fixture-event".into(),
                name: None,
                harness: "claude".into(),
                session_id: "fixture-session".into(),
                cwd: f.server._root.path().to_owned(),
                model: None,
            })
            .unwrap();
        Arc::get_mut(&mut f.server.core).unwrap().service(
            &f.app.state.structured_agents,
            &mut f.app.structured_workers,
            f.server._root.path(),
        );
        let tcp_event = f.drive(tcp.text()).await;
        let native_event = f.drive(native.text()).await;
        // A new cut forces a fresh authorization check while all four hellos wait.
        tcp.send(json!({"t":"sub","sub_id":"contended","stream":"fleet"}))
            .await;
        let tcp_refusal = f.drive(tcp.text()).await;
        native
            .send(json!({"t":"sub","sub_id":"contended","stream":"fleet"}))
            .await;
        let native_refusal = f.drive(native.text()).await;
        drop(lock);
        let probe = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(f.server._root.path().join("client-tokens/.lock"))
            .unwrap();
        let released = probe.try_lock_shared();
        let release_probe = format!("{released:?}");
        if released.is_ok() {
            probe.unlock().unwrap();
        }
        events
            .lock()
            .unwrap()
            .push((format!("released; probe={release_probe}"), origin.elapsed()));
        assert_eq!(tcp_event["ev"]["type"], "fleet.upsert");
        assert_eq!(native_event["ev"]["type"], "fleet.upsert");
        for refusal in [tcp_refusal, native_refusal] {
            assert_eq!(refusal["t"], "subfail");
            assert_eq!(refusal["code"], "session_busy");
        }
        for socket in &mut tcp_waiters {
            let reply = socket.receive().await;
            assert!(
                matches!(reply, Received::Text(ref value) if value["t"] == "welcome"),
                "hello={reply:?}; events={:?}",
                events.lock().unwrap()
            );
        }
        for socket in &mut unix_waiters {
            let reply = socket.receive().await;
            assert!(
                matches!(reply, Received::Text(ref value) if value["t"] == "welcome"),
                "hello={reply:?}; events={:?}",
                events.lock().unwrap()
            );
        }
        f.finish().await;
    }
}

/// Pause a real contended helper before its two-second IPC read starts.
/// The test owns every release sender; unwinding or a `within` failure drops
/// them and releases this gate. The gate adds no deadline around later work.
fn gate_mutation_wait(
    f: &Native,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (ready, waiting) = tokio::sync::oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    f.server.core.shared.store.on_mutation_waiting(move || {
        ready.send(()).expect("mutation gate observer disappeared");
        let _ = held.recv();
    });
    (waiting, release)
}

#[tokio::test]
async fn waiting_retirements_do_not_starve_established_subscriptions() {
    let mut f = Native::new().await;
    let token = browser_file(&f);
    let mut live = Socket::open(&f.server).await;
    live.hello(1, &token.0).await;
    live.send(json!({"t":"sub","sub_id":"live","stream":"fleet"}))
        .await;
    assert_eq!(f.drive(live.text()).await["t"], "gap");
    assert_eq!(f.drive(live.text()).await["t"], "synced");
    let mut sockets = Vec::new();
    let mut tokens = Vec::new();
    for _ in 0..4 {
        let token = f.mint().await;
        let mut socket = f.open().await;
        socket.hello(1, &token.0).await;
        sockets.push(socket);
        tokens.push(token);
    }
    f.server.clock.deadline_count(12 * 60 * 60 * 1000, 4).await;
    let lock = management_lock(&f);
    let mut releases = Vec::new();
    // Hold each worker before its deadline-bounded read while subscriptions run.
    for token in &tokens {
        let (waiting, release) = gate_mutation_wait(&f);
        releases.push(release);
        let session = f.server.core.shared.store.authenticate(token).unwrap();
        f.server.core.shared.revoke(&session);
        within(waiting)
            .await
            .expect("retirement never reached its contended lock wait");
    }
    f.app
        .state
        .structured_agents
        .register(crate::structured::registry::Record {
            runtime: crate::structured::registry::StructuredRuntime::Structured,
            agent_id: "fixture-retire-event".into(),
            name: None,
            harness: "claude".into(),
            session_id: "fixture-retire-session".into(),
            cwd: f.server._root.path().to_owned(),
            model: None,
        })
        .unwrap();
    Arc::get_mut(&mut f.server.core).unwrap().service(
        &f.app.state.structured_agents,
        &mut f.app.structured_workers,
        f.server._root.path(),
    );
    let event = f.drive(live.text()).await;
    assert_eq!(event["ev"]["type"], "fleet.upsert");
    // Force authorization on this established socket while retirements still wait.
    live.send(json!({"t":"sub","sub_id":"contended","stream":"fleet"}))
        .await;
    let refusal = f.drive(live.text()).await;
    assert_eq!(refusal["t"], "subfail");
    assert_eq!(refusal["code"], "session_busy");
    for (socket, token) in sockets.iter().zip(&tokens) {
        assert!(f.server.core.shared.store.authenticate(token).is_ok());
        assert_eq!(
            socket.0.try_read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
    drop(lock);
    for release in releases {
        release.send(()).expect("retirement gate exited early");
    }
    for (socket, token) in sockets.iter_mut().zip(&tokens) {
        assert_eq!(close_detail(socket).await.0, AUTH_REVOKED_CLOSE);
        assert!(f.server.core.shared.store.authenticate(token).is_err());
    }
    f.finish().await;
}

#[tokio::test]
async fn shutdown_waits_outside_the_store_io_pool() {
    assert!(
        std::env::var_os("NEXTEST").is_some(),
        "capacity baseline requires nextest's per-test process isolation"
    );
    let mut f = Native::new().await;
    let token = f.mint().await;
    let mut socket = f.open().await;
    socket.hello(1, &token.0).await;
    // nextest isolates each case in a process, so this pool has no other users.
    let initial_capacity = super::super::super::store_io::shared().available();
    let lock = management_lock(&f);
    let (waiting, release) = gate_mutation_wait(&f);
    {
        let shutdown = f.server.core.shutdown();
        tokio::pin!(shutdown);
        tokio::select! {
            ready = within(waiting) => ready.expect("shutdown lock producer disappeared"),
            result = &mut shutdown => panic!("shutdown returned before its held lock was released: {result:?}"),
        }
        let capacity = super::super::super::store_io::shared().available();
        drop(lock);
        release.send(()).expect("shutdown gate exited early");
        within(&mut shutdown).await.unwrap();
        assert_eq!(
            capacity, initial_capacity,
            "shutdown reserved store I/O while waiting for its lock"
        );
        assert_eq!(close_detail(&mut socket).await.0, AUTH_REVOKED_CLOSE);
    }
    f.finish().await;
}

#[tokio::test]
async fn failed_assertion_releases_the_mutation_gate() {
    use futures_util::FutureExt;
    use std::panic::AssertUnwindSafe;
    let mut f = Native::new().await;
    let token = f.mint().await;
    let mut socket = f.open().await;
    socket.hello(1, &token.0).await;
    let lock = crate::platform::client_token_lock::LockFile::new(management_lock(&f));
    let (waiting, release) = gate_mutation_wait(&f);
    let session = f.server.core.shared.store.authenticate(&token).unwrap();
    f.server.core.shared.revoke(&session);
    let failure = AssertUnwindSafe(async move {
        // The failing scope owns both the lock and the gate's only sender.
        let _resources = (lock, release);
        assert!(
            within(waiting).await.is_err(),
            "injected assertion failure after mutation gate entry"
        );
    })
    .catch_unwind()
    .await
    .expect_err("the injected assertion unexpectedly passed");
    let message = failure
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| failure.downcast_ref::<&str>().copied())
        .unwrap_or("");
    assert!(message.contains("injected assertion failure after mutation gate entry"));
    // The real retirement must finish after sender drop, bounded by within.
    assert_eq!(close_detail(&mut socket).await.0, AUTH_REVOKED_CLOSE);
    assert!(f.server.core.shared.store.authenticate(&token).is_err());
    f.finish().await;
}
