// TracedCommand (logging redesign PR-3) polices shipped code; this harness
// exec's raw lsof/pgrep to inspect the running system under test.
#![allow(clippy::disallowed_methods)]

mod support;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use support::{
    cleanup_test_base, client_handshake, register_runtime_dir, register_spawned_flock_pid,
    send_input, unregister_spawned_flock_pid, wait_for_disconnect, wait_for_socket,
};

struct SpawnedFlock {
    _master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

struct RequestError {
    retryable: bool,
    message: String,
}

impl Drop for SpawnedFlock {
    fn drop(&mut self) {
        let pid = self.child.process_id();
        let _ = self.child.kill();
        unregister_spawned_flock_pid(pid);
    }
}

/// Serialises this binary's server-spawning tests: at most one server-spawning
/// test at a time *within this process*.
///
/// It serialises tests, not servers: one test here may legitimately hold
/// several `flk server` processes at once — a live handoff runs the old and
/// the replacement server together by construction, and
/// `duplicate_server_start_fails_gracefully` starts a second one on purpose.
///
/// The `OnceLock` below is function-local, so it is per-crate-per-binary. It
/// does NOT coordinate with the identical `test_lock()` in api_ping.rs,
/// client_mode.rs, cross_area.rs and the rest: nextest runs each integration
/// binary as its own process, concurrently. "One flock server per test at a
/// time" therefore holds per binary, not across binaries — do not read this as
/// a global lock, and do not assume a sibling binary's server is idle.
///
/// What actually keeps concurrent binaries off each other's paths is naming:
/// each test roots its config home, runtime dir and socket inside its own
/// `unique_test_dir()` (a per-binary prefix plus this process's pid and a
/// unique suffix), and the spawn helpers redirect `XDG_CONFIG_HOME` and
/// `XDG_RUNTIME_DIR` into that tree. Most also pass an explicit
/// `FLOCK_SOCKET_PATH` inside it; three deliberately `env_remove` it to
/// exercise default path resolution (`live_handoff.rs`, `auto_detect.rs`), and
/// that still lands in the same tree, because the default socket derives from
/// the config home — pinned by
/// `socket_path_defaults_to_config_dir_even_when_xdg_runtime_dir_is_set` in
/// `src/api/server.rs`. No two tests, in this binary or any other, share a
/// socket, config or runtime dir. Cross-binary CPU contention is nextest's to
/// schedule — see the `serial-pty` group and the retries in
/// `.config/nextest.toml`.
fn test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn unique_test_dir() -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    PathBuf::from(format!("/tmp/hlh-{}-{n}", std::process::id()))
}

fn spawn_server(config_home: &Path, runtime_dir: &Path, api_socket: &Path) -> SpawnedFlock {
    spawn_server_with_env(config_home, runtime_dir, api_socket, &[])
}

fn spawn_server_with_env(
    config_home: &Path,
    runtime_dir: &Path,
    api_socket: &Path,
    extra_env: &[(&str, &str)],
) -> SpawnedFlock {
    fs::create_dir_all(config_home.join("flock")).unwrap();
    fs::create_dir_all(runtime_dir).unwrap();
    fs::write(
        config_home.join("flock/config.toml"),
        "onboarding = false\n",
    )
    .unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_flk"));
    cmd.arg("server");
    // Config diagnostics must come from the fixture, not the runner's name aliases.
    cmd.env_remove("FLOCK_HOST_NAME");
    cmd.env_remove("FLOCK_NAME");
    for (key, value) in support::environment::isolated_env(config_home, runtime_dir) {
        cmd.env(key, value);
    }
    cmd.env("XDG_STATE_HOME", runtime_dir.join("state"));
    cmd.env("FLOCK_SOCKET_PATH", api_socket);
    cmd.env(
        "FLOCK_CLIENT_SOCKET_PATH",
        runtime_dir.join("flock-client.sock"),
    );
    cmd.env("SHELL", "/bin/sh");
    for (key, value) in extra_env {
        cmd.env(key, value);
    }

    support::environment::assert_pty_isolated(&cmd);
    let child = support::environment::spawn_pty(pair.slave.as_ref(), cmd).unwrap();
    register_spawned_flock_pid(child.process_id());
    SpawnedFlock {
        _master: pair.master,
        child,
    }
}

fn spawn_named_session_server(
    config_home: &Path,
    runtime_dir: &Path,
    session_name: &str,
) -> SpawnedFlock {
    fs::create_dir_all(config_home.join("flock-dev")).unwrap();
    fs::create_dir_all(runtime_dir).unwrap();
    fs::write(
        config_home.join("flock-dev/config.toml"),
        "onboarding = false\n",
    )
    .unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_flk"));
    cmd.arg("server");
    for (key, value) in support::environment::isolated_env(config_home, runtime_dir) {
        cmd.env(key, value);
    }
    cmd.env("XDG_STATE_HOME", runtime_dir.join("state"));
    cmd.env("FLOCK_SESSION", session_name);
    cmd.env_remove("FLOCK_SOCKET_PATH");
    cmd.env_remove("FLOCK_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");

    support::environment::assert_pty_isolated(&cmd);
    let child = support::environment::spawn_pty(pair.slave.as_ref(), cmd).unwrap();
    register_spawned_flock_pid(child.process_id());
    SpawnedFlock {
        _master: pair.master,
        child,
    }
}

fn spawn_default_session_server(config_home: &Path, runtime_dir: &Path) -> SpawnedFlock {
    fs::create_dir_all(config_home.join("flock-dev")).unwrap();
    fs::create_dir_all(runtime_dir).unwrap();
    fs::write(
        config_home.join("flock-dev/config.toml"),
        "onboarding = false\n",
    )
    .unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_flk"));
    cmd.arg("server");
    for (key, value) in support::environment::isolated_env(config_home, runtime_dir) {
        cmd.env(key, value);
    }
    cmd.env("XDG_STATE_HOME", runtime_dir.join("state"));
    cmd.env_remove("FLOCK_SESSION");
    cmd.env_remove("FLOCK_SOCKET_PATH");
    cmd.env_remove("FLOCK_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");

    support::environment::assert_pty_isolated(&cmd);
    let child = support::environment::spawn_pty(pair.slave.as_ref(), cmd).unwrap();
    register_spawned_flock_pid(child.process_id());
    SpawnedFlock {
        _master: pair.master,
        child,
    }
}

