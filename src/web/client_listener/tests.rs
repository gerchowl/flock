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
    let core = Core::new(data.path().to_owned()).await.unwrap();
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let health = Health::default();
    health.publish(Status::Listening);
    let task = tokio::spawn(serve(
        || async { Err(io::Error::from_raw_os_error(libc::EBADF)) },
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
    (
        data,
        Listener {
            health,
            socket: Arc::new(Mutex::new(Some(socket))),
            core: Arc::new(core),
            clock: Arc::new(SystemClock),
            probe: Arc::new(check_funnel),
            reload: Arc::new(Notify::new()),
            codes: Codes::default(),
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
        Health::default(),
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
        Health::default(),
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
async fn inherited_listener_without_bind_is_closed_and_reports_disabled() {
    use crate::platform::client_listener::{assert_fd_closed_in_child, isolated_fd_child};
    use std::os::fd::AsRawFd;
    if !isolated_fd_child("web::client_listener::tests::inherited_listener_without_bind_is_closed_and_reports_disabled") { return; }
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let fd = socket.as_raw_fd();
    let health = Health::default();
    health.publish(Status::Listening);
    let codes = Codes::default();
    codes.set_endpoint(socket.local_addr().unwrap()).unwrap();
    let root = crate::test_support::TempDir::socket();
    let result = Listener::prepare_using(
        None,
        codes.clone(),
        root.path().into(),
        Some(socket),
        health.clone(),
        Arc::new(SystemClock),
        Arc::new(|_| panic!("disabled listener must never probe")),
    )
    .await
    .unwrap();
    assert_fd_closed_in_child(fd);
    assert!(result.is_none());
    assert_eq!(health.status(), Some(Status::Disabled));
    assert_eq!(codes.bound_endpoint(), None);
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
    assert!(logs.contains(&format!("errno=Some({})", libc::ENOMEM)));
    assert_eq!(
        finish_shutdown(Ok(()), Err(io::Error::from_raw_os_error(libc::ENOMEM)))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENOMEM)
    );
    assert!(finish_shutdown(Ok(()), Ok(())).is_ok());
}

/// Isolated prepared listener for the headless handoff-export boundary test.
pub(crate) async fn prepared_listener() -> (crate::test_support::TempDir, Listener) {
    let root = crate::test_support::TempDir::socket();
    let listener = Listener::prepare_using(
        Some("127.0.0.1:0".parse().unwrap()),
        Codes::default(),
        root.path().into(),
        None,
        Health::default(),
        Arc::new(SystemClock),
        Arc::new(|_| Ok(())),
    )
    .await
    .unwrap()
    .unwrap();
    (root, listener)
}

/// Drive the real activation cleanup without an external forwarding probe.
pub(crate) async fn fail_activation(listener: &mut Listener) {
    listener.activate_with_addr(|_| Err(io::Error::from_raw_os_error(libc::EIO)));
    let error = bounded(listener.task.take().unwrap())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::EIO));
    assert!(listener.export_descriptor().unwrap().is_none());
}
