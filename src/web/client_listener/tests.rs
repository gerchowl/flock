use super::*;
use crate::web::wire::tests::TestClock;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn bounded<T>(work: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), work)
        .await
        .expect("accept producer stayed silent")
}
async fn recover(errno: i32, resource: bool) {
    let listener = Arc::new(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap());
    let address = listener.local_addr().unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let count = attempts.clone();
    let clock = Arc::new(TestClock::new());
    let health = Health::default();
    health.publish(Status::Listening);
    let task = tokio::spawn(serve(
        move || {
            let listener = listener.clone();
            let count = count.clone();
            async move {
                if count.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(io::Error::from_raw_os_error(errno))
                } else {
                    listener.accept().await
                }
            }
        },
        Router::new().route("/", axum::routing::get(|| async { "accepted" })),
        clock.clone(),
        health.clone(),
    ));
    if resource {
        clock.deadline(100).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        clock.advance(99);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        clock.advance(1);
    }
    let mut connection = bounded(tokio::net::TcpStream::connect(address))
        .await
        .unwrap();
    bounded(
        connection
            .write_all(b"GET / HTTP/1.1\r\nHost: fixture.example\r\nConnection: close\r\n\r\n"),
    )
    .await
    .unwrap();
    let mut response = String::new();
    bounded(connection.read_to_string(&mut response))
        .await
        .unwrap();
    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(response.ends_with("accepted"));
    assert_eq!(health.status(), Some(Status::Listening));
    assert!(!task.is_finished());
    task.abort();
    let _ = task.await;
}
#[tokio::test]
async fn connection_accept_errors_keep_serving() {
    for errno in [
        libc::ECONNABORTED,
        libc::ECONNRESET,
        libc::EINTR,
        libc::EPROTO,
    ] {
        recover(errno, false).await;
    }
}
#[tokio::test]
async fn resource_accept_errors_wait_on_the_supplied_clock_then_keep_serving() {
    for errno in [libc::EMFILE, libc::ENFILE, libc::ENOBUFS, libc::ENOMEM] {
        recover(errno, true).await;
    }
}
/// Construct a failed accept task without involving any installed server or external command.
pub(crate) async fn failed_listener() -> (crate::test_support::TempDir, Listener) {
    let data = crate::test_support::TempDir::socket();
    let core = Core::new(data.path().to_owned(), Arc::new(tokio::sync::Notify::new()))
        .await
        .unwrap();
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let health = Health::default();
    health.publish(Status::Listening);
    let task = tokio::spawn(serve(
        || async {
            Err::<(tokio::net::TcpStream, SocketAddr), _>(io::Error::from_raw_os_error(libc::EBADF))
        },
        Router::new(),
        Arc::new(SystemClock),
        health.clone(),
    ));
    // Observe termination with a bounded executor wait, without a test sleep.
    bounded(async {
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let local = super::super::local_listener::Listener::prepare(
        data.path().join("native.sock"),
        None,
        core.local_router(),
    )
    .unwrap();
    (
        data,
        Listener {
            health,
            socket: Some(socket),
            local,
            core,
            prepared: None,
            task: Some(task),
        },
    )
}
#[tokio::test]
async fn fatal_accept_marks_only_the_listener_failed() {
    let (_data, listener) = failed_listener().await;
    assert_eq!(listener.health.status(), Some(Status::Failed));
    listener.shutdown().await.unwrap();
}

fn probe_output(value: &str, exit: i32) -> std::process::Output {
    use std::os::unix::process::ExitStatusExt;
    std::process::Output {
        status: std::process::ExitStatus::from_raw(exit << 8),
        stdout: value.as_bytes().to_vec(),
        stderr: Vec::new(),
    }
}
#[test]
fn serve_and_funnel_targeting_the_bound_port_are_refused() {
    for config in [
        serde_json::json!({"Web":{"fixture.example:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:45678"}}}}}),
        serde_json::json!({"AllowFunnel":{"fixture.example:45678":true}}),
        serde_json::json!({"TCP":{"45678":{"HTTPS":true}}}),
        serde_json::json!({"Foreground":{"123":{"Web":{"fixture.example:443":{"Handlers":{"/":{"Proxy":"45678"}}}}}}}),
        serde_json::json!({"Services":{"svc:fixture":{"TCP":{"443":{"TCPForward":"127.0.0.1:45678"}}}}}),
    ] {
        let error = validate_funnel(probe_output(&config.to_string(), 0), 45678).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            error.to_string(),
            "tailscale serve/funnel may expose the client port"
        );
    }
}
#[test]
fn unrelated_serve_ports_are_allowed_and_status_failures_are_precise() {
    let config = r#"{"AllowFunnel":{"fixture.example:443":true},"Web":{"fixture.example:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:45679"}}}}}"#;
    validate_funnel(probe_output(config, 0), 45678).unwrap();
    let failed = validate_funnel(probe_output("invalid JSON", 7), 45678).unwrap_err();
    assert!(failed
        .to_string()
        .contains("tailscale serve status failed:"));
    assert!(!failed.to_string().contains("invalid tailscale status JSON"));
    assert_eq!(
        validate_funnel(probe_output("invalid JSON", 0), 45678)
            .unwrap_err()
            .to_string(),
        "invalid tailscale status JSON"
    );
    for proxy in [
        serde_json::Value::Null,
        serde_json::json!(false),
        serde_json::json!("http://["),
    ] {
        let config =
            serde_json::json!({"Web":{"fixture.example:443":{"Handlers":{"/":{"Proxy":proxy}}}}});
        assert_eq!(
            validate_funnel(probe_output(&config.to_string(), 0), 45678)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
#[test]
fn serve_proxy_port_forms_match_the_bound_authority() {
    for (target, port) in [
        ("45678", 45678),
        ("127.0.0.1:45678", 45678),
        ("http://127.0.0.1:45678/path", 45678),
        ("https://fixture.example", 443),
        ("https-proxy.example", 80),
        ("http://fixture.example", 80),
    ] {
        assert_eq!(target_port(target), Some(port));
    }
}

async fn refuse_wildcard(inherit_as_loopback: bool) {
    use crate::platform::client_listener::InvalidListener;
    let socket = TcpListener::bind("0.0.0.0:0").unwrap();
    let actual = socket.local_addr().unwrap();
    let requested = if inherit_as_loopback {
        SocketAddr::from((Ipv4Addr::LOCALHOST, actual.port()))
    } else {
        actual
    };
    let root = crate::test_support::TempDir::socket();
    let error = Listener::prepare(
        Some(requested),
        Codes::default(),
        root.path().to_owned(),
        Some(socket),
        (root.path().join("native.sock"), None),
        Health::default(),
        Core::new(root.path().to_owned(), Arc::new(tokio::sync::Notify::new()))
            .await
            .unwrap(),
    )
    .await
    .err()
    .expect("wildcard must refuse before the forwarding probe");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(
        error
            .get_ref()
            .and_then(|e| e.downcast_ref::<InvalidListener>()),
        Some(&InvalidListener::NonLoopback)
    );
}
#[tokio::test]
async fn configured_wildcard_listener_is_a_typed_refusal_before_any_probe() {
    refuse_wildcard(false).await;
}
#[tokio::test]
async fn inherited_wildcard_listener_is_a_typed_refusal_before_any_probe() {
    refuse_wildcard(true).await;
}

async fn assert_preparation_refusal(
    bind: Option<SocketAddr>,
    socket: Option<TcpListener>,
    expected: InvalidListener,
) {
    let root = crate::test_support::TempDir::socket();
    let error = Listener::prepare(
        bind,
        Codes::default(),
        root.path().into(),
        socket,
        (root.path().join("native.sock"), None),
        Health::default(),
        Core::new(root.path().to_owned(), Arc::new(tokio::sync::Notify::new()))
            .await
            .unwrap(),
    )
    .await
    .err()
    .expect("preparation must refuse before the forwarding probe");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(
        error
            .get_ref()
            .and_then(|error| error.downcast_ref::<InvalidListener>()),
        Some(&expected)
    );
}
#[tokio::test]
async fn alternate_loopback_address_has_typed_exact_binding_refusal() {
    assert_preparation_refusal(
        Some("127.0.0.5:0".parse().unwrap()),
        None,
        InvalidListener::NonLoopback,
    )
    .await;
}
#[tokio::test]
async fn inherited_listener_without_bind_has_typed_refusal() {
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    assert_preparation_refusal(None, Some(socket), InvalidListener::Unconfigured).await;
}
#[tokio::test]
async fn inherited_listener_port_mismatch_has_typed_refusal() {
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let actual = socket.local_addr().unwrap();
    let expected = SocketAddr::from((
        Ipv4Addr::LOCALHOST,
        if actual.port() == u16::MAX {
            1
        } else {
            actual.port() + 1
        },
    ));
    assert_preparation_refusal(
        Some(expected),
        Some(socket),
        InvalidListener::BindMismatch { expected, actual },
    )
    .await;
}
#[test]
fn server_failure_wins_over_listener_shutdown_failure_and_logs_secondary_kind() {
    let logs = crate::logging::capture_logs(|| {
        let error = finish_shutdown(
            Err(io::Error::from_raw_os_error(libc::EIO)),
            Err(io::Error::from_raw_os_error(libc::ENOMEM)),
        )
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EIO));
    });
    assert!(logs.contains("client.listener.shutdown_failed"));
    assert!(logs.contains("OutOfMemory"));
    assert!(logs.contains(&libc::ENOMEM.to_string()));
    assert_eq!(
        finish_shutdown(Ok(()), Err(io::Error::from_raw_os_error(libc::ENOMEM)))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENOMEM)
    );
    assert!(finish_shutdown(Ok(()), Ok(())).is_ok());
}

