//! Actual WebSocket upgrade/frames against the unmounted core, with a supplied clock.
use super::*;
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn within<T>(work: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), work)
        .await
        .expect("transport producer stayed silent")
}
#[derive(Clone)]
pub(crate) struct TestClock(Arc<ClockState>);
struct ClockState {
    base: Instant,
    elapsed: AtomicU64,
    signal: watch::Sender<u64>,
    deadlines: Mutex<Vec<Instant>>,
}
impl TestClock {
    pub(crate) fn new() -> Self {
        let (signal, _) = watch::channel(0);
        Self(Arc::new(ClockState {
            base: Instant::now(),
            elapsed: AtomicU64::new(0),
            signal,
            deadlines: Mutex::new(Vec::new()),
        }))
    }
    pub(crate) fn advance(&self, ms: u64) {
        self.0.elapsed.fetch_add(ms, Ordering::SeqCst);
        self.0.signal.send_modify(|generation| *generation += 1);
    }
    pub(crate) async fn deadline(&self, ms: u64) {
        self.deadline_count(ms, 1).await;
    }
    fn count(&self, ms: u64) -> usize {
        self.0
            .deadlines
            .lock()
            .unwrap()
            .iter()
            .filter(|&&d| d == self.0.base + Duration::from_millis(ms))
            .count()
    }
    async fn deadline_count(&self, ms: u64, count: usize) {
        let mut signal = self.0.signal.subscribe();
        within(async {
            loop {
                if self
                    .0
                    .deadlines
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|&&d| d == self.0.base + Duration::from_millis(ms))
                    .count()
                    >= count
                {
                    return;
                }
                signal.changed().await.unwrap();
            }
        })
        .await;
    }
}
impl Clock for TestClock {
    fn now(&self) -> Instant {
        self.0.base + Duration::from_millis(self.0.elapsed.load(Ordering::SeqCst))
    }
    fn until(&self, deadline: Instant) -> BoxFuture<'_, ()> {
        let mut signal = self.0.signal.subscribe();
        self.0.deadlines.lock().unwrap().push(deadline);
        self.0.signal.send_modify(|generation| *generation += 1);
        Box::pin(async move {
            loop {
                if self.now() >= deadline {
                    return;
                }
                signal.changed().await.unwrap();
            }
        })
    }
}
struct Server {
    core: Arc<Core>,
    bind: std::net::SocketAddr,
    task: tokio::task::JoinHandle<()>,
    clock: TestClock,
    _root: crate::structured::launch::SessionConfigDir,
}
impl Server {
    async fn new() -> Self {
        Self::with_clock(|clock| Arc::new(clock)).await
    }
    async fn with_clock(wrap: impl FnOnce(TestClock) -> Arc<dyn Clock>) -> Self {
        let root =
            crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let dir = root.path().join("client-tokens");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        for (name, token, principal) in [("one", "a", "c"), ("two", "d", "e")] {
            let path = dir.join(format!("{name}.token"));
            fs::write(&path, json!({"token":token.repeat(64),"principal":principal.repeat(32),"scopes":["read","send","answer","operate"]}).to_string()).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let clock = TestClock::new();
        let core =
            Core::with_clock(TokenStore::open(root.path()).unwrap(), wrap(clock.clone())).unwrap();
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bind = socket.local_addr().unwrap();
        let router = core.router();
        let task = tokio::spawn(async move {
            axum::serve(socket, router).await.unwrap();
        });
        Self {
            core: Arc::new(core),
            bind,
            task,
            clock,
            _root: root,
        }
    }
    async fn finish(mut self) {
        self.core.shutdown().await.unwrap();
        assert!(self.core.shared.clients.lock().unwrap().is_empty());
        assert_eq!(
            self.core.shared.slots.available_permits(),
            CONNECTIONS as usize
        );
        self.task.abort();
        let _ = (&mut self.task).await;
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.core.shared.stop.send_replace(true);
        self.task.abort();
    }
}
struct Socket(tokio::net::TcpStream);
#[derive(Debug)]
enum Received {
    Text(Value),
    Close(u16),
    End,
}
impl Socket {
    async fn open(server: &Server) -> Self {
        let (socket, headers) = Self::upgrade(server).await;
        assert!(headers.starts_with("HTTP/1.1 101"), "{headers}");
        assert!(!headers
            .to_ascii_lowercase()
            .contains("sec-websocket-extensions:"));
        socket
    }
    async fn upgrade(server: &Server) -> (Self, String) {
        within(async {
            let mut stream = tokio::net::TcpStream::connect(server.bind).await.unwrap();
            stream.write_all(format!("GET /v1/ws HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Extensions: permessage-deflate; client_max_window_bits\r\n\r\n", server.bind).as_bytes()).await.unwrap(); // gitleaks:allow -- RFC 6455 public handshake example nonce
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                bytes.push(stream.read_u8().await.unwrap());
                assert!(bytes.len() < 8192);
            }
            (Self(stream), String::from_utf8(bytes).unwrap())
        })
        .await
    }
    async fn send(&mut self, value: Value) {
        self.raw(value.to_string().as_bytes(), 0x81).await;
    }
    async fn raw(&mut self, data: &[u8], flags: u8) {
        within(async {
            let mut frame = vec![flags];
            if data.len() < 126 {
                frame.push(0x80 | data.len() as u8);
            } else if data.len() <= u16::MAX as usize {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(data.len() as u16).to_be_bytes());
            } else {
                frame.push(0x80 | 127);
                frame.extend_from_slice(&(data.len() as u64).to_be_bytes());
            }
            let mask = [1, 2, 3, 4];
            frame.extend(mask);
            frame.extend(data.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
            self.0.write_all(&frame).await.unwrap();
        })
        .await;
    }
    async fn receive(&mut self) -> Received {
        within(async {
            loop {
                let flags = match self.0.read_u8().await {
                    Ok(v) => v,
                    Err(_) => return Received::End,
                };
                assert_eq!(flags & 0x70, 0, "server negotiated or emitted compression");
                let length = self.0.read_u8().await.unwrap();
                assert_eq!(length & 0x80, 0);
                let length = match length & 127 {
                    126 => u64::from(self.0.read_u16().await.unwrap()),
                    127 => self.0.read_u64().await.unwrap(),
                    n => u64::from(n),
                };
                assert!(length <= MAX_MESSAGE as u64);
                let mut bytes = vec![0; length as usize];
                self.0.read_exact(&mut bytes).await.unwrap();
                match flags & 15 {
                    1 => return Received::Text(serde_json::from_slice(&bytes).unwrap()),
                    8 => {
                        return Received::Close(u16::from_be_bytes(bytes[..2].try_into().unwrap()))
                    }
                    9 | 10 => (),
                    _ => panic!("unexpected websocket opcode"),
                }
            }
        })
        .await
    }
    async fn text(&mut self) -> Value {
        match self.receive().await {
            Received::Text(v) => v,
            other => panic!("expected frame, got {other:?}"),
        }
    }
    async fn hello(&mut self, protocol: u32, token: &str) -> Value {
        self.send(json!({"t":"hello","protocol":protocol,"client":{"name":"fixture","version":"1"},"token":token})).await;
        self.text().await
    }
}
fn runtime(work: impl Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(work);
}
#[test]
fn hello_uses_generated_contracts_epoch_identity_and_read_only_metadata() {
    runtime(async {
        let server = Server::new().await;
        let mut first = Socket::open(&server).await;
        let welcome = first.hello(WIRE_JSON_VERSION, &"a".repeat(64)).await;
        assert_eq!(welcome["protocol"], WIRE_JSON_VERSION);
        assert_eq!(welcome["principal"], "c".repeat(32));
        assert_eq!(welcome["capabilities"], json!([]));
        assert_eq!(welcome["idempotency_window_ms"], 86_400_000);
        assert!(!welcome["server_version"].as_str().unwrap().is_empty());
        assert_eq!(welcome["server_epoch"].as_str().unwrap().len(), 32);
        assert!(matches!(
            serde_json::from_value::<ServerFrame>(welcome.clone()).unwrap(),
            ServerFrame::Welcome { .. }
        ));
        let mut newer = Socket::open(&server).await;
        let newer_welcome = newer.hello(WIRE_JSON_VERSION + 1, &"d".repeat(64)).await;
        assert_eq!(newer_welcome["capabilities"], json!([]));
        assert_eq!(newer_welcome["server_epoch"], welcome["server_epoch"]);
        server.clock.deadline(15_000).await;
        {
            let clients = server.core.shared.clients.lock().unwrap();
            assert_eq!(clients.len(), 2);
            assert_eq!(clients.values().filter(|c| c.read_only).count(), 1);
        }
        server.finish().await;
    });
}
#[test]
fn malformed_missing_bad_and_unavailable_tokens_refuse_uniformly_without_secrets() {
    runtime(async {
        let server = Server::new().await;
        let frames = [
            json!({"t":"hello","protocol":1,"client":{"name":"f","version":"1"}}),
            json!({"t":"hello","protocol":1,"client":{"name":"f","version":"1"},"token":"bad-secret"}),
            json!({"t":"pong","nonce":"no-hello"}),
            json!({"t":"newer-frame"}),
        ];
        for frame in frames {
            let mut client = Socket::open(&server).await;
            client.send(frame).await;
            assert!(matches!(
                client.receive().await,
                Received::Close(AUTH_REFUSED_CLOSE)
            ));
        }
        let mut invalid = Socket::open(&server).await;
        invalid.raw(b"not-json", 0x81).await;
        assert!(matches!(
            invalid.receive().await,
            Received::Close(AUTH_REFUSED_CLOSE)
        ));
        fs::remove_file(server._root.path().join("client-tokens/one.token")).unwrap();
        let mut revoked = Socket::open(&server).await;
        revoked.send(json!({"t":"hello","protocol":1,"client":{"name":"f","version":"1"},"token":"a".repeat(64)})).await;
        assert!(matches!(
            revoked.receive().await,
            Received::Close(AUTH_REFUSED_CLOSE)
        ));
        server.finish().await;
    });
}
#[test]
fn hello_deadline_is_absolute_and_releases_an_unauthenticated_slot() {
    runtime(async {
        let server = Server::new().await;
        let mut client = Socket::open(&server).await;
        server.clock.deadline(HANDSHAKE_DEADLINE_MS).await;
        server.clock.advance(HANDSHAKE_DEADLINE_MS);
        assert!(matches!(
            client.receive().await,
            Received::Close(GOING_AWAY_CLOSE)
        ));
        let permit = within(
            server
                .core
                .shared
                .slots
                .clone()
                .acquire_many_owned(CONNECTIONS),
        )
        .await
        .unwrap();
        drop(permit);
        server.finish().await;
    });
}
#[test]
fn heartbeat_matches_only_current_non_replayed_nonces_and_expiry_is_exact() {
    runtime(async {
        let server = Server::new().await;
        let mut client = Socket::open(&server).await;
        client.hello(1, &"a".repeat(64)).await;
        server.clock.deadline(15_000).await;
        server.clock.advance(15_000);
        let ping = client.text().await;
        assert_eq!(ping["t"], "ping");
        client.send(json!({"t":"pong","nonce":ping["nonce"]})).await;
        server.clock.deadline_count(60_000, 2).await;
        server.clock.advance(15_000);
        let second = client.text().await;
        assert_ne!(ping["nonce"], second["nonce"]);
        client.send(json!({"t":"pong","nonce":ping["nonce"]})).await;
        client.send(json!({"t":"pong","nonce":"never-sent"})).await;
        server.clock.advance(15_000);
        let third = client.text().await;
        assert_eq!(third["t"], "ping");
        server.clock.advance(15_000);
        assert!(matches!(
            client.receive().await,
            Received::Close(GOING_AWAY_CLOSE)
        ));
        server.finish().await;
    });
}
#[test]
fn old_socket_nonce_and_control_pongs_cannot_renew_json_liveness() {
    runtime(async {
        let server = Server::new().await;
        let mut first = Socket::open(&server).await;
        first.hello(1, &"a".repeat(64)).await;
        let mut second = Socket::open(&server).await;
        second.hello(1, &"a".repeat(64)).await;
        server.clock.deadline(15_000).await;
        server.clock.advance(15_000);
        let old = first.text().await;
        let current = second.text().await;
        assert_ne!(old["nonce"], current["nonce"]);
        let processed = server.clock.count(45_000);
        second.send(json!({"t":"pong","nonce":old["nonce"]})).await;
        second.raw(b"control", 0x8A).await;
        server.clock.deadline_count(45_000, processed + 2).await;
        server.clock.advance(30_000);
        assert!(matches!(
            second.receive().await,
            Received::Close(GOING_AWAY_CLOSE)
        ));
        server.finish().await;
    });
}
#[test]
fn fanout_closes_only_the_verified_principal_and_cleanup_is_owned() {
    runtime(async {
        let server = Server::new().await;
        let mut one = Socket::open(&server).await;
        one.hello(1, &"a".repeat(64)).await;
        let mut duplicate = Socket::open(&server).await;
        duplicate.hello(1, &"a".repeat(64)).await;
        let mut other = Socket::open(&server).await;
        other.hello(1, &"d".repeat(64)).await;
        server.clock.deadline(15_000).await;
        let session = server
            .core
            .shared
            .clients
            .lock()
            .unwrap()
            .values()
            .find(|c| c.session.info.principal == "c".repeat(32))
            .unwrap()
            .session
            .clone();
        server.core.revoke(&session);
        assert!(matches!(
            one.receive().await,
            Received::Close(AUTH_REVOKED_CLOSE)
        ));
        assert!(matches!(
            duplicate.receive().await,
            Received::Close(AUTH_REVOKED_CLOSE)
        ));
        server.clock.advance(15_000);
        assert_eq!(other.text().await["t"], "ping");
        server.finish().await;
    });
}
#[test]
fn sixteen_pending_hellos_bound_slots_and_shutdown_refuses_new_upgrades() {
    runtime(async {
        let server = Server::new().await;
        let mut sockets = Vec::new();
        for _ in 0..16 {
            sockets.push(Socket::open(&server).await);
        }
        let (_, refusal) = Socket::upgrade(&server).await;
        assert!(refusal.starts_with("HTTP/1.1 503"));
        server.core.shutdown().await.unwrap();
        let (_, refusal) = Socket::upgrade(&server).await;
        assert!(refusal.starts_with("HTTP/1.1 503"));
        assert_eq!(
            server.core.shared.slots.available_permits(),
            CONNECTIONS as usize
        );
        server.finish().await;
    });
}