fn try_request(
    socket_path: &Path,
    request: serde_json::Value,
) -> Result<serde_json::Value, RequestError> {
    let mut stream = UnixStream::connect(socket_path).map_err(|err| RequestError {
        retryable: true,
        message: format!("connect {}: {err}", socket_path.display()),
    })?;
    let request_text = request.to_string();
    stream
        .write_all(request_text.as_bytes())
        .map_err(|err| RequestError {
            retryable: true,
            message: format!("write request to {}: {err}", socket_path.display()),
        })?;
    stream.write_all(b"\n").map_err(|err| RequestError {
        retryable: true,
        message: format!("write newline to {}: {err}", socket_path.display()),
    })?;
    stream.flush().map_err(|err| RequestError {
        retryable: true,
        message: format!("flush request to {}: {err}", socket_path.display()),
    })?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|err| RequestError {
            retryable: true,
            message: format!("read response from {}: {err}", socket_path.display()),
        })?;
    if line.is_empty() {
        return Err(RequestError {
            retryable: true,
            message: format!(
                "empty response from {} for request {request_text}",
                socket_path.display()
            ),
        });
    }
    serde_json::from_str(&line).map_err(|err| RequestError {
        retryable: false,
        message: format!(
            "parse response from {} for request {request_text}: {err}; response was {line:?}",
            socket_path.display()
        ),
    })
}

fn request(socket_path: &Path, request: serde_json::Value) -> serde_json::Value {
    try_request(socket_path, request).unwrap_or_else(|err| panic!("{}", err.message))
}

fn assert_ok(response: serde_json::Value) {
    assert!(
        response.get("result").is_some(),
        "api request failed: {response}"
    );
}

