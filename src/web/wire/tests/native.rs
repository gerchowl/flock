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
        let (server, app, _) = super::app_sessions::fixture().await;
        let root = crate::test_support::TempDir::socket();
        let mut listener = local_listener::Listener::prepare(
            root.path().join("native.sock"),
            None,
            server.core.local_router(),
        )
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
    async fn open(&mut self) -> Socket<UnixStream> {
        let path = self.path();
        self.drive(async move {
            let mut socket = UnixStream::connect(path).await.unwrap();
            socket.write_all(b"GET /v1/ws HTTP/1.1\r\nHost: fixture.example\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n").await.unwrap(); // gitleaks:allow -- RFC 6455 public handshake example nonce
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                header.push(socket.read_u8().await.unwrap());
                assert!(header.len() < 8192);
            }
            let header = String::from_utf8(header).unwrap();
            assert!(header.starts_with("HTTP/1.1 101"), "{header}");
            Socket(socket)
        })
        .await
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
    assert_eq!(ack["error"]["code"], "agent_not_found");
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