struct GateSink {
    gate: tokio::sync::oneshot::Receiver<()>,
    open: bool,
    failed: bool,
    pending: Option<Message>,
    started: Option<tokio::sync::oneshot::Sender<()>>,
    flushed: mpsc::Sender<Message>,
    dropped: Arc<std::sync::atomic::AtomicBool>,
}
impl Drop for GateSink {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}
impl futures_util::Sink<Message> for GateSink {
    type Error = io::Error;
    fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn start_send(mut self: Pin<&mut Self>, message: Message) -> io::Result<()> {
        self.pending = Some(message);
        if let Some(started) = self.started.take() {
            let _ = started.send(());
        }
        Ok(())
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.failed {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        if !self.open {
            match Pin::new(&mut self.gate).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(_)) => {
                    self.failed = true;
                    return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
                }
                Poll::Ready(Ok(())) => self.open = true,
            }
        }
        if let Some(message) = self.pending.take() {
            self.flushed.try_send(message).unwrap();
        }
        Poll::Ready(Ok(()))
    }
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}
fn data_frame(id: &str) -> ServerFrame {
    ServerFrame::Synced {
        sub_id: id.into(),
        seq: 0,
    }
}
type SinkFixture = (
    GateSink,
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
    mpsc::Receiver<Message>,
    Arc<std::sync::atomic::AtomicBool>,
);
fn gated_sink() -> SinkFixture {
    let (release, gate) = tokio::sync::oneshot::channel();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (flushed, receive) = mpsc::channel(16);
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    (
        GateSink {
            gate,
            open: false,
            failed: false,
            pending: None,
            started: Some(started),
            flushed,
            dropped: dropped.clone(),
        },
        release,
        ready,
        receive,
        dropped,
    )
}
#[test]
fn writer_prioritizes_ping_between_bounded_data_writes() {
    runtime(async {
        let clock = TestClock::new();
        let (sink, release, ready, mut received, _) = gated_sink();
        let (priority, high) = mpsc::channel(4);
        let (data, low) = mpsc::channel(8);
        data.try_send(data_frame("first")).unwrap();
        let task = tokio::spawn(output::writer(sink, Arc::new(clock), high, low));
        within(ready).await.unwrap();
        for _ in 0..8 {
            data.try_send(data_frame("backlog")).unwrap();
        }
        assert!(matches!(
            data.try_send(data_frame("overflow")),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        priority
            .try_send(
                output::encode(ServerFrame::Ping {
                    nonce: "priority".into(),
                })
                .unwrap(),
            )
            .unwrap();
        release.send(()).unwrap();
        let first = within(received.recv()).await.unwrap();
        let ping = within(received.recv()).await.unwrap();
        let text = |m| match m {
            Message::Text(t) => serde_json::from_str::<Value>(&t).unwrap(),
            _ => panic!("text required"),
        };
        assert_eq!(text(first)["sub_id"], "first");
        assert_eq!(text(ping)["nonce"], "priority");
        for _ in 0..8 {
            assert_eq!(
                text(within(received.recv()).await.unwrap())["sub_id"],
                "backlog"
            );
        }
        drop(priority);
        within(task).await.unwrap();
    });
}
#[test]
fn live_silent_writer_expires_at_its_original_ceiling_and_is_dropped() {
    runtime(async {
        let clock = TestClock::new();
        let (sink, _alive, ready, _received, dropped) = gated_sink();
        let (_priority, high) = mpsc::channel(4);
        let (data, low) = mpsc::channel(8);
        data.try_send(data_frame("stuck")).unwrap();
        let task = tokio::spawn(output::writer(sink, Arc::new(clock.clone()), high, low));
        within(ready).await.unwrap();
        clock.deadline(LIVENESS_WINDOW_MS).await;
        clock.advance(LIVENESS_WINDOW_MS);
        within(task).await.unwrap();
        assert!(dropped.load(Ordering::SeqCst));
    });
}
#[test]
fn dead_writer_fails_without_waiting_for_liveness_expiry() {
    runtime(async {
        let clock = TestClock::new();
        let (sink, dead, ready, _received, dropped) = gated_sink();
        let (_priority, high) = mpsc::channel(4);
        let (data, low) = mpsc::channel(8);
        data.try_send(data_frame("dead")).unwrap();
        let task = tokio::spawn(output::writer(sink, Arc::new(clock.clone()), high, low));
        within(ready).await.unwrap();
        drop(dead);
        within(task).await.unwrap();
        assert_eq!(clock.0.elapsed.load(Ordering::SeqCst), 0);
        assert!(dropped.load(Ordering::SeqCst));
    });
}
#[test]
fn serialized_frames_are_capped_at_the_boundary_during_encoding() {
    let overhead = serde_json::to_vec(&ServerFrame::Ping {
        nonce: String::new(),
    })
    .unwrap()
    .len();
    let exact = output::encode(ServerFrame::Ping {
        nonce: "x".repeat(MAX_MESSAGE - overhead),
    })
    .unwrap();
    assert!(matches!(exact, Message::Text(t) if t.len()==MAX_MESSAGE));
    assert!(output::encode(ServerFrame::Ping {
        nonce: "x".repeat(MAX_MESSAGE - overhead + 1)
    })
    .is_none());
}
#[test]
fn frame_limit_fragmented_message_limit_and_rsv1_are_enforced_on_real_sockets() {
    runtime(async {
        let server = Server::new().await;
        assert_eq!(MAX_MESSAGE, 65_536);
        let mut hello = json!({"t":"hello","protocol":1,"client":{"name":"f","version":"1"},"token":"a".repeat(64),"padding":""});
        let overhead = hello.to_string().len();
        hello["padding"] = Value::String("x".repeat(65_537 - overhead));
        let bytes = hello.to_string().into_bytes();
        assert_eq!(bytes.len(), 65_537);
        for fragmented in [false, true] {
            let mut client = Socket::open(&server).await;
            if fragmented {
                client.raw(&bytes[..32_768], 0x01).await;
                client.raw(&bytes[32_768..], 0x80).await;
            } else {
                client.raw(&bytes, 0x81).await;
            }
            assert!(matches!(
                client.receive().await,
                Received::Close(AUTH_REFUSED_CLOSE) | Received::End
            ));
        }
        hello["padding"] = Value::String("x".repeat(65_536 - overhead));
        let mut exact = Socket::open(&server).await;
        exact.send(hello).await;
        assert_eq!(exact.text().await["t"], "welcome");
        let mut compressed = Socket::open(&server).await;
        compressed.raw(json!({"t":"hello","protocol":1,"client":{"name":"f","version":"1"},"token":"a".repeat(64)}).to_string().as_bytes(), 0xC1).await;
        assert!(matches!(
            compressed.receive().await,
            Received::Close(AUTH_REFUSED_CLOSE) | Received::End
        ));
        server.finish().await;
    });
}
#[test]
fn covers_selected_fixture_handshake_vectors_run_against_the_core() {
    runtime(async {
        use crate::structured_wire::fixture::{FixtureCoverage, FixtureLine, FixtureStep};
        let directory =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/client/test/fixtures/wire");
        let mut selected = 0;
        let mut refused = 0;
        let mut heartbeats = 0;
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            let script = FixtureLine::parse_script(&fs::read_to_string(&path).unwrap()).unwrap();
            if !matches!(&script[0].step, FixtureStep::Given { covers, .. } if covers.contains(&FixtureCoverage::S7Min))
            {
                continue;
            }
            let Some(mut hello) = script.iter().find_map(|line| match &line.step {
                FixtureStep::Expect {
                    frame: frame @ ClientFrame::Hello { .. },
                    ..
                } => Some(frame.clone()),
                _ => None,
            }) else {
                continue;
            };
            selected += 1;
            let auth_refusal = script.iter().any(|line| {
                matches!(
                    line.step,
                    FixtureStep::TokenRefused {
                        code: AUTH_REFUSED_CLOSE
                    }
                )
            });
            // Fixture client credentials are fictional; map the credential to this store's seed.
            if !auth_refusal {
                if let ClientFrame::Hello { token, .. } = &mut hello {
                    *token = Some("a".repeat(64));
                }
            }
            let server = Server::new().await;
            let mut client = Socket::open(&server).await;
            client.send(serde_json::to_value(hello).unwrap()).await;
            if auth_refusal {
                assert!(matches!(
                    client.receive().await,
                    Received::Close(AUTH_REFUSED_CLOSE)
                ));
                refused += 1;
            } else {
                let welcome = client.text().await;
                assert_eq!(welcome["t"], "welcome");
                assert_eq!(welcome["protocol"], WIRE_JSON_VERSION);
                assert_eq!(
                    welcome["capabilities"],
                    json!([]),
                    "later slices advertise stream/command capabilities"
                );
                if script.iter().any(|line| {
                    matches!(
                        line.step,
                        FixtureStep::Send {
                            frame: ServerFrame::Ping { .. },
                            ..
                        }
                    )
                }) {
                    server.clock.deadline(HEARTBEAT_INTERVAL_MS).await;
                    server.clock.advance(HEARTBEAT_INTERVAL_MS);
                    let ping = client.text().await;
                    client.send(json!({"t":"pong","nonce":ping["nonce"]})).await;
                    heartbeats += 1;
                }
            }
            // Only transport vectors run here; subscriptions/client reducers remain S8.
            server.finish().await;
        }
        assert!(selected >= 9);
        assert_eq!(refused, 1);
        assert!(heartbeats >= 2);
    });
}

#[test]
fn authentication_work_cannot_extend_the_original_hello_deadline() {
    tokio::runtime::Builder::new_current_thread().enable_all().max_blocking_threads(1).build().unwrap().block_on(async {
        let (started, ready) = tokio::sync::oneshot::channel(); let (release, wait) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || { started.send(()).unwrap(); wait.recv_timeout(Duration::from_secs(5)).unwrap(); });
        within(ready).await.unwrap();
        let server = Server::new().await; let mut client = Socket::open(&server).await;
        client.send(json!({"t":"hello","protocol":1,"client":{"name":"fixture","version":"1"},"token":"a".repeat(64)})).await;
        server.clock.deadline_count(5_000, 2).await; server.clock.advance(5_000);
        let result = client.receive().await;
        release.send(()).unwrap(); within(blocker).await.unwrap();
        assert!(matches!(result, Received::Close(GOING_AWAY_CLOSE)));
        server.finish().await;
    });
}
#[test]
fn future_data_queue_is_bounded_on_the_actual_verified_socket() {
    runtime(async {
        let server = Server::new().await;
        let mut client = Socket::open(&server).await;
        client.hello(1, &"a".repeat(64)).await;
        server.clock.deadline(15_000).await;
        let sender = server
            .core
            .shared
            .clients
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .data
            .clone();
        assert_eq!(sender.max_capacity(), 8);
        assert!(matches!(
            sender.try_reserve_many(9),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        server.finish().await;
    });
}
#[test]
fn unknown_post_hello_operations_and_binary_frames_execute_nothing() {
    runtime(async {
        let server = Server::new().await;
        for frame in [
            json!({"t":"cmd","cmd_id":"fixture","verb":"agent.send","args":{"agent":"fiction","text":"no-dispatch","submit":true},"resume_only":false}),
            json!({"t":"new-operation"}),
            json!({"t":"hello","protocol":1,"client":{"name":"f","version":"1"},"token":"a".repeat(64)}),
        ] {
            let mut client = Socket::open(&server).await;
            client.hello(1, &"a".repeat(64)).await;
            client.send(frame).await;
            assert!(matches!(client.receive().await, Received::Close(1008)));
        }
        let mut binary = Socket::open(&server).await;
        binary.hello(1, &"a".repeat(64)).await;
        binary.raw(b"binary", 0x82).await;
        assert!(matches!(
            binary.receive().await,
            Received::Close(POLICY_VIOLATION_CLOSE)
        ));
        server.finish().await;
    });
}

#[test]
fn held_management_lock_does_not_block_valid_or_junk_hello_verification() {
    runtime(async {
        let server = Server::new().await;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(server._root.path().join("client-tokens/.lock"))
            .unwrap();
        lock.lock().unwrap();
        let mut valid = Socket::open(&server).await;
        assert_eq!(valid.hello(1, &"a".repeat(64)).await["t"], "welcome");
        let mut junk = Socket::open(&server).await;
        junk.send(
            json!({"t":"hello","protocol":1,"client":{"name":"f","version":"1"},"token":"invalid"}),
        )
        .await;
        assert!(matches!(
            junk.receive().await,
            Received::Close(AUTH_REFUSED_CLOSE)
        ));
        drop(lock);
        server.finish().await;
    });
}
#[test]
fn normal_constructor_opens_the_store_once_and_reports_startup_failure() {
    runtime(async {
        let root =
            crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let core = Core::new(root.path().into()).await.unwrap();
        assert_eq!(core.shared.slots.available_permits(), 16);
        core.shutdown().await.unwrap();
        let file = root.path().join("not-directory");
        fs::write(&file, b"fixture").unwrap();
        assert!(Core::new(file).await.is_err());
    });
}

#[test]
fn exhausted_socket_identity_refuses_instead_of_reusing_a_nonce_namespace() {
    runtime(async {
        let server = Server::new().await;
        server.core.shared.next.store(u64::MAX, Ordering::Relaxed);
        let mut client = Socket::open(&server).await;
        assert!(matches!(client.receive().await, Received::End));
        server.finish().await;
    });
}

#[test]
fn abandoned_writer_releases_the_sink_without_waiting_for_the_peer() {
    runtime(async {
        let clock = TestClock::new();
        let (sink, _alive, ready, _received, dropped) = gated_sink();
        let (_priority, high) = mpsc::channel(4);
        let (data, low) = mpsc::channel(8);
        data.try_send(data_frame("abandoned")).unwrap();
        let task = tokio::spawn(output::writer(sink, Arc::new(clock.clone()), high, low));
        within(ready).await.unwrap();
        task.abort();
        assert!(within(task).await.unwrap_err().is_cancelled());
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(clock.0.elapsed.load(Ordering::SeqCst), 0);
    });
}

/// A pending deadline is dropped when its protected producer completes.
struct OnVerified(Option<Box<dyn FnOnce() + Send>>);
impl Drop for OnVerified {
    fn drop(&mut self) {
        if let Some(action) = self.0.take() {
            action();
        }
    }
}
type Revocation = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;
struct RevokeClock {
    clock: TestClock,
    calls: AtomicU64,
    action: Revocation,
    at: u64,
}
impl Clock for RevokeClock {
    fn now(&self) -> Instant {
        self.clock.now()
    }
    fn until(&self, end: Instant) -> BoxFuture<'_, ()> {
        let guard = OnVerified(if self.calls.fetch_add(1, Ordering::SeqCst) == self.at {
            self.action.lock().unwrap().take()
        } else {
            None
        });
        let deadline = self.clock.until(end);
        Box::pin(async move {
            let _guard = guard;
            deadline.await
        })
    }
}
#[test]
fn revocation_between_verification_and_registration_never_welcomes() {
    runtime(revoke_during_hello(1, 0));
}
#[test]
fn revocation_after_storage_recheck_never_welcomes() {
    runtime(revoke_during_hello(3, 1));
}
async fn revoke_during_hello(at: u64, registered: usize) {
    let action = Arc::new(Mutex::new(None));
    let server = Server::with_clock(|clock| {
        Arc::new(RevokeClock {
            clock,
            calls: AtomicU64::new(0),
            at,
            action: action.clone(),
        })
    })
    .await;
    let core = Arc::downgrade(&server.core);
    let session = server
        .core
        .shared
        .store
        .authenticate(&Bearer("a".repeat(64)))
        .unwrap();
    let path = server._root.path().join("client-tokens/one.token");
    let fired = Arc::new(AtomicU64::new(0));
    let observed = fired.clone();
    *action.lock().unwrap() = Some(Box::new(move || {
        let core = core.upgrade().expect("transport owner must remain live");
        assert_eq!(
            core.shared.clients.lock().unwrap().len(),
            registered,
            "revocation must fire at the requested handshake stage"
        );
        fs::remove_file(path).unwrap();
        core.revoke(&session);
        observed.fetch_add(1, Ordering::SeqCst);
    }));
    let mut client = Socket::open(&server).await;
    client.send(json!({"t":"hello","protocol":1,"client":{"name":"fixture","version":"1"},"token":"a".repeat(64)})).await;
    assert!(matches!(
        client.receive().await,
        Received::Close(AUTH_REVOKED_CLOSE)
    ));
    assert_eq!(fired.load(Ordering::SeqCst), 1);
    server.finish().await;
}

#[test]
fn registered_recheck_deadline_preserves_the_token_with_a_non_auth_close() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build().unwrap().block_on(async {
            let action = Arc::new(Mutex::new(None));
            let server = Server::with_clock(|clock| Arc::new(RevokeClock {
                clock, calls: AtomicU64::new(0), at: 1, action: action.clone(),
            })).await;
            let (started, ready) = tokio::sync::oneshot::channel();
            let (release, wait) = std::sync::mpsc::channel();
            let worker = Arc::new(Mutex::new(None));
            let owned = worker.clone();
            *action.lock().unwrap() = Some(Box::new(move || {
                *owned.lock().unwrap() = Some(tokio::task::spawn_blocking(move || {
                    started.send(()).unwrap();
                    wait.recv_timeout(Duration::from_secs(5)).unwrap();
                }));
            }));
            let mut client = Socket::open(&server).await;
            client.send(json!({"t":"hello","protocol":1,"client":{"name":"fixture","version":"1"},"token":"a".repeat(64)})).await;
            within(ready).await.unwrap();
            server.clock.deadline_count(HANDSHAKE_DEADLINE_MS, 4).await;
            assert_eq!(server.core.shared.clients.lock().unwrap().len(), 1);
            server.clock.advance(HANDSHAKE_DEADLINE_MS);
            let result = client.receive().await;
            release.send(()).unwrap();
            let worker = worker.lock().unwrap().take().unwrap();
            within(worker).await.unwrap();
            assert!(matches!(result, Received::Close(GOING_AWAY_CLOSE)), "{result:?}");
            assert!(server.core.shared.store.authenticate(&Bearer("a".repeat(64))).is_ok());
            server.finish().await;
        });
}