fn wait_for_api(socket_path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let mut last_error = String::new();
    while Instant::now() < deadline {
        match try_request(
            socket_path,
            serde_json::json!({"id":"test:ping","method":"ping","params":{}}),
        ) {
            Ok(response) if response.get("result").is_some() => return,
            Ok(response) => panic!("api ping returned non-success response: {response}"),
            Err(err) if !err.retryable => panic!("{}", err.message),
            Err(err) => {
                last_error = err.message;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "api did not become ready at {}; last error: {last_error}",
        socket_path.display()
    );
}

fn wait_for_output(socket_path: &Path, pane_id: &str, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_text = String::new();
    let mut last_response = serde_json::Value::Null;
    while Instant::now() < deadline {
        let response = request(
            socket_path,
            serde_json::json!({
                "id": "test:pane:read",
                "method": "pane.read",
                "params": {
                    "pane_id": pane_id,
                    "source": "visible",
                    "lines": 20,
                    "format": "text",
                    "strip_ansi": true
                }
            }),
        );
        last_response = response.clone();
        let text = response["result"]["read"]["text"]
            .as_str()
            .unwrap_or_default();
        last_text = text.to_string();
        if text.contains(needle) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "pane output did not contain {needle:?}; last text was {last_text:?}; last response was {last_response}"
    );
}

fn wait_for_file_contains(path: &Path, needle: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    let mut last_text = String::new();
    while Instant::now() < deadline {
        if let Ok(text) = fs::read_to_string(path) {
            last_text = text;
            if last_text.contains(needle) {
                return last_text;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "{} did not contain {needle:?}; last text was {last_text:?}",
        path.display()
    );
}

#[cfg(target_os = "linux")]
fn server_ptmx_fd_count(pid: u32) -> usize {
    let Ok(entries) = fs::read_dir(format!("/proc/{pid}/fd")) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| fs::read_link(entry.path()).ok())
        .filter(|target| target == Path::new("/dev/ptmx"))
        .count()
}

#[cfg(target_os = "macos")]
fn server_ptmx_fd_count(pid: u32) -> usize {
    let Ok(output) = std::process::Command::new("lsof")
        .args(["-nP", "-p", &pid.to_string()])
        .output()
    else {
        return 0;
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains("/dev/ptmx"))
        .count()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wait_for_server_ptmx_fd_count(pid: u32, expected: usize, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let mut last_count = 0;
    while Instant::now() < deadline {
        last_count = server_ptmx_fd_count(pid);
        if last_count == expected {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("server pid {pid} had {last_count} /dev/ptmx fds; expected {expected}");
}

/// Take ownership of the server the handoff spawned.
///
/// A live handoff replaces the server with a NEW process that this harness did
/// not spawn: `SpawnedFlock::drop` only kills its own child, so without this
/// the replacement is a grandchild nobody reaps. `register_spawned_flock_pid`
/// puts it under the same atexit / panic / ctrl-c hooks as a spawned server,
/// which is what makes cleanup survive an abnormal exit too.
///
/// The runtime-dir sweep in `support` would otherwise be the safety net, but it
/// enumerates `/proc` and so is inert on macOS — a leaked server there simply
/// runs until the machine is rebooted. One was found alive after 11 hours.
fn adopt_replacement_server(pid: u32) -> u32 {
    support::register_spawned_flock_pid(Some(pid));
    pid
}

#[cfg(target_os = "linux")]
fn wait_for_replacement_server_pid(runtime_dir: &Path, old_pid: u32, timeout: Duration) -> u32 {
    let deadline = Instant::now() + timeout;
    let mut last_pids = Vec::new();
    while Instant::now() < deadline {
        last_pids = support::flock_server_pids_for_runtime_dir(runtime_dir).unwrap_or_default();
        if let Some(pid) = last_pids.iter().copied().find(|pid| *pid != old_pid) {
            return adopt_replacement_server(pid);
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "replacement server for {} did not appear; last pids: {:?}",
        runtime_dir.display(),
        last_pids
    );
}

#[cfg(target_os = "macos")]
fn wait_for_replacement_server_pid(_runtime_dir: &Path, old_pid: u32, timeout: Duration) -> u32 {
    let handoff_socket_pattern = format!("flock-handoff-{old_pid}.sock");
    let deadline = Instant::now() + timeout;
    let mut last_stdout = String::new();
    while Instant::now() < deadline {
        if let Ok(output) = std::process::Command::new("pgrep")
            .args(["-af", &handoff_socket_pattern])
            .output()
        {
            last_stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            for line in last_stdout.lines() {
                let Some(pid_text) = line.split_whitespace().next() else {
                    continue;
                };
                let Ok(pid) = pid_text.parse::<u32>() else {
                    continue;
                };
                if pid != old_pid {
                    return adopt_replacement_server(pid);
                }
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "replacement server for {} did not appear; last pgrep output: {}",
        _runtime_dir.display(),
        last_stdout
    );
}

fn unused_local_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_for_http_contains(port: u16, needle: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    let mut last_response = String::new();
    while Instant::now() < deadline {
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            let _ =
                stream.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
            let mut response = String::new();
            let _ = stream.read_to_string(&mut response);
            last_response = response;
            if last_response.contains(needle) {
                return last_response;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "http server on port {port} did not return {needle:?}; last response was {last_response:?}"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn live_server_holds_one_pty_master_fd_per_pane() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);
    let server_pid = spawned
        .child
        .process_id()
        .expect("test server should expose pid");
    wait_for_server_ptmx_fd_count(server_pid, 0, Duration::from_secs(5));

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    wait_for_server_ptmx_fd_count(server_pid, 1, Duration::from_secs(5));

    let second = request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:split-second",
            "method": "pane.split",
            "params": {
                "target_pane_id": pane_id,
                "direction": "right",
                "focus": true
            }
        }),
    );
    assert_ok(second.clone());
    let second_pane_id = second["result"]["pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    wait_for_server_ptmx_fd_count(server_pid, 2, Duration::from_secs(5));

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:split-third",
            "method": "pane.split",
            "params": {
                "target_pane_id": second_pane_id,
                "direction": "down",
                "focus": true
            }
        }),
    ));
    wait_for_server_ptmx_fd_count(server_pid, 3, Duration::from_secs(5));

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    let replacement_pid =
        wait_for_replacement_server_pid(&runtime_dir, server_pid, Duration::from_secs(10));
    wait_for_api(&api_socket, Duration::from_secs(10));
    wait_for_server_ptmx_fd_count(replacement_pid, 3, Duration::from_secs(5));

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    drop(spawned);
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_preserves_named_session_socket_paths() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let session_dir = config_home.join("flock-dev/sessions/work");
    let api_socket = session_dir.join("flock.sock");
    let client_socket = session_dir.join("flock-client.sock");

    let spawned = spawn_named_session_server(&config_home, &runtime_dir, "work");
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let before = request(
        &api_socket,
        serde_json::json!({"id":"identity-before","method":"ping","params":{}}),
    );
    let node_id = before["result"]["capabilities"]["node_id"]
        .as_str()
        .expect("persisted node id");

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(5));
    assert!(
        !config_home.join("flock-dev/flock.sock").exists(),
        "named handoff unexpectedly bound the default session API socket"
    );

    let after = request(
        &api_socket,
        serde_json::json!({"id":"identity-after","method":"ping","params":{}}),
    );
    assert_eq!(after["result"]["capabilities"]["node_id"], node_id);

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_preserves_pane_process_io() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");
    let client_socket = runtime_dir.join("flock-client.sock");
    let marker = base.join("child.pid");
    let second_marker = base.join("second-child.pid");
    let hup_marker = base.join("hup");
    let second_hup_marker = base.join("second-hup");
    let received_marker = base.join("received");
    let second_received_marker = base.join("second-received");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let split = request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:split",
            "method": "pane.split",
            "params": {
                "target_pane_id": pane_id,
                "direction": "right",
                "focus": false
            }
        }),
    );
    assert_ok(split.clone());
    let second_pane_id = split["result"]["pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    let command = format!(
        "sh -c 'echo READY $$ > {}; trap \"echo HUP >> {}\" HUP; while read line; do echo got:$line; echo got:$line >> {}; done'",
        marker.display(),
        hup_marker.display(),
        received_marker.display()
    );
    let second_command = format!(
        "sh -c 'echo SECOND_READY $$ > {}; trap \"echo HUP >> {}\" HUP; while read line; do echo second:$line; echo second:$line >> {}; done'",
        second_marker.display(),
        second_hup_marker.display(),
        second_received_marker.display()
    );
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:run",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": command, "keys": ["Enter"]}
        }),
    ));
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:second-pane:run",
            "method": "pane.send_input",
            "params": {"pane_id": second_pane_id, "text": second_command, "keys": ["Enter"]}
        }),
    ));
    support::wait_for_file(&marker, Duration::from_secs(5));
    support::wait_for_file(&second_marker, Duration::from_secs(5));
    let pid_text = fs::read_to_string(&marker).unwrap();
    let child_pid: u32 = pid_text.split_whitespace().last().unwrap().parse().unwrap();
    let second_pid_text = fs::read_to_string(&second_marker).unwrap();
    let second_child_pid: u32 = second_pid_text
        .split_whitespace()
        .last()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::kill(child_pid as libc::pid_t, 0) }, 0);
    assert_eq!(unsafe { libc::kill(second_child_pid as libc::pid_t, 0) }, 0);

    let protocol = request(
        &api_socket,
        serde_json::json!({"id":"test:protocol","method":"ping","params":{}}),
    )["result"]["protocol"]
        .as_u64()
        .unwrap() as u32;
    let mut client_stream = UnixStream::connect(&client_socket).unwrap();
    let (server_protocol, error) = client_handshake(&mut client_stream, protocol, 80, 24).unwrap();
    assert_eq!(server_protocol, protocol);
    assert!(error.is_none(), "client handshake failed: {error:?}");

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:before-log",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": "before_replay", "keys": ["Enter"]}
        }),
    ));
    wait_for_output(&api_socket, &pane_id, "got:before_replay");

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    assert!(
        wait_for_disconnect(&mut client_stream, Duration::from_secs(5)).unwrap(),
        "connected clients should disconnect during live handoff"
    );
    thread::sleep(Duration::from_millis(300));
    wait_for_api(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(5));
    assert_eq!(unsafe { libc::kill(child_pid as libc::pid_t, 0) }, 0);
    assert_eq!(unsafe { libc::kill(second_child_pid as libc::pid_t, 0) }, 0);
    assert!(
        !hup_marker.exists(),
        "pane process received HUP during handoff"
    );
    assert!(
        !second_hup_marker.exists(),
        "second pane process received HUP during handoff"
    );
    wait_for_output(&api_socket, &pane_id, "got:before_replay");

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:send",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": "after-handoff", "keys": ["Enter"]}
        }),
    ));
    wait_for_file_contains(
        &received_marker,
        "got:after-handoff",
        Duration::from_secs(5),
    );
    wait_for_output(&api_socket, &pane_id, "got:after-handoff");
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:second-pane:send",
            "method": "pane.send_input",
            "params": {"pane_id": second_pane_id, "text": "after-handoff-second", "keys": ["Enter"]}
        }),
    ));
    wait_for_file_contains(
        &second_received_marker,
        "second:after-handoff-second",
        Duration::from_secs(5),
    );
    wait_for_output(&api_socket, &second_pane_id, "second:after-handoff-sec");

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    let _ = client_socket;
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_preserves_keyboard_protocol_for_client_input() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");
    let client_socket = runtime_dir.join("flock-client.sock");
    let script = base.join("read-raw.py");
    let ready_marker = base.join("keyboard-ready");
    let received_marker = base.join("keyboard-received");

    fs::create_dir_all(&base).unwrap();
    fs::write(
        &script,
        format!(
            r#"import os
import pathlib
import select
import sys
import tty

sys.stdout.buffer.write(b"\x1b[>5u")
sys.stdout.flush()
pathlib.Path({ready:?}).write_text("ready")
tty.setraw(sys.stdin.fileno())
ready_fds, _, _ = select.select([sys.stdin.fileno()], [], [], 5)
data = os.read(sys.stdin.fileno(), 32) if ready_fds else b""
pathlib.Path({received:?}).write_text(data.hex())
"#,
            ready = ready_marker.display().to_string(),
            received = received_marker.display().to_string()
        ),
    )
    .unwrap();

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:run",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": format!("python3 {}", script.display()), "keys": ["Enter"]}
        }),
    ));
    support::wait_for_file(&ready_marker, Duration::from_secs(5));

    let protocol = request(
        &api_socket,
        serde_json::json!({"id":"test:protocol","method":"ping","params":{}}),
    )["result"]["protocol"]
        .as_u64()
        .unwrap() as u32;
    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(5));

    let mut client_stream = UnixStream::connect(&client_socket).unwrap();
    let (server_protocol, error) = client_handshake(&mut client_stream, protocol, 80, 24).unwrap();
    assert_eq!(server_protocol, protocol);
    assert!(error.is_none(), "client handshake failed: {error:?}");
    send_input(&mut client_stream, b"\x1b[13;2u").unwrap();

    wait_for_file_contains(&received_marker, "1b5b31333b3275", Duration::from_secs(5));

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_preserves_modify_other_keys_for_client_input() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");
    let client_socket = runtime_dir.join("flock-client.sock");
    let script = base.join("read-raw.py");
    let ready_marker = base.join("modify-ready");
    let received_marker = base.join("modify-received");

    fs::create_dir_all(&base).unwrap();
    fs::write(
        &script,
        format!(
            r#"import os
import pathlib
import select
import sys
import tty

sys.stdout.buffer.write(b"\x1b[>4;2m")
sys.stdout.flush()
pathlib.Path({ready:?}).write_text("ready")
tty.setraw(sys.stdin.fileno())
ready_fds, _, _ = select.select([sys.stdin.fileno()], [], [], 5)
data = os.read(sys.stdin.fileno(), 32) if ready_fds else b""
pathlib.Path({received:?}).write_text(data.hex())
"#,
            ready = ready_marker.display().to_string(),
            received = received_marker.display().to_string()
        ),
    )
    .unwrap();

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:run",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": format!("python3 {}", script.display()), "keys": ["Enter"]}
        }),
    ));
    support::wait_for_file(&ready_marker, Duration::from_secs(5));

    let protocol = request(
        &api_socket,
        serde_json::json!({"id":"test:protocol","method":"ping","params":{}}),
    )["result"]["protocol"]
        .as_u64()
        .unwrap() as u32;
    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(5));

    let mut client_stream = UnixStream::connect(&client_socket).unwrap();
    let (server_protocol, error) = client_handshake(&mut client_stream, protocol, 80, 24).unwrap();
    assert_eq!(server_protocol, protocol);
    assert!(error.is_none(), "client handshake failed: {error:?}");
    send_input(&mut client_stream, b"\x1b[13;2u").unwrap();

    wait_for_file_contains(
        &received_marker,
        "1b5b32373b323b31337e",
        Duration::from_secs(5),
    );

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_accepts_old_pane_id_from_child_env() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");
    let pane_id_marker = base.join("old-pane-id");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:print-id",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": format!("printf '%s' \"$FLOCK_PANE_ID\" > {}", pane_id_marker.display()), "keys": ["Enter"]}
        }),
    ));
    let old_pane_id = wait_for_file_contains(&pane_id_marker, "p_", Duration::from_secs(5));
    assert!(
        old_pane_id.starts_with("p_"),
        "unexpected pane id from env: {old_pane_id:?}"
    );

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:old-pane-report",
            "method": "pane.report_agent",
            "params": {
                "pane_id": old_pane_id,
                "source": "handoff-test",
                "agent": "pi",
                "state": "working"
            }
        }),
    ));
    let agents = request(
        &api_socket,
        serde_json::json!({"id":"test:agent-list","method":"agent.list","params":{}}),
    );
    let found = agents["result"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .any(|agent| {
            agent["agent"].as_str() == Some("pi")
                && agent["agent_status"].as_str() == Some("working")
        });
    assert!(
        found,
        "old pane id report did not update restored pane: {agents}"
    );

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_keeps_agent_started_pane_after_agent_exits() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");
    let started_marker = base.join("agent-started");
    let exited_marker = base.join("agent-exited");
    let shell_marker = base.join("shell-after-agent");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let command = format!(
        "echo started > {}; sleep 1; echo exited > {}",
        started_marker.display(),
        exited_marker.display()
    );
    let started = request(
        &api_socket,
        serde_json::json!({
            "id": "test:agent-start",
            "method": "agent.start",
            "params": {
                "name": "handoff-agent",
                "cwd": "/tmp",
                "focus": true,
                "argv": ["/bin/sh", "-c", command]
            }
        }),
    );
    assert_ok(started.clone());
    let pane_id = started["result"]["agent"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    support::wait_for_file(&started_marker, Duration::from_secs(5));

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));
    support::wait_for_file(&exited_marker, Duration::from_secs(5));
    thread::sleep(Duration::from_millis(300));

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:shell-after-agent",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": format!("echo alive > {}", shell_marker.display()), "keys": ["Enter"]}
        }),
    ));
    support::wait_for_file(&shell_marker, Duration::from_secs(5));

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_keeps_shell_pane_after_foreground_process_exits() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");
    let started_marker = base.join("foreground-started");
    let exited_marker = base.join("foreground-exited");
    let shell_marker = base.join("shell-after-foreground");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let command = format!(
        "sh -c 'echo started > {}; sleep 1; echo exited > {}'",
        started_marker.display(),
        exited_marker.display()
    );
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:run-foreground",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": command, "keys": ["Enter"]}
        }),
    ));
    support::wait_for_file(&started_marker, Duration::from_secs(5));

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));
    support::wait_for_file(&exited_marker, Duration::from_secs(5));

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:shell-after-foreground",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": format!("echo alive > {}", shell_marker.display()), "keys": ["Enter"]}
        }),
    ));
    support::wait_for_file(&shell_marker, Duration::from_secs(5));

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_preserves_python_http_server() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");
    let client_socket = runtime_dir.join("flock-client.sock");
    let web_root = base.join("web");
    fs::create_dir_all(&web_root).unwrap();
    fs::write(
        web_root.join("index.html"),
        "hello-from-python-before-and-after",
    )
    .unwrap();
    let port = unused_local_port();

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": web_root, "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:run-python",
            "method": "pane.send_input",
            "params": {
                "pane_id": pane_id,
                "text": format!("python3 -m http.server {port} --bind 127.0.0.1"),
                "keys": ["Enter"]
            }
        }),
    ));
    wait_for_http_contains(
        port,
        "hello-from-python-before-and-after",
        Duration::from_secs(10),
    );

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));
    wait_for_http_contains(
        port,
        "hello-from-python-before-and-after",
        Duration::from_secs(10),
    );

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    let _ = client_socket;
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_preserves_http_servers_across_multiple_sessions() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let sessions = [
        (None, config_home.join("flock-dev/flock.sock")),
        (
            Some("work"),
            config_home.join("flock-dev/sessions/work/flock.sock"),
        ),
    ];
    let mut spawned = Vec::new();
    let mut ports = Vec::new();

    for (session_name, api_socket) in &sessions {
        let web_root = base.join(format!("web-{}", session_name.unwrap_or("default")));
        fs::create_dir_all(&web_root).unwrap();
        fs::write(
            web_root.join("index.html"),
            format!("hello-from-{}", session_name.unwrap_or("default")),
        )
        .unwrap();
        let port = unused_local_port();
        let server = if let Some(session_name) = session_name {
            spawn_named_session_server(&config_home, &runtime_dir, session_name)
        } else {
            spawn_default_session_server(&config_home, &runtime_dir)
        };
        wait_for_socket(api_socket, Duration::from_secs(10));
        let created = request(
            api_socket,
            serde_json::json!({
                "id": "test:workspace:create",
                "method": "workspace.create",
                "params": {"cwd": web_root, "focus": true}
            }),
        );
        let pane_id = created["result"]["root_pane"]["pane_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_ok(request(
            api_socket,
            serde_json::json!({
                "id": "test:pane:run-python",
                "method": "pane.send_input",
                "params": {
                    "pane_id": pane_id,
                    "text": format!("python3 -m http.server {port} --bind 127.0.0.1"),
                    "keys": ["Enter"]
                }
            }),
        ));
        wait_for_http_contains(
            port,
            &format!("hello-from-{}", session_name.unwrap_or("default")),
            Duration::from_secs(10),
        );
        spawned.push(server);
        ports.push((port, session_name.unwrap_or("default").to_string()));
    }
    register_runtime_dir(&runtime_dir);

    for (_session_name, api_socket) in &sessions {
        assert_ok(request(
            api_socket,
            serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
        ));
    }
    drop(spawned);

    for (_session_name, api_socket) in &sessions {
        wait_for_api(api_socket, Duration::from_secs(10));
    }
    for (port, label) in ports {
        wait_for_http_contains(
            port,
            &format!("hello-from-{label}"),
            Duration::from_secs(10),
        );
    }

    for (_session_name, api_socket) in &sessions {
        let _ = request(
            api_socket,
            serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
        );
    }
    cleanup_test_base(&base);
}

