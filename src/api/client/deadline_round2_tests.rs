//! Regression coverage for the server-work budgets and pre-dispatch refusal.
use super::*;
use crate::{
    api::deadline::{SocketTimeout, OPERATION_MARGIN, REQUEST_TIMEOUT},
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
        "id": "agent-budget", "method": "agent.start", "params": {"name": "budget-fixture", "argv": ["/bin/sh"]}
    }))
    .unwrap();
    delayed_operation(request.method).await;
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
        crate::cli::output::io_error_code(error).exit(),
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
    let budget = Duration::from_millis(250);
    let mut worker = tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let result = client.request_value_with_timeout(&request, budget);
        (started.elapsed(), result)
    });
    let completion = tokio::time::timeout(CEILING, &mut worker).await;
    let (elapsed, error) = match completion {
        Ok(result) => {
            let (elapsed, result) = result.unwrap();
            (elapsed, result.unwrap_err())
        }
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
    assert!(
        elapsed >= budget,
        "connect failed before exhausting its budget: {elapsed:?}"
    );
    assert_pre_dispatch(&error);
}

#[test]
fn completed_input_connect_uses_its_existing_absolute_deadline() {
    let root = TempDir::socket();
    let path = root.path().join("input-backlog");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let _holders = crate::platform::socket_deadline::saturate_backlog(&listener, &path);
    let client = ApiClient::for_target(ConnectionTarget::SocketPath(path));
    let request: Request = serde_json::from_value(serde_json::json!({
        "id":"input-connect", "method":"pane.write_text", "params":{"pane_id":"w1:p1", "text":"brief"}
    })).unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let budget = Duration::from_millis(250);
    let worker = std::thread::spawn(move || {
        let started = Instant::now();
        let result = client.exchange_until(&request, started + budget);
        let _ = send.send((started.elapsed(), result));
    });
    let completion = receive.recv_timeout(OPERATION_MARGIN);
    let on_time = completion.is_ok();
    drop(_holders);
    drop(listener);
    let (elapsed, result) = completion.unwrap_or_else(|_| {
        receive
            .recv_timeout(OPERATION_MARGIN)
            .expect("closing the owned backlog must release connect")
    });
    worker.join().unwrap();
    assert!(on_time, "input connect exceeded its absolute deadline");
    assert!(
        elapsed >= budget,
        "input connect failed before exhausting its budget: {elapsed:?}"
    );
    let error = result.unwrap_err().into_error();
    let ApiClientError::Io(error) = error else {
        panic!("unexpected failure: {error}")
    };
    assert_pre_dispatch(&error);
}
