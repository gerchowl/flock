//! Regression coverage for the server-work budgets and pre-dispatch refusal.
use super::*;
use crate::{
    api::deadline::{DeadlineStream, SocketTimeout, OPERATION_MARGIN, REQUEST_TIMEOUT},
    test_support::TempDir,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    sync::oneshot,
};

const SHORT: Duration = Duration::from_millis(100);
const CEILING: Duration = REQUEST_TIMEOUT.saturating_add(OPERATION_MARGIN);

/// A real API peer withholds the reply through the ordinary cap. Closing its
/// owned connection releases the worker even if the client deadline is broken.
async fn delayed_operation(method: Method) {
    let root = TempDir::socket();
    let path = root.path().join("work");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let request = Request {
        id: "budget".into(),
        method,
    };
    let client = ApiClient::for_target(ConnectionTarget::SocketPath(path));
    let sent = request.clone();
    let (send, mut receive) = oneshot::channel();
    let worker = std::thread::spawn(move || {
        let _ = send.send(client.request_value(&sent));
    });
    let (peer, _) = tokio::time::timeout(CEILING, listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut peer = tokio::io::BufReader::new(peer);
    let mut line = String::new();
    tokio::time::timeout(CEILING, peer.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), request);
    if let Ok(result) = tokio::time::timeout(REQUEST_TIMEOUT + SHORT, &mut receive).await {
        drop(peer);
        worker.join().unwrap();
        panic!("server work was cut off by the ordinary cap: {result:?}");
    }
    peer.get_mut()
        .write_all(b"{\"id\":\"budget\",\"result\":{\"type\":\"ok\"}}\n")
        .await
        .unwrap();
    let completion = tokio::time::timeout(OPERATION_MARGIN, &mut receive).await;
    drop(peer);
    let result = match completion {
        Ok(result) => result.unwrap(),
        Err(_) => {
            let _ = tokio::time::timeout(CEILING, receive).await;
            worker.join().unwrap();
            panic!("client did not finish after the positive server reply");
        }
    };
    worker.join().unwrap();
    assert_eq!(result.unwrap()["result"]["type"], "ok");
}

#[tokio::test]
async fn creation_work_outlasts_the_ordinary_request_cap() {
    delayed_operation(Method::WorkspaceCreate(
        crate::api::schema::WorkspaceCreateParams {
            dry_run: false,
            cwd: None,
            focus: false,
            label: None,
        },
    ))
    .await;
}

#[tokio::test]
async fn agent_start_outlasts_the_ordinary_request_cap() {
    let request: Request = serde_json::from_value(serde_json::json!({
        "id": "agent-budget", "method": "agent.start", "params": {"name": "budget-fixture"}
    }))
    .unwrap();
    delayed_operation(request.method).await;
}

#[tokio::test]
async fn default_guarded_write_reserves_write_and_confirmation() {
    let method = Method::PaneSendInput(crate::api::schema::PaneSendInputParams {
        pane_id: "w1:p1".into(),
        text: "brief".into(),
        keys: vec!["Enter".into()],
        write_timeout_ms: None,
        raw: false,
    });
    delayed_operation(method.clone()).await;
    assert_eq!(
        deadline::request_timeout(
            &Request {
                id: "default".into(),
                method
            },
            REQUEST_TIMEOUT
        ),
        Duration::from_millis(crate::api::schema::DEFAULT_RAW_WRITE_TIMEOUT_MS)
            + crate::app::guarded_submit::CONFIRM_WINDOW
            + OPERATION_MARGIN
    );
}

#[tokio::test]
async fn handoff_reply_reserves_synchronous_server_phases() {
    delayed_operation(Method::ServerLiveHandoff(Default::default())).await;
    let request = Request {
        id: "handoff".into(),
        method: Method::ServerLiveHandoff(Default::default()),
    };
    assert_eq!(
        deadline::request_timeout(&request, REQUEST_TIMEOUT),
        crate::server::handoff::operation_budget() + OPERATION_MARGIN
    );
}

fn assert_pre_dispatch(error: &io::Error) {
    assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error:?}");
    assert!(
        error.get_ref().is_some_and(|e| e.is::<SocketTimeout>()),
        "{error:?}"
    );
    assert_eq!(
        crate::api::error_code::ErrorCode::Timeout.exit(),
        crate::cli::exit::TIMEOUT
    );
}

#[tokio::test]
async fn full_backlog_mutation_is_pre_dispatch_timeout_exit_124() {
    let root = TempDir::socket();
    let path = root.path().join("backlog");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let _holders = crate::platform::socket_deadline::saturate_backlog(&listener, &path);
    let client = ApiClient::for_target(ConnectionTarget::SocketPath(path.clone()));
    let request = Request {
        id: "backlog".into(),
        method: Method::ServerStop(Default::default()),
    };
    // No invocation gate or request bytes: exercise the default mutating class.
    let mut worker = tokio::task::spawn_blocking(move || {
        client.request_value_with_timeout(&request, Duration::ZERO)
    });
    let completion = tokio::time::timeout(CEILING, &mut worker).await;
    let error = match completion {
        Ok(result) => result.unwrap().unwrap_err(),
        Err(_) => {
            drop(_holders);
            drop(listener);
            let _ = tokio::time::timeout(CEILING, worker).await;
            panic!("connect ignored its deadline while the listener stayed alive and silent");
        }
    };
    let ApiClientError::Io(error) = error else {
        panic!("unexpected connect failure: {error}")
    };
    assert_pre_dispatch(&error);
    // An exhausted budget wins before connecting, including Darwin's immediate
    // ECONNREFUSED on a full backlog. Linux EAGAIN must also be typed when
    // observed with time remaining, independently of the host's native result.
    assert_pre_dispatch(&DeadlineStream::connect_error(
        &path,
        Instant::now(),
        io::ErrorKind::WouldBlock.into(),
    ));
}