#[test]
fn removed_config_keys_warn_on_cold_start_and_live_reload() {
    let _lock = test_lock();
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let config_home = base.join("config");
    let runtime = base.join("runtime");
    let api = runtime.join("flock.sock");
    let config = base.join("config.toml");
    fs::write(
        &config,
        "onboarding=false\n[msg]\nenabled=false\nallow_from=[]\nuplink_timeout_secs=1\n",
    )
    .unwrap();
    let spawned = spawn_server_with_env(
        &config_home,
        &runtime,
        &api,
        &[("FLOCK_CONFIG_PATH", config.to_str().unwrap())],
    );
    wait_for_socket(&api, Duration::from_secs(10));
    register_runtime_dir(&runtime);
    let created = request(
        &api,
        serde_json::json!({"id":"create", "method":"workspace.create", "params":{"cwd":base,"focus":true}}),
    );
    let pane = created["result"]["root_pane"]["pane_id"].as_str().unwrap();
    for reload in [false, true] {
        if reload {
            fs::write(&config, "onboarding=false\n[msg]\nenabled=false\nallow_from=[]\nuplink_heartbeat_secs=2\ndeferral_relay_concurrency=4\n").unwrap();
        }
        let report = request(
            &api,
            serde_json::json!({"id":"reload", "method":"server.reload_config", "params":{}}),
        );
        assert_eq!(report["result"]["status"], "partial", "{report}");
        assert!(report.to_string().contains("delete this line"), "{report}");
        let refused = request(
            &api,
            serde_json::json!({"id":"policy", "method":"msg.send", "params":{"to":{"type":"pane","pane":pane},"body":"still disabled"}}),
        );
        assert_eq!(refused["error"]["code"], "msg_not_allowed", "{refused}");
    }
    let log = fs::read_to_string(config_home.join("flock-dev/flock-server.log")).unwrap();
    assert!(log.contains("config.removed_key"), "{log}");
    assert_ok(request(
        &api,
        serde_json::json!({"id":"stop", "method":"server.stop", "params":{}}),
    ));
    drop(spawned);
    cleanup_test_base(&base);
}

