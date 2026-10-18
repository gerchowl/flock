//! Real server, PTY writer and CLI: a blocked pane reports its own write result.
use super::*;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::{
        ffi::OsStrExt,
        fs::{OpenOptionsExt, PermissionsExt},
    },
    path::Path,
};
use support::server_startup::ServerStartup;
use tokio::sync::oneshot;

struct SlowPane {
    server: ServerStartup,
    release: File,
}
impl Drop for SlowPane {
    fn drop(&mut self) {
        let _ = self.release.write_all(b"exit\n");
        self.server.stop();
    }
}
fn setup(root: &Path, socket: &Path) -> SlowPane {
    let fifo = root.join("release");
    let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: name is NUL terminated and this fixture owns its directory.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let release = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&fifo)
        .unwrap();
    let shell = root.join("shell");
    fs::write(&shell, format!("#!/bin/sh\nstty -echo -icanon\nexec 3<'{}'\nprintf 'READY\\r\\n'\nIFS= read -r release <&3\n", fifo.display())).unwrap();
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
    let config = root.join("c");
    let runtime = root.join("r");
    let env = support::environment::isolated_env(&config, &runtime);
    let app_config = config.join(support::paths::app_dir_name());
    fs::create_dir_all(&app_config).unwrap();
    fs::write(app_config.join("config.toml"), "onboarding = false\n").unwrap();
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_flk"));
    command.arg("server");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("FLOCK_") {
            command.env_remove(key);
        }
    }
    for (key, value) in env {
        command.env(key, value);
    }
    command.env("FLOCK_SOCKET_PATH", socket);
    command.env("SHELL", &shell);
    let child = support::environment::spawn_pty(pair.slave.as_ref(), command).unwrap();
    drop(pair.slave);
    let server = ServerStartup::new(pair.master, child, socket, CEILING * 3).unwrap();
    let mut fixture = SlowPane { server, release };
    fixture.server.wait_responsive().unwrap();
    fixture
}
fn api(socket: &Path, request: Value) -> Value {
    serde_json::from_str(
        &support::json_api::request(socket, &request.to_string(), CEILING * 3).unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn default_write_budget_to_a_real_blocked_pane_returns_server_evidence() {
    let root = SocketDir::reserve("writebudget");
    let socket = root.path().join("api");
    let server = setup(root.path(), &socket);
    let created = api(
        &socket,
        json!({"id":"create", "method":"workspace.create", "params":{"cwd":root.path(), "focus":false}}),
    );
    let pane = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .expect("real pane");
    let ready = api(
        &socket,
        json!({"id":"ready", "method":"pane.wait_for_output", "params":{"pane_id":pane,"source":"recent","match":{"type":"substring","value":"READY"},"timeout_ms":5000}}),
    );
    assert!(ready.get("error").is_none(), "{ready}");
    assert!(!server.server.has_exited().unwrap());
    let proxy_path = root.path().join("proxy");
    let listener = UnixListener::bind(&proxy_path).unwrap();
    let (arrived, receive) = oneshot::channel();
    let (admit, admitted) = oneshot::channel();
    let mut peers = tokio::task::JoinSet::new();
    peers.spawn(async move {
        let mut arrived = Some(arrived);
        let mut admitted = Some(admitted);
        let mut evidence = Value::Null;
        for method in ["ping", "pane.send_text"] {
            let (stream, _) = listener.accept().await?;
            let mut peer = BufReader::new(stream);
            let mut line = String::new();
            assert!(
                peer.read_line(&mut line).await? > 0,
                "client exited before {method}"
            );
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], method);
            if method != "ping" {
                assert!(request["params"].get("write_timeout_ms").is_none());
                arrived.take().unwrap().send(()).unwrap();
                admitted.take().unwrap().await.unwrap();
            }
            let mut upstream = BufReader::new(tokio::net::UnixStream::connect(&socket).await?);
            upstream.get_mut().write_all(line.as_bytes()).await?;
            let mut reply = String::new();
            assert!(
                upstream.read_line(&mut reply).await? > 0,
                "server exited before {method}"
            );
            if method != "ping" {
                evidence = serde_json::from_str(&reply).unwrap();
            }
            peer.get_mut().write_all(reply.as_bytes()).await?;
        }
        Ok::<_, std::io::Error>(evidence)
    });
    let mut command = support::environment::Command::new(env!("CARGO_BIN_EXE_flk"));
    command.env("FLOCK_SOCKET_PATH", &proxy_path).args([
        "--json",
        "pane",
        "send-text",
        "--raw",
        pane,
        &"x".repeat(64 * 1024),
    ]);
    let mut command = tokio::process::Command::from(command.into_std());
    command.kill_on_drop(true);
    let mut clients = tokio::task::JoinSet::new();
    clients.spawn(async move { command.output().await });
    tokio::time::timeout(CEILING, receive)
        .await
        .expect("client must reach write admission")
        .unwrap();
    // Assert pending before admitting to the real writer. This reserves actual
    // dispatch overhead, making the old 10-second cap fail deterministically.
    if let Ok(early) = tokio::time::timeout(Duration::from_millis(500), clients.join_next()).await {
        panic!("CLI exited before the pane write was admitted: {early:?}");
    }
    admit.send(()).unwrap();
    let (proxy, output) = tokio::time::timeout(CEILING * 3, async {
        tokio::join!(peers.join_next(), clients.join_next())
    })
    .await
    .expect("write and both producers must finish");
    let output = output.unwrap().unwrap().unwrap();
    assert_eq!(
        output.status.code(),
        Some(exit::UNCONFIRMED),
        "{output:?}; proxy={proxy:?}"
    );
    let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reply["error"]["code"], "input_write_failed");
    let evidence = proxy.unwrap().unwrap().unwrap();
    assert_eq!(evidence["error"]["code"], "input_write_failed");
    let write = &evidence["error"]["data"]["input_write"];
    assert!(write["bytes_written"].as_u64().unwrap() > 0);
    assert!(write["bytes_written"].as_u64().unwrap() < write["bytes_total"].as_u64().unwrap());
    assert_eq!(write["remainder_discarded"], true);
    assert_eq!(write["reason"], "deadline");
    assert!(
        !server.server.has_exited().unwrap(),
        "the pane's server must stay alive"
    );
}