#[test]
fn failed_welcome_send_does_not_attempt_a_revocation_close() {
    runtime(async {
        let clock = TestClock::new();
        let (mut sink, dead, _ready, mut received, _) = gated_sink();
        drop(dead);
        let (_stop, mut stopped) = watch::channel(false);
        let (_revoke, mut revoked) = watch::channel(false);
        let welcome = ServerFrame::Welcome {
            protocol: WIRE_JSON_VERSION,
            server_version: "fixture".into(),
            capabilities: vec![],
            principal: "fixture-principal".into(),
            server_epoch: "fixture-epoch".into(),
            idempotency_window_ms: IDEMPOTENCY_WINDOW_MS,
        };
        assert!(
            !within(finish_welcome(
                &mut sink,
                &clock,
                clock.now() + Duration::from_millis(HANDSHAKE_DEADLINE_MS),
                async { Ok(()) },
                welcome,
                (&mut stopped, &mut revoked)
            ))
            .await
        );
        assert!(
            matches!(sink.pending, Some(Message::Text(_))),
            "failed send must drop the transport, without another send attempt"
        );
        assert!(received.try_recv().is_err());
    });
}

#[test]
fn hello_store_io_and_unsafe_failures_preserve_credentials() {
    runtime(async {
        for unsafe_mode in [true, false] {
            let server = Server::new().await;
            let dir = server._root.path().join("client-tokens");
            let saved = server._root.path().join("saved-tokens");
            if unsafe_mode {
                fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
            } else {
                fs::rename(&dir, &saved).unwrap();
            }
            let error = server
                .core
                .shared
                .store
                .authenticate(&Bearer("a".repeat(64)))
                .err()
                .unwrap();
            assert!(matches!(
                (&error, unsafe_mode),
                (TokenError::Unsafe, true) | (TokenError::Io(_), false)
            ));
            let mut client = Socket::open(&server).await;
            client.send(json!({"t":"hello","protocol":1,"client":{"name":"fixture","version":"1"},"token":"a".repeat(64)})).await;
            let result = client.receive().await;
            if unsafe_mode {
                fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
            } else {
                fs::rename(&saved, &dir).unwrap();
            }
            assert!(
                matches!(result, Received::Close(GOING_AWAY_CLOSE)),
                "{error:?}: {result:?}"
            );
            assert!(server
                .core
                .shared
                .store
                .authenticate(&Bearer("a".repeat(64)))
                .is_ok());
            server.finish().await;
        }
    });
}