#[test]
fn config_check_json_and_cli_warnings_name_source_keys() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime = base.join("runtime");
    let env = support::environment::isolated_env(&config_home, &runtime);
    let config = base.join("config.toml");
    fs::write(&config, "onboarding=false\n[msg]\nenabled=false\nallow_from=[]\nuplink_timeout_secs=1\nfuture_setting=true\n").unwrap();
    let run = |args: &[&str]| {
        let mut command = support::environment::Command::new(env!("CARGO_BIN_EXE_flk"));
        command
            .args(args)
            .env_remove("FLOCK_HOST_NAME")
            .env_remove("FLOCK_NAME")
            .envs(env.iter().cloned())
            .env("FLOCK_CONFIG_PATH", &config);
        support::environment::assert_command_isolated(&command);
        command.output().unwrap()
    };
    let checked = run(&["config", "check", "--json"]);
    assert_eq!(checked.status.code(), Some(1));
    let json: serde_json::Value = serde_json::from_slice(&checked.stdout).unwrap();
    assert_eq!(json["exit_code"], 1);
    assert_eq!(json["diagnostics"].as_array().unwrap().len(), 2, "{json}");
    assert!(
        json.to_string()
            .contains(&format!("{}:5: msg.uplink_timeout_secs", config.display())),
        "{json}"
    );
    assert!(
        json.to_string().contains(&format!(
            "{}:6: unknown msg.future_setting",
            config.display()
        )),
        "{json}"
    );
    let text = run(&["config", "check"]);
    assert_eq!(text.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&text.stderr).contains("delete this line"));
    let status = run(&["status", "--json"]);
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(
        json["config_warnings"].as_array().unwrap().len(),
        1,
        "{json}"
    );
    assert_eq!(
        String::from_utf8_lossy(&status.stderr)
            .matches("delete this line")
            .count(),
        0,
        "status carries the warning in its JSON, not again on stderr (#860)"
    );
    let plain = run(&["status"]);
    assert!(plain.status.success());
    let shown = format!(
        "{}{}",
        String::from_utf8_lossy(&plain.stdout),
        String::from_utf8_lossy(&plain.stderr)
    );
    assert_eq!(
        shown.matches("delete this line").count(),
        1,
        "plain status prints each warning once (#860): {shown}"
    );
    for scope in ["server", "client"] {
        let scoped = run(&["status", scope]);
        let shown = format!(
            "{}{}",
            String::from_utf8_lossy(&scoped.stdout),
            String::from_utf8_lossy(&scoped.stderr)
        );
        assert_eq!(
            shown.matches("delete this line").count(),
            1,
            "status {scope} does not print config warnings itself, so the CLI must (#860): {shown}"
        );
    }
    let version = run(&["--version"]);
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stderr)
            .matches("delete this line")
            .count(),
        1
    );
    fs::write(&config, "onboarding=false\n").unwrap();
    assert_eq!(run(&["config", "check", "--json"]).status.code(), Some(0));
    fs::write(&config, "[broken\n").unwrap();
    assert_eq!(run(&["config", "check", "--json"]).status.code(), Some(2));
    cleanup_test_base(&base);
}

