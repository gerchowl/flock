use super::*;
use crate::web::wire::tests::TestClock;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
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
    let supplied = clock.clone();
    publish(Status::Listening);
    let task = tokio::spawn(async move {
        accept_next(
            &mut move || {
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
            supplied.as_ref(),
        )
        .await
    });
    if resource {
        bounded(clock.deadline(100)).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        clock.advance(99);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        clock.advance(1);
    }
    let client = bounded(tokio::net::TcpStream::connect(address))
        .await
        .unwrap();
    let (mut accepted, peer) = bounded(task).await.unwrap().unwrap();
    assert_eq!(client.local_addr().unwrap(), peer);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    // Prove the recovered accept returns a real, usable connection.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    bounded(accepted.write_all(b"ok")).await.unwrap();
    let mut bytes = [0; 2];
    let mut client = client;
    bounded(client.read_exact(&mut bytes)).await.unwrap();
    assert_eq!(&bytes, b"ok");
    assert_eq!(
        crate::api::client_listener::status(),
        Some(Status::Listening)
    );
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
async fn resource_accept_errors_use_the_supplied_clock_then_keep_serving() {
    for errno in [libc::EMFILE, libc::ENFILE, libc::ENOBUFS, libc::ENOMEM] {
        recover(errno, true).await;
    }
}
#[tokio::test]
async fn fatal_accept_marks_only_the_listener_failed() {
    let clock = TestClock::new();
    for errno in [libc::EBADF, libc::EINVAL, libc::ENOTSOCK] {
        publish(Status::Listening);
        let mut calls = 0;
        let result: io::Result<()> = bounded(accept_next(
            &mut || {
                calls += 1;
                async move { Err(io::Error::from_raw_os_error(errno)) }
            },
            &clock,
        ))
        .await;
        assert_eq!(result.unwrap_err().raw_os_error(), Some(errno));
        assert_eq!(calls, 1);
        assert_eq!(crate::api::client_listener::status(), Some(Status::Failed));
    }
}
#[test]
fn listener_status_is_typed_and_optional_in_ping() {
    use crate::api::schema::ResponseResult;
    let pong = |status| ResponseResult::Pong {
        version: "fixture".into(),
        protocol: 5,
        capabilities: None,
        session_health: None,
        api_listener: None,
        client_listener: status,
        agent_account_diagnostic: None,
    };
    let absent = serde_json::to_value(pong(None)).unwrap();
    assert!(absent.get("client_listener").is_none());
    let decoded: ResponseResult = serde_json::from_value(absent).unwrap();
    assert!(matches!(
        decoded,
        ResponseResult::Pong {
            client_listener: None,
            ..
        }
    ));
    for status in [Status::Listening, Status::Failed] {
        let value = serde_json::to_value(pong(Some(status))).unwrap();
        assert_eq!(
            value["client_listener"],
            serde_json::to_value(status).unwrap()
        );
        let decoded: ResponseResult = serde_json::from_value(value).unwrap();
        assert!(
            matches!(decoded,ResponseResult::Pong{client_listener:Some(observed),..} if observed==status)
        );
    }
}