#[test]
fn hello_unavailable_store_worker_is_not_an_auth_refusal() {
    runtime(async {
        let server = Server::new().await;
        let mut tasks = Vec::new();
        let mut releases = Vec::new();
        for _ in 0..4 {
            let (ready, started) = tokio::sync::oneshot::channel();
            let (release, wait) = std::sync::mpsc::channel();
            releases.push(release);
            tasks.push(tokio::spawn(async move {
                super::super::store_io::shared()
                    .run(move || {
                        ready.send(()).unwrap();
                        wait.recv_timeout(Duration::from_secs(5)).unwrap();
                    })
                    .await
            }));
            within(started).await.unwrap();
        }
        assert!(super::super::store_io::shared().run(|| ()).await.is_none());
        let mut client = Socket::open(&server).await;
        client.send(json!({"t":"hello","protocol":1,"client":{"name":"fixture","version":"1"},"token":"a".repeat(64)})).await;
        let result = client.receive().await;
        for release in releases {
            release.send(()).unwrap();
        }
        for task in tasks {
            assert!(within(task).await.unwrap().is_some());
        }
        assert!(
            matches!(result, Received::Close(GOING_AWAY_CLOSE)),
            "{result:?}"
        );
        assert!(server
            .core
            .shared
            .store
            .authenticate(&Bearer("a".repeat(64)))
            .is_ok());
        server.finish().await;
    });
}

#[test]
fn failed_welcome_encode_drops_without_any_close_send() {
    runtime(async {
        let clock = TestClock::new();
        let (mut sink, _release, mut started, mut received, _) = gated_sink();
        let (_stop, mut stopped) = watch::channel(false);
        let (_revoke, mut revoked) = watch::channel(false);
        let welcome = ServerFrame::Welcome {
            protocol: WIRE_JSON_VERSION,
            server_version: "x".repeat(MAX_MESSAGE),
            capabilities: vec![],
            principal: "fixture-principal".into(),
            server_epoch: "fixture-epoch".into(),
            idempotency_window_ms: IDEMPOTENCY_WINDOW_MS,
        };
        assert!(
            !within(finish_welcome(
                &mut sink,
                &clock,
                clock.now() + Duration::from_millis(HANDSHAKE_DEADLINE_MS),
                async { Ok(()) },
                welcome,
                (&mut stopped, &mut revoked),
            ))
            .await
        );
        assert!(
            sink.pending.is_none(),
            "encoding failure must never send a close"
        );
        assert!(started.try_recv().is_err());
        assert!(received.try_recv().is_err());
    });
}
