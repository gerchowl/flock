//! Real accepted sockets with no response. Every failing harness closes its
//! owned peer before joining the client, including the pre-deadline control.
use super::*;
use crate::{
    api::deadline::{SocketTimeout, OPERATION_MARGIN, REQUEST_TIMEOUT},
    test_support::TempDir,
};
use std::thread;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt},
    sync::oneshot,
};

const SHORT: Duration = Duration::from_millis(100);
const FINISH_CAP: Duration = REQUEST_TIMEOUT.saturating_add(OPERATION_MARGIN);

fn io_error(error: ApiClientError) -> io::Error {
    match error {
        ApiClientError::Io(error) => error,
        error => io::Error::other(error),
    }
}
fn ping() -> Request {
    Request {
        id: "deadline".into(),
        method: Method::Ping(Default::default()),
    }
}
fn typed_timeout(error: &io::Error, path: &std::path::Path) {
    assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
    let timeout = error
        .get_ref()
        .and_then(|e| e.downcast_ref::<SocketTimeout>())
        .expect("typed socket expiry");
    assert_eq!(timeout.socket, path);
    assert!(!timeout.elapsed.is_zero());
    assert!(error.to_string().contains("timed out after"));
    assert!(error.to_string().contains(&path.display().to_string()));
}

async fn wedged(call: impl FnOnce(ApiClient) -> io::Result<()> + Send + 'static) {
    let root = TempDir::socket();
    let path = root.path().join("api");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let client = ApiClient::for_target(ConnectionTarget::SocketPath(path.clone()));
    let (send, mut receive) = oneshot::channel();
    let worker = thread::spawn(move || {
        let _ = send.send(call(client));
    });
    let (peer, _) = tokio::time::timeout(FINISH_CAP, listener.accept())
        .await
        .expect("client must connect")
        .unwrap();
    let mut peer = tokio::io::BufReader::new(peer);
    let mut request = String::new();
    tokio::time::timeout(FINISH_CAP, peer.read_line(&mut request))
        .await
        .expect("client must send its request")
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Request>(&request).unwrap().method,
        ping().method
    );
    let completion = tokio::time::timeout(FINISH_CAP, &mut receive).await;
    // Closing the accepted peer rescues a pre-fix client blocked in read_line.
    drop(peer);
    let on_time = completion.is_ok();
    let result = match completion {
        Ok(result) => result.unwrap(),
        Err(_) => tokio::time::timeout(FINISH_CAP, receive)
            .await
            .expect("closing the peer must release the client")
            .unwrap(),
    };
    worker.join().unwrap();
    assert!(on_time, "request exceeded its bounded harness: {result:?}");
    typed_timeout(&result.unwrap_err(), &path);
}

#[tokio::test]
async fn request_value_times_out_on_a_real_wedged_server() {
    wedged(|client| client.request_value(&ping()).map(|_| ()).map_err(io_error)).await;
}

#[tokio::test]
async fn status_ping_times_out_instead_of_reporting_server_not_running() {
    wedged(|client| crate::api::read_runtime_status_at(&client.socket_path(), SHORT).map(|_| ()))
        .await;
}

#[tokio::test]
async fn compatibility_probe_reports_a_typed_timeout() {
    wedged(|client| {
        crate::api::compatibility::probe(&client)
            .map(|_| ())
            .map_err(io_error)
    })
    .await;
}

#[tokio::test]
async fn a_long_poll_outlasts_the_ordinary_request_cap() {
    let root = TempDir::socket();
    let path = root.path().join("poll");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let client = ApiClient::for_target(ConnectionTarget::SocketPath(path));
    let hold = REQUEST_TIMEOUT + SHORT;
    let request = Request {
        id: "poll".into(),
        method: Method::EventsRead(crate::api::schema::EventsReadParams {
            wait_ms: Some(hold.as_millis() as u64),
            ..Default::default()
        }),
    };
    assert_eq!(
        deadline::request_timeout(&request, REQUEST_TIMEOUT),
        hold + OPERATION_MARGIN
    );
    let (send, mut receive) = oneshot::channel();
    let worker = thread::spawn(move || {
        let _ = send.send(client.request_value(&request));
    });
    let (peer, _) = tokio::time::timeout(FINISH_CAP, listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut peer = tokio::io::BufReader::new(peer);
    let mut line = String::new();
    tokio::time::timeout(FINISH_CAP, peer.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    let sent: Request = serde_json::from_str(&line).unwrap();
    assert!(matches!(sent.method, Method::EventsRead(_)));
    // This bounded receive is the assertion: the server has not completed its
    // operation yet, and the transport must not apply the shorter ordinary cap.
    let early = tokio::time::timeout(hold, &mut receive).await;
    if let Ok(result) = early {
        drop(peer);
        worker.join().unwrap();
        panic!("long poll was cut short: {result:?}");
    }
    peer.get_mut()
        .write_all(b"{\"id\":\"poll\",\"result\":{\"type\":\"events_read\",\"events\":[]}}\n")
        .await
        .unwrap();
    let result = tokio::time::timeout(OPERATION_MARGIN, &mut receive).await;
    drop(peer);
    let response = match result {
        Ok(result) => result.unwrap().unwrap(),
        Err(_) => {
            let _ = tokio::time::timeout(FINISH_CAP, receive).await;
            panic!("long poll did not return after its positive reply");
        }
    };
    worker.join().unwrap();
    assert_eq!(response["result"]["type"], "events_read");
}

#[tokio::test]
async fn status_cli_returns_timeout_exit_and_names_the_socket() {
    let root = TempDir::socket();
    let path = root.path().join("cli");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let binary = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/flk");
    let mut command = tokio::process::Command::new(binary);
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", root.path())
        .env("XDG_CONFIG_HOME", root.path().join("c"))
        .env("XDG_STATE_HOME", root.path().join("s"))
        .env("FLOCK_SOCKET_PATH", &path)
        .args(["status", "server", "--json"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let output = tokio::spawn(async move {
        let mut out = Vec::new();
        let mut err = Vec::new();
        stdout.read_to_end(&mut out).await.unwrap();
        stderr.read_to_end(&mut err).await.unwrap();
        (out, err)
    });
    let (peer, _) = tokio::time::timeout(FINISH_CAP, listener.accept())
        .await
        .expect("CLI must connect")
        .unwrap();
    let mut peer = tokio::io::BufReader::new(peer);
    let mut request = String::new();
    tokio::time::timeout(FINISH_CAP, peer.read_line(&mut request))
        .await
        .unwrap()
        .unwrap();
    let status = match tokio::time::timeout(FINISH_CAP, child.wait()).await {
        Ok(status) => status.unwrap(),
        Err(_) => {
            child.kill().await.unwrap();
            let (out, err) = tokio::time::timeout(OPERATION_MARGIN, output)
                .await
                .unwrap()
                .unwrap();
            panic!("CLI did not exit within its deadline: stdout={out:?}, stderr={err:?}");
        }
    };
    drop(peer);
    let (out, err) = tokio::time::timeout(OPERATION_MARGIN, output)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        status.code(),
        Some(crate::cli::exit::TIMEOUT),
        "stdout={out:?}, stderr={err:?}"
    );
    let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(value["error"]["code"], "timeout");
    assert!(value["error"]["message"]
        .as_str()
        .unwrap()
        .contains(&path.display().to_string()));
}