#[test]
fn removed_config_key_during_handoff_succeeds_preserving_policy() {
    let _lock = test_lock();
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let config_home = base.join("config");
    let runtime = base.join("runtime");
    let api = runtime.join("flock.sock");
    let config = base.join("config.toml");
    fs::write(
        &config,
        "onboarding=false\n[msg]\nenabled=false\nallow_from=[]\n",
    )
    .unwrap();
    let spawned = spawn_server_with_env(
        &config_home,
        &runtime,
        &api,
        &[("FLOCK_CONFIG_PATH", config.to_str().unwrap())],
    );
    wait_for_socket(&api, Duration::from_secs(10));
    register_runtime_dir(&runtime);
    let created = request(
        &api,
        serde_json::json!({"id":"create", "method":"workspace.create", "params":{"cwd":base,"focus":true}}),
    );
    let pane = created["result"]["root_pane"]["pane_id"].as_str().unwrap();
    fs::write(
        &config,
        "onboarding=false\n[msg]\nenabled=false\nallow_from=[]\nuplink_timeout_secs=1\n",
    )
    .unwrap();
    assert_ok(request(
        &api,
        serde_json::json!({"id":"handoff", "method":"server.live_handoff", "params":{}}),
    ));
    wait_for_api(&api, Duration::from_secs(5));
    let marker = base.join("after-handoff");
    assert_ok(request(
        &api,
        serde_json::json!({"id":"after", "method":"pane.send_input", "params":{"pane_id":pane, "text":format!("printf survived > '{}'", marker.display()), "keys":["Enter"]}}),
    ));
    wait_for_file_contains(&marker, "survived", Duration::from_secs(5));
    let refused = request(
        &api,
        serde_json::json!({"id":"policy", "method":"msg.send", "params":{"to":{"type":"pane","pane":pane},"body":"still disabled"}}),
    );
    assert_eq!(refused["error"]["code"], "msg_not_allowed", "{refused}");
    assert_ok(request(
        &api,
        serde_json::json!({"id":"stop", "method":"server.stop", "params":{}}),
    ));
    drop(spawned);
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_bad_expected_protocol_rolls_back_old_server() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");
    let marker = base.join("child.pid");
    let received_marker = base.join("received");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let command = format!(
        "sh -c 'echo READY $$ > {}; while read line; do echo got:$line; echo got:$line >> {}; done'",
        marker.display(),
        received_marker.display()
    );
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:run",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": command, "keys": ["Enter"]}
        }),
    ));
    support::wait_for_file(&marker, Duration::from_secs(5));
    let pid_text = fs::read_to_string(&marker).unwrap();
    let child_pid: u32 = pid_text.split_whitespace().last().unwrap().parse().unwrap();

    let failed = request(
        &api_socket,
        serde_json::json!({
            "id": "test:bad-handoff",
            "method": "server.live_handoff",
            "params": {"expected_protocol": 999999}
        }),
    );
    assert!(
        failed.get("error").is_some(),
        "bad protocol handoff should fail: {failed}"
    );
    // #600: the exporter must surface WHY the import refused, not an opaque
    // "handoff stream closed while reading line". Both sides are post-fix in
    // this test, so the importer's `error: <reason>` round-trips into
    // `handoff import refused: <reason>` here.
    let error_message = failed
        .get("error")
        .and_then(|err| err.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or("");
    assert!(
        error_message.starts_with("handoff import refused:"),
        "exporter must surface the refusal reason, got: {error_message:?}"
    );
    assert!(
        error_message.contains("protocol"),
        "refusal reason must name the protocol mismatch, got: {error_message:?}"
    );
    wait_for_api(&api_socket, Duration::from_secs(5));
    assert_eq!(unsafe { libc::kill(child_pid as libc::pid_t, 0) }, 0);

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:send-after-failed-handoff",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": "after-failed-handoff", "keys": ["Enter"]}
        }),
    ));
    wait_for_file_contains(
        &received_marker,
        "got:after-failed-handoff",
        Duration::from_secs(5),
    );
    wait_for_output(&api_socket, &pane_id, "got:after-failed-handoff");

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    drop(spawned);
    cleanup_test_base(&base);
}

