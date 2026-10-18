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
    publish(Status::Listening);
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
    assert_eq!(
        crate::api::client_listener::status(),
        Some(Status::Listening)
    );
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
    publish(Status::Listening);
    let task = tokio::spawn(serve(
        || async { Err(io::Error::from_raw_os_error(libc::EBADF)) },
        Router::new(),
        Arc::new(SystemClock),
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
            socket,
            core,
            prepared: None,
            task: Some(task),
        },
    )
}
#[tokio::test]
async fn fatal_accept_marks_only_the_listener_failed() {
    let (_data, listener) = failed_listener().await;
    assert_eq!(crate::api::client_listener::status(), Some(Status::Failed));
    listener.shutdown().await.unwrap();
}
