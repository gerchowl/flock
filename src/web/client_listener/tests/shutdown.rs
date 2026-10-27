use super::*;
use crate::web::wire::tests::{app_sessions, Received, Socket};
#[tokio::test]
async fn aggregate_shutdown_ends_native_and_tcp_upgrades_and_retires_app_bearers() {
    let (server, mut app, owner) = app_sessions::fixture_with_origin(false).await;
    let token = app_sessions::mint(&mut app).await;
    let root = crate::test_support::TempDir::socket();
    let path = root.path().join("native.sock");
    let core = Core::new_shared(
        server._root.path().into(),
        server.wake.clone(),
        owner.clone(),
    )
    .await
    .unwrap();
    let slots = crate::web::wire::tests::lease_counter(&core);
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = tcp.local_addr().unwrap();
    let router = core.router();
    let tcp_task = tokio::spawn(async move {
        axum::serve(tcp, router).await.unwrap();
    });
    let mut listener = Listener::prepare(
        None,
        Codes::default(),
        server._root.path().into(),
        None,
        (path.clone(), None),
        Health::default(),
        core,
    )
    .await
    .unwrap()
    .unwrap();
    listener.activate();
    let work = async {
        let mut stream = tokio::net::UnixStream::connect(path).await.unwrap();
        stream.write_all(b"GET /v1/ws HTTP/1.1\r\nHost: fixture.example\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n").await.unwrap(); // gitleaks:allow -- RFC 6455 public example nonce
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            header.push(stream.read_u8().await.unwrap());
        }
        assert!(String::from_utf8(header)
            .unwrap()
            .starts_with("HTTP/1.1 101"));
        let mut local = Socket(stream);
        local
            .hello(crate::structured_wire::WIRE_JSON_VERSION, &token.0)
            .await;
        let mut tcp = tokio::net::TcpStream::connect(address).await.unwrap();
        // Keep a second upgraded socket waiting for hello across the other transport.
        tcp.write_all(b"GET /v1/ws HTTP/1.1\r\nHost: fixture.example\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n").await.unwrap(); // gitleaks:allow -- RFC 6455 public example nonce
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            header.push(tcp.read_u8().await.unwrap());
        }
        assert!(String::from_utf8(header)
            .unwrap()
            .starts_with("HTTP/1.1 101"));
        (local, tcp)
    };
    tokio::pin!(work);
    let (mut local, mut tcp) = bounded(async {
        loop {
            tokio::select! {
                sockets = &mut work => break sockets,
                _ = server.wake.notified() => listener.service_commands(&mut app),
            }
        }
    })
    .await;
    assert_eq!(slots.available_permits(), 14);
    bounded(listener.shutdown()).await.unwrap();
    assert_eq!(slots.available_permits(), 16);
    assert!(matches!(local.receive().await, Received::Close(4403)));
    let mut tail = Vec::new();
    assert_eq!(bounded(tcp.read_to_end(&mut tail)).await.unwrap(), 0);
    let store = owner
        .get(
            server._root.path(),
            Arc::new(crate::structured::permissions::auth::tokens::app_sessions::SystemClock),
        )
        .unwrap();
    assert!(store.authenticate(&token).is_err());
    tcp_task.abort();
    let _ = tcp_task.await;
    server.finish().await;
}