fn live_handoff_import_failure_rolls_back_old_server_at(failure_point: &str) {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");
    let client_socket = runtime_dir.join("flock-client.sock");
    let marker = base.join("child.pid");
    let received_marker = base.join("received");

    let store_failure = matches!(failure_point, "rollback_store" | "committed_store");
    let slow_open = matches!(failure_point, "slow_store" | "slow_store_rollback");
    let committed = failure_point == "committed_store" || slow_open;
    let fail_file = base.join("fail-store-open");
    let hook = match failure_point {
        "rollback_store" => "before_fds",
        "committed_store" | "slow_store" | "slow_store_rollback" => "",
        other => other,
    };
    let identity_path = runtime_dir.join("state/flock-dev/mesh/identity.json");
    let mut extra_env = vec![
        ("FLOCK_TEST_HANDOFF_IMPORT_FAIL", hook),
        (
            if slow_open {
                "FLOCK_TEST_MESH_OPEN_WAIT_FILE"
            } else {
                "FLOCK_TEST_MESH_OPEN_FAIL_FILE"
            },
            fail_file.to_str().unwrap(),
        ),
    ];
    let late_corruption = matches!(hook, "before_ready" | "before_commit" | "stale_generation");
    if late_corruption {
        extra_env.push((
            "FLOCK_TEST_HANDOFF_CORRUPT_IDENTITY_PATH",
            identity_path.to_str().unwrap(),
        ));
    }
    let spawned = spawn_server_with_env(&config_home, &runtime_dir, &api_socket, &extra_env);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let command = format!(
        "sh -c 'echo READY $$ > {}; while read line; do echo got:$line; echo got:$line >> {}; done'",
        marker.display(),
        received_marker.display()
    );
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:run",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": command, "keys": ["Enter"]}
        }),
    ));
    support::wait_for_file(&marker, Duration::from_secs(5));
    let pid_text = fs::read_to_string(&marker).unwrap();
    let child_pid: u32 = pid_text.split_whitespace().last().unwrap().parse().unwrap();

    let before = request(
        &api_socket,
        serde_json::json!({"id":"identity-before","method":"ping","params":{}}),
    );
    if matches!(hook, "after_restored" | "before_fds") {
        fs::write(&identity_path, b"corrupt during handoff").unwrap();
    }
    let identity_before = fs::read(&identity_path).unwrap();
    if store_failure || slow_open {
        fs::write(&fail_file, b"fail").unwrap();
    }
    let failed = request(
        &api_socket,
        serde_json::json!({"id":"test:handoff-fail","method":"server.live_handoff","params":{}}),
    );
    assert_eq!(failed.get("error").is_some(), !committed, "{failed}");
    if committed {
        wait_for_replacement_server_pid(
            &runtime_dir,
            spawned.child.process_id().unwrap(),
            Duration::from_secs(10),
        );
    }
    if failure_point == "rollback_store" {
        assert!(
            failed["error"]["message"]
                .as_str()
                .unwrap()
                .contains("test handoff failure before fd transfer"),
            "{failed}"
        );
    }
    if failure_point == "stale_generation" {
        assert!(
            failed["error"]["message"]
                .as_str()
                .unwrap()
                .contains("older than handoff generation"),
            "{failed}"
        );
    }
    wait_for_api(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(5));
    let after = request(
        &api_socket,
        serde_json::json!({"id":"identity-after","method":"ping","params":{}}),
    );
    assert_eq!(
        before["result"]["capabilities"],
        after["result"]["capabilities"]
    );
    let expected_identity = if late_corruption {
        b"corrupt during handoff".to_vec()
    } else {
        identity_before
    };
    assert_eq!(fs::read(identity_path).unwrap(), expected_identity);
    if store_failure {
        let status = request(
            &api_socket,
            serde_json::json!({"id":"mesh-status", "method":"peers.enrollment", "params":{}}),
        );
        assert_eq!(
            status["result"]["mesh_suspended_reason"],
            "injected mesh store open failure"
        );
        let output = support::environment::Command::new(env!("CARGO_BIN_EXE_flk"))
            .args(["status", "--json"])
            .env("FLOCK_SOCKET_PATH", &api_socket)
            .envs(support::environment::isolated_env(
                &config_home,
                &runtime_dir,
            ))
            .env("XDG_STATE_HOME", runtime_dir.join("state"))
            .output()
            .unwrap();
        assert!(output.status.success());
        let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(status["enrollment_warning"]
            .as_str()
            .unwrap()
            .contains("mesh: suspended: injected mesh store open failure"));
    } else if !slow_open {
        // API responsiveness precedes asynchronous custody recovery.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = request(
                &api_socket,
                serde_json::json!({
                    "id":"rollback-recovery", "method":"peers.enrollment", "params":{}
                }),
            );
            if status.get("error").is_none() && status["result"]["mesh_suspended_reason"].is_null()
            {
                break;
            }
            assert!(Instant::now() < deadline, "{status}");
            thread::sleep(Duration::from_millis(25));
        }
        // This API uses the shared writer, unlike ping or pane input.
        assert_ok(request(
            &api_socket,
            serde_json::json!({"id":"writer-after-rollback", "method":"msg.send", "params":{
                "to":{"type":"pane", "pane":pane_id}, "body":"writer recovered", "intent":"fyi"
            }}),
        ));
    }
    let database = rusqlite::Connection::open_with_flags(
        runtime_dir.join("state/flock-dev/mesh-mail.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let generation: i64 = database
        .query_row("SELECT generation FROM writer_generation", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(generation, 1);
    drop(database);
    assert_eq!(unsafe { libc::kill(child_pid as libc::pid_t, 0) }, 0);

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:send-after-import-failure",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": failure_point, "keys": ["Enter"]}
        }),
    ));
    wait_for_file_contains(
        &received_marker,
        &format!("got:{failure_point}"),
        Duration::from_secs(5),
    );

    wait_for_output(&api_socket, &pane_id, &format!("got:{failure_point}"));
    if store_failure || slow_open {
        if slow_open {
            let started = Instant::now();
            let status = request(
                &api_socket,
                serde_json::json!({"id":"slow-open-status", "method":"peers.enrollment", "params":{}}),
            );
            assert_eq!(
                status["result"]["mesh_suspended_reason"],
                "recovering store after handoff"
            );
            assert!(started.elapsed() < Duration::from_secs(1), "{status}");
            let output = support::environment::Command::new(env!("CARGO_BIN_EXE_flk"))
                .arg("status")
                .env("FLOCK_SOCKET_PATH", &api_socket)
                .envs(support::environment::isolated_env(
                    &config_home,
                    &runtime_dir,
                ))
                .env("XDG_STATE_HOME", runtime_dir.join("state"))
                .output()
                .unwrap();
            assert!(output.status.success());
            let text = String::from_utf8(output.stdout).unwrap();
            assert!(
                text.contains("mesh: recovering store after handoff"),
                "{text}"
            );
            assert!(!text.contains("retrying recovery"), "{text}");
            if failure_point == "slow_store_rollback" {
                support::wait_for_file(
                    &fail_file.with_extension("entered"),
                    Duration::from_secs(5),
                );
                let failed = request(
                    &api_socket,
                    serde_json::json!({
                        "id":"handoff-during-recovery", "method":"server.live_handoff", "params":{}
                    }),
                );
                assert!(
                    failed["error"]["message"]
                        .as_str()
                        .is_some_and(|reason| reason.contains("mesh store recovery in progress")),
                    "{failed}"
                );
                let started = Instant::now();
                assert_ok(request(
                    &api_socket,
                    serde_json::json!({
                        "id":"responsive-after-rollback", "method":"ping", "params":{}
                    }),
                ));
                assert!(started.elapsed() < Duration::from_secs(1));
                assert!(fail_file.exists(), "recovery must still be blocked");
                assert_ok(request(
                    &api_socket,
                    serde_json::json!({
                        "id":"input-after-rollback", "method":"pane.send_input",
                        "params":{"pane_id":pane_id, "text":"after-recovery-rollback", "keys":["Enter"]}
                    }),
                ));
                wait_for_file_contains(
                    &received_marker,
                    "got:after-recovery-rollback",
                    Duration::from_secs(5),
                );
            }
        }
        if failure_point == "slow_store" {
            // #887: mail waits for recovery only up to its bound, then is
            // refused and dropped, so a caller's retry cannot duplicate it.
            let started = Instant::now();
            let refused = request(
                &api_socket,
                serde_json::json!({"id":"send-during-recovery", "method":"msg.send", "params":{
                    "to":{"type":"pane", "pane":pane_id}, "body":"ghost",
                    "correlation_id":"ghost-during-recovery", "intent":"fyi"
                }}),
            );
            assert_eq!(
                refused["error"]["code"], "mail_store_unavailable",
                "{refused}"
            );
            assert!(started.elapsed() < Duration::from_secs(10), "{refused}");
            assert!(fail_file.exists(), "recovery must still be blocked");
        }
        fs::remove_file(&fail_file).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let status = request(
                &api_socket,
                serde_json::json!({"id":"mesh-recovered", "method":"peers.enrollment", "params":{}}),
            );
            if status["result"]["mesh_suspended_reason"].is_null() {
                break;
            }
            assert!(Instant::now() < deadline, "{status}");
            thread::sleep(Duration::from_millis(100));
        }
        assert_ok(request(
            &api_socket,
            serde_json::json!({"id":"recovered-send", "method":"msg.send", "params":{
                "to":{"type":"pane", "pane":pane_id}, "body":"recovered", "intent":"fyi"
            }}),
        ));
        if failure_point == "slow_store" {
            let read = request(
                &api_socket,
                serde_json::json!({"id":"read-after-recovery", "method":"msg.read", "params":{"pane":pane_id}}),
            );
            let bodies: Vec<_> = read["result"]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|message| message["body"].clone())
                .collect();
            assert_eq!(bodies, [serde_json::json!("recovered")], "{read}");
        }
    }
    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    drop(spawned);
    cleanup_test_base(&base);
}

