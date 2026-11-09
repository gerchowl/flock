//! Stale and missing sockets keep their pre-deadline status and CLI exits.
use super::*;
use crate::test_support::TempDir;

const FAST: Duration = Duration::from_millis(750);
const BUDGET: Duration = Duration::from_secs(2);

fn socket_fixture(stale: bool) -> (TempDir, PathBuf) {
    let root = TempDir::socket();
    let path = root.path().join("dead");
    if stale {
        // Leave the permanent ownership file too, as after a server crash.
        let owner = crate::ipc::SocketOwner::acquire(&path).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop((listener, owner));
        assert!(path.exists());
    }
    (root, path)
}

fn status_not_running(stale: bool) {
    let (_root, path) = socket_fixture(stale);
    let started = Instant::now();
    let status = crate::api::read_runtime_status_at(&path, BUDGET);
    let elapsed = started.elapsed();
    assert!(
        elapsed < FAST,
        "not-running status took {elapsed:?}: {status:?}"
    );
    assert_eq!(status.unwrap(), None);
    let client = ApiClient::for_target(ConnectionTarget::SocketPath(path));
    let request = Request {
        id: "dead".into(),
        method: Method::Ping(Default::default()),
    };
    let started = Instant::now();
    let error = client
        .request_value_with_timeout(&request, BUDGET)
        .unwrap_err();
    let elapsed = started.elapsed();
    let ApiClientError::Io(error) = error else {
        panic!("unexpected failure: {error}")
    };
    assert!(elapsed < FAST, "transport took {elapsed:?}: {error}");
    assert_eq!(
        error.kind(),
        if stale {
            io::ErrorKind::ConnectionRefused
        } else {
            io::ErrorKind::NotFound
        }
    );
    assert_eq!(
        crate::cli::output::io_error_code(&error).exit(),
        crate::cli::exit::FAILURE
    );
}

#[test]
fn stale_socket_status_is_not_running_and_preserves_transport_exit() {
    status_not_running(true);
}

#[test]
fn missing_socket_status_is_not_running_and_preserves_transport_exit() {
    status_not_running(false);
}

async fn cli_not_running(stale: bool) {
    let (root, path) = socket_fixture(stale);
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("flk");
    for status in [false, true] {
        let mut command = tokio::process::Command::new(&binary);
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", root.path())
            .env("XDG_CONFIG_HOME", root.path().join("c"))
            .env("XDG_STATE_HOME", root.path().join("s"))
            .env("FLOCK_SOCKET_PATH", &path)
            .args(if status {
                ["status", "server", "--json"].as_slice()
            } else {
                ["--json", "workspace", "rename", "ws_1", "new-label"].as_slice()
            })
            .kill_on_drop(true);
        let started = Instant::now();
        let output = tokio::time::timeout(Duration::from_secs(15), command.output())
            .await
            .expect("owned CLI failed to exit within harness ceiling")
            .unwrap();
        let elapsed = started.elapsed();
        let out = String::from_utf8_lossy(&output.stdout);
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(
            elapsed < FAST,
            "CLI took {elapsed:?}, exit {:?}, stdout={out}, stderr={err}",
            output.status.code()
        );
        assert_eq!(
            output.status.code(),
            Some(if status { 0 } else { crate::cli::exit::FAILURE }),
            "stdout={out}, stderr={err}"
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        if status {
            assert_eq!(value["status"], "not_running");
            assert_eq!(value["running"], false);
        } else {
            assert_eq!(value["error"]["code"], "transport");
            assert_eq!(value["error"]["exit"], 1);
        }
    }
}

#[tokio::test]
async fn stale_socket_cli_preserves_transport_and_not_running_exits() {
    cli_not_running(true).await;
}

#[tokio::test]
async fn missing_socket_cli_preserves_transport_and_not_running_exits() {
    cli_not_running(false).await;
}