struct ShellServer {
    bind: SocketAddr,
    core: Core,
    _root: crate::structured::launch::SessionConfigDir,
    task: JoinHandle<io::Result<()>>,
}
impl Drop for ShellServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl ShellServer {
    async fn new() -> Self {
        let root =
            crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let clock = Arc::new(TestClock::new());
        let core = Core::with_clock(
            crate::structured::permissions::auth::tokens::TokenStore::open(root.path()).unwrap(),
            clock.clone(),
            Arc::new(tokio::sync::Notify::new()),
        )
        .unwrap();
        let listener = Arc::new(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap());
        let bind = listener.local_addr().unwrap();
        let router = core
            .router()
            .merge(client_exchange::router(Codes::default(), root.path().to_owned(), bind).unwrap())
            .route("/fixture", axum::routing::get(|| async { "same-origin" }))
            .layer(middleware::from_fn_with_state(
                (bind, HeaderValue::from_static("default-src 'none'")),
                guard,
            ));
        let task = tokio::spawn(serve(
            move || {
                let listener = listener.clone();
                async move { listener.accept().await }
            },
            router,
            clock,
            Health::default(),
        ));
        Self {
            bind,
            core,
            _root: root,
            task,
        }
    }
    async fn request(
        &self,
        path: &str,
        origin: &str,
        site: &str,
        extra: &str,
    ) -> (tokio::net::TcpStream, String) {
        bounded(async {
            let mut stream = tokio::net::TcpStream::connect(self.bind).await.unwrap();
            stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: {}\r\nOrigin: {origin}\r\nSec-Fetch-Site: {site}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{extra}\r\n",self.bind).as_bytes()).await.unwrap(); // gitleaks:allow -- RFC 6455 public example nonce
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                header.push(stream.read_u8().await.unwrap());
                assert!(header.len() < 8192);
            }
            (stream, String::from_utf8(header).unwrap())
        })
        .await
    }
}
#[tokio::test]
async fn captured_shell_origin_is_exact_cross_site_and_ws_only() {
    let server = ShellServer::new().await;
    let (stream, header) = server
        .request("/v1/ws", "tauri://localhost", "cross-site", "")
        .await;
    assert!(header.starts_with("HTTP/1.1 101"), "{header}");
    let mut socket = crate::web::wire::tests::Socket(stream);
    socket.send(serde_json::json!({"t":"hello","protocol":crate::structured_wire::WIRE_JSON_VERSION,"token":"bad"})).await;
    assert!(matches!(
        socket.receive().await,
        crate::web::wire::tests::Received::Close(4401)
    ));
    for (path, origin, site, extra) in [
        ("/fixture", "tauri://localhost", "cross-site", ""),
        ("/v1/client/exchange", "tauri://localhost", "cross-site", ""),
        ("/v1/ws/", "tauri://localhost", "cross-site", ""),
        ("/v1/ws", "tauri://localhost/", "cross-site", ""),
        ("/v1/ws", "tauri://localhost", "same-site", ""),
        ("/v1/ws", "null", "cross-site", ""),
        ("/v1/ws", "https://example.test", "cross-site", ""),
        (
            "/v1/ws",
            "tauri://localhost",
            "cross-site",
            "Host: example.test\r\n",
        ),
        (
            "/v1/ws",
            "tauri://localhost",
            "cross-site",
            "Origin: tauri://localhost\r\n",
        ),
        (
            "/v1/ws",
            "tauri://localhost",
            "cross-site",
            "Sec-Fetch-Site: same-origin\r\n",
        ),
    ] {
        let (_, header) = server.request(path, origin, site, extra).await;
        assert!(
            header.starts_with("HTTP/1.1 403"),
            "{path} {origin}: {header}"
        );
    }
    let origin = format!("http://{}", server.bind);
    let (_, header) = server.request("/fixture", &origin, "same-origin", "").await;
    assert!(header.starts_with("HTTP/1.1 200"));
    let (_, header) = server.request("/fixture", &origin, "cross-site", "").await;
    assert!(header.starts_with("HTTP/1.1 403"));
    server.core.shutdown().await.unwrap();
}

#[tokio::test]
async fn native_listener_is_prepared_when_tcp_is_disabled() {
    let root = crate::test_support::TempDir::socket();
    let listener = Listener::prepare(
        None,
        Codes::default(),
        root.path().into(),
        None,
        (root.path().join("flock-ws.sock"), None),
        Health::default(),
        Core::new(root.path().to_owned(), Arc::new(tokio::sync::Notify::new()))
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    let listener = listener.expect("native listener exists independently of TCP");
    assert!(listener.socket.is_none());
    assert!(super::super::local_listener::socket_path(&root.path().join("flock.sock")).exists());
    listener.shutdown().await.unwrap();
}