#[test]
fn rollback_store_open_failure_preserves_panes_and_original_error() {
    live_handoff_import_failure_rolls_back_old_server_at("rollback_store");
}

#[test]
fn handoff_rollback_during_slow_recovery_keeps_the_loop_responsive() {
    live_handoff_import_failure_rolls_back_old_server_at("slow_store_rollback");
}

#[test]
fn post_handoff_recovery_does_not_block_the_app_loop() {
    live_handoff_import_failure_rolls_back_old_server_at("slow_store");
}

#[test]
fn recovery_failure_keeps_server_and_panes() {
    live_handoff_import_failure_rolls_back_old_server_at("committed_store");
}

#[test]
fn live_handoff_after_restored_failure_rolls_back_old_server() {
    live_handoff_import_failure_rolls_back_old_server_at("after_restored");
}

#[test]
fn live_handoff_refuses_stale_store_generation_and_reopens_writer() {
    live_handoff_import_failure_rolls_back_old_server_at("stale_generation");
}

#[test]
fn live_handoff_before_fds_failure_reopens_writer() {
    live_handoff_import_failure_rolls_back_old_server_at("before_fds");
}

#[test]
fn live_handoff_before_ready_failure_reopens_writer() {
    live_handoff_import_failure_rolls_back_old_server_at("before_ready");
}

#[test]
fn live_handoff_before_commit_failure_reopens_writer() {
    live_handoff_import_failure_rolls_back_old_server_at("before_commit");
}

#[test]
fn handoff_import_stalled_peer_exits_within_deadline() {
    let base = std::env::temp_dir().join(format!("hi363-{}", std::process::id()));
    fs::create_dir_all(&base).unwrap();
    let socket = base.join("import.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut command = support::environment::Command::new(env!("CARGO_BIN_EXE_flk"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("FLOCK_") {
            command.env_remove(key);
        }
    }
    command
        .args(["server", "--handoff-import"])
        .arg(&socket)
        .arg("stalled-peer-token")
        .envs(support::environment::isolated_env(
            &base.join("config"),
            &base.join("runtime"),
        ))
        .env("XDG_STATE_HOME", base.join("state"))
        .env("SHELL", "/bin/sh")
        .env("FLOCK_TEST_HANDOFF_IMPORT_TIMEOUT_MS", "5000")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    support::environment::assert_command_isolated(&command);
    let mut child = command.spawn().unwrap();
    register_spawned_flock_pid(Some(child.id()));
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut peer = None;
    let status = loop {
        if peer.is_none() {
            match listener.accept() {
                Ok((stream, _)) => peer = Some(stream),
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(err) => panic!("accept importer: {err}"),
            }
        }
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("importer exceeded startup deadline");
        }
        thread::sleep(Duration::from_millis(25));
    };
    unregister_spawned_flock_pid(Some(child.id()));
    assert!(peer.is_some(), "importer must reach the stalled peer");
    assert_eq!(status.code(), Some(124));
    drop(peer);
    drop(listener);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn handoff_ready_importer_survives_commit_after_startup_deadline() {
    let base = std::env::temp_dir().join(format!("hc363-{}", std::process::id()));
    fs::create_dir_all(&base).unwrap();
    let socket = base.join("import.sock");
    let api_socket = base.join("api.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut command = support::environment::Command::new(env!("CARGO_BIN_EXE_flk"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("FLOCK_") {
            command.env_remove(key);
        }
    }
    command
        .args(["server", "--handoff-import"])
        .arg(&socket)
        .arg("late-commit-token")
        .envs(support::environment::isolated_env(
            &base.join("config"),
            &base.join("runtime"),
        ))
        .env("XDG_STATE_HOME", base.join("state"))
        .env("SHELL", "/bin/sh")
        .env("FLOCK_SOCKET_PATH", &api_socket)
        .env("FLOCK_CLIENT_SOCKET_PATH", base.join("client.sock"))
        .env("FLOCK_TEST_HANDOFF_IMPORT_TIMEOUT_MS", "5000")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    support::environment::assert_command_isolated(&command);
    let mut child = command.spawn().unwrap();
    register_spawned_flock_pid(Some(child.id()));
    let started = Instant::now();
    let peer = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(err) => panic!("accept importer: {err}"),
        }
        assert!(started.elapsed() < Duration::from_secs(10));
        thread::sleep(Duration::from_millis(25));
    };
    peer.set_nonblocking(false).unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    peer.set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut peer = BufReader::new(peer);
    let mut line = String::new();
    peer.read_line(&mut line).unwrap();
    assert_eq!(line.trim_end(), "late-commit-token");
    let manifest = serde_json::json!({
        "version": 1, "source_version": "test", "source_protocol": 0,
        "expected_version": null, "expected_protocol": null,
        "snapshot": {"version": 3, "workspaces": [], "active": null, "selected": 0},
        "panes": []
    });
    writeln!(peer.get_mut(), "{manifest}").unwrap();
    for expected in ["validated", "restored", "ready"] {
        line.clear();
        peer.read_line(&mut line).unwrap();
        assert_eq!(line.trim_end(), expected);
    }
    // No custody writer or schema migration is allowed before commit.
    assert!(!base.join("state/flock-dev/mesh-mail.sqlite").exists());
    // The same clock would have killed the previous importer before commit.
    thread::sleep(Duration::from_secs(6));
    assert!(child.try_wait().unwrap().is_none());
    peer.get_mut().write_all(b"committed\n").unwrap();
    line.clear();
    peer.read_line(&mut line).unwrap();
    assert_eq!(line.trim_end(), "owned");
    let response = request(
        &api_socket,
        serde_json::json!({"id":"late-commit-stop", "method":"server.stop", "params":{}}),
    );
    assert!(response.get("error").is_none(), "{response}");
    child.kill().ok();
    child.wait().unwrap();
    unregister_spawned_flock_pid(Some(child.id()));
    drop(peer);
    drop(listener);
    cleanup_test_base(&base);
}
