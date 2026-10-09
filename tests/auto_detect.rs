//! Integration tests for auto-detect launch behavior.

// This file used to open with `#![cfg(not(target_os = "macos"))]`, compiling all
// 10 tests out of every Mac build. The guard was collateral from e97413d rather
// than a judgement about this platform, and the harness already bound under a
// short `/tmp` path. Nothing here needed accommodating, so the gate simply went;
// see #269 and the note on `tests/cli_wrapper.rs`.
//
// TracedCommand (logging redesign PR-3) polices flock's shipped code; this
// harness drives the compiled flock binary through raw Command.
#![allow(clippy::disallowed_methods)]

mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde_json::Value;
use support::{
    cleanup_test_base, register_runtime_dir, register_spawned_flock_pid,
    unregister_spawned_flock_pid,
};

fn unique_test_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!(
        "/tmp/flock-autodetect-test-{}-{nanos}",
        std::process::id()
    ))
}

struct SpawnedFlock {
    _master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl Drop for SpawnedFlock {
    fn drop(&mut self) {
        let pid = self.child.process_id();
        let _ = self.child.kill();

        if let Some(pid) = pid {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                let mut status = 0;
                let result =
                    unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
                if result == pid as libc::pid_t || result == -1 {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }

            unregister_spawned_flock_pid(Some(pid));
        }
    }
}

fn cleanup_spawned_flock(spawned: SpawnedFlock, base: PathBuf) {
    drop(spawned);
    cleanup_test_base(&base);
}

fn wait_for_socket(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() && UnixStream::connect(path).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("socket did not appear at {}", path.display());
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

fn spawn_server(
    config_home: &Path,
    runtime_dir: &Path,
    api_socket_path: &Path,
    _client_socket_path: &Path,
) -> SpawnedFlock {
    fs::create_dir_all(config_home.join("flock")).unwrap();
    fs::create_dir_all(runtime_dir).unwrap();
    register_runtime_dir(runtime_dir);
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
    cmd.env("XDG_CONFIG_HOME", config_home);
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env("FLOCK_SOCKET_PATH", api_socket_path);
    cmd.env_remove("FLOCK_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("FLOCK_ENV");

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_flock_pid(child.process_id());
    drop(pair.slave);

    SpawnedFlock {
        _master: pair.master,
        child,
    }
}

/// Spawn `flock` (no subcommand) — the auto-detect launch path.
fn spawn_flock_auto(
    config_home: &Path,
    runtime_dir: &Path,
    api_socket_path: &Path,
    _client_socket_path: &Path,
) -> SpawnedFlock {
    fs::create_dir_all(config_home.join("flock")).unwrap();
    fs::create_dir_all(runtime_dir).unwrap();
    register_runtime_dir(runtime_dir);
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
    // No subcommand, no --no-session → auto-detect launch
    cmd.env("XDG_CONFIG_HOME", config_home);
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env("FLOCK_SOCKET_PATH", api_socket_path);
    cmd.env_remove("FLOCK_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("FLOCK_ENV");

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_flock_pid(child.process_id());
    drop(pair.slave);

    SpawnedFlock {
        _master: pair.master,
        child,
    }
}

/// Spawn `flock --no-session` — the monolithic escape hatch.
fn spawn_flock_no_session(
    config_home: &Path,
    runtime_dir: &Path,
    api_socket_path: &Path,
) -> SpawnedFlock {
    fs::create_dir_all(config_home.join("flock")).unwrap();
    fs::create_dir_all(runtime_dir).unwrap();
    register_runtime_dir(runtime_dir);
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
    cmd.arg("--no-session");
    cmd.env("XDG_CONFIG_HOME", config_home);
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env("FLOCK_SOCKET_PATH", api_socket_path);
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("FLOCK_ENV");

    let home = config_home.join("home");
    fs::create_dir_all(&home).unwrap();
    cmd.env("HOME", &home);
    cmd.env("XDG_STATE_HOME", home.join("state"));
    cmd.env("XDG_DATA_HOME", home.join("data"));
    cmd.env("XDG_CACHE_HOME", home.join("cache"));
    cmd.env_remove("FLOCK_AGENT_ID");
    cmd.env_remove("FLOCK_SESSION");
    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_flock_pid(child.process_id());
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().unwrap();
    thread::spawn(move || {
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
    });
    SpawnedFlock {
        _master: pair.master,
        child,
    }
}

fn ping_socket(socket_path: &Path) -> String {
    let mut stream = UnixStream::connect(socket_path).expect("should connect to API socket");

    let request = r#"{"id":"1","method":"ping","params":{}}"#;
    writeln!(stream, "{}", request).unwrap();

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response).unwrap();
    response.trim().to_string()
}

fn wait_for_log_contains(path: &Path, needle: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(content) = fs::read_to_string(path) {
            if content.contains(needle) {
                return;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    let content = fs::read_to_string(path).unwrap_or_default();
    panic!(
        "log {} did not contain {:?}. content:\n{}",
        path.display(),
        needle,
        content
    );
}

fn run_cli(socket_path: &Path, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_flk"));
    command.args(args);
    command.env("FLOCK_SOCKET_PATH", socket_path);
    command.output().unwrap()
}

fn process_exists(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    if result == 0 {
        true
    } else {
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

fn read_json_line(stream: UnixStream) -> Value {
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response).unwrap();
    serde_json::from_str(&response).unwrap()
}

fn wait_for_pid_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_exists(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// #521: the daemon auto-detect spawns is reaped, even though nothing in this
/// harness ever learns its pid.
///
/// The four auto-detect tests that exercise this launch path all leak it, and
/// before #521 that cost four processes per `just check` run while failing
/// nothing. The product spawns the daemon detached, in its own process group,
/// with stdio on `/dev/null`, and `SpawnedFlock::drop` kills only the client pid
/// — so it survives as PPID 1 still writing `flock-server.log`. None of the
/// three existing reap paths can see it: the pid registry never heard of it,
/// `pgrep -f <base>` cannot match a bare `flk server` argv, and the runtime-dir
/// sweep used to walk `/proc`, which does not exist on Darwin. nextest cannot
/// see it either — it only catches descendants holding the test's output
/// pipes, and these hold none.
///
/// So this asserts the leak directly: find the daemon by its runtime dir while
/// it is still running, then assert the sweep in `cleanup_test_base` killed it.
/// The daemon carries `XDG_RUNTIME_DIR` and `FLOCK_SOCKET_PATH` inherited from
/// the client, which is the only thing connecting a process this harness never
/// spawned back to the test that owns it.
#[test]
fn auto_detect_daemon_is_reaped_by_the_runtime_dir_sweep() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");
    let client_socket = runtime_dir.join("flock-client.sock");

    let flock = spawn_flock_auto(&config_home, &runtime_dir, &api_socket, &client_socket);
    let client_pid = flock.child.process_id().expect("client should have PID");

    wait_for_socket(&api_socket, Duration::from_secs(10));

    // Finding the daemon is itself the capability under test: on macOS the
    // sweep used to enumerate nothing, so nothing here could ever have reaped
    // it. Polled rather than read once, because the daemon is a freshly exec'd
    // process on a machine running twenty test binaries, and a sweep is a
    // snapshot.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut daemon_pids: Vec<u32> = Vec::new();
    while Instant::now() < deadline {
        daemon_pids = support::flock_server_pids_for_runtime_dir(&runtime_dir)
            .unwrap_or_default()
            .into_iter()
            .filter(|pid| *pid != client_pid)
            .collect();
        if !daemon_pids.is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !daemon_pids.is_empty(),
        "the auto-detect daemon should be visible to the reap sweep while it runs, \
         otherwise nothing here can ever reap it"
    );
    assert!(
        daemon_pids.iter().all(|pid| process_exists(*pid)),
        "the daemon must still be running when the sweep looks for it, or this \
         test proves nothing"
    );

    cleanup_spawned_flock(flock, base);

    // `terminate_pid` escalates SIGTERM to SIGKILL and then waits, but a
    // reaped-process check can still lag the signal by a moment on a loaded
    // machine, so this polls rather than reads once.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let survivors: Vec<u32> = daemon_pids
            .iter()
            .copied()
            .filter(|pid| process_exists(*pid))
            .collect();
        if survivors.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "auto-detect daemons survived cleanup: {survivors:?}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

/// Running `flock` with no server present starts a server
/// and attaches as client.
#[test]
fn auto_detect_no_server_spawns_server_and_attaches() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");
    let client_socket = runtime_dir.join("flock-client.sock");

    // Ensure no server is running initially.
    assert!(
        !api_socket.exists(),
        "api socket should not exist initially"
    );
    assert!(
        !client_socket.exists(),
        "client socket should not exist initially"
    );

    // Run `flock` (no subcommand) — should auto-detect, spawn server, attach as client.
    let flock = spawn_flock_auto(&config_home, &runtime_dir, &api_socket, &client_socket);

    // Wait for both sockets to appear (server was spawned).
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(10));

    // Verify the API socket responds to ping (server is running).
    let response = ping_socket(&api_socket);
    assert!(
        response.contains("pong"),
        "API socket should respond to ping: {response}"
    );

    // Verify the client socket accepts connections (server is listening on it).
    let _stream = UnixStream::connect(&client_socket)
        .expect("should connect to client socket (server is listening)");

    // Verify the client process is running.
    let client_pid = flock.child.process_id().expect("client should have PID");
    assert!(
        process_exists(client_pid),
        "client process should be running"
    );

    cleanup_spawned_flock(flock, base);
}

/// Running `flock` with a server already running attaches
/// as client directly (no second server).
#[test]
fn auto_detect_server_running_attaches_directly() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");
    let client_socket = runtime_dir.join("flock-client.sock");

    // Start a server explicitly.
    let server = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(10));

    let server_pid = server.child.process_id().expect("server should have PID");

    // Verify server is running.
    assert!(process_exists(server_pid), "server should be running");

    // Run `flock` (no subcommand) — should detect the running server and attach.
    let client = spawn_flock_auto(&config_home, &runtime_dir, &api_socket, &client_socket);

    // Wait a moment for the client to attach.
    thread::sleep(Duration::from_millis(500));

    // Verify the client is running.
    let client_pid = client.child.process_id().expect("client should have PID");
    assert!(
        process_exists(client_pid),
        "client process should be running"
    );

    // Verify the server is still the same one (no second server spawned).
    assert!(
        process_exists(server_pid),
        "original server should still be running"
    );

    // Verify API still responds.
    let response = ping_socket(&api_socket);
    assert!(
        response.contains("pong"),
        "API should still respond to ping: {response}"
    );

    cleanup_spawned_flock(client, PathBuf::from("/nonexistent"));
    cleanup_spawned_flock(server, base);
}

/// Socket path resolution is consistent between server and client.
/// Both derive the client socket from the `FLOCK_SOCKET_PATH` override,
/// so overriding the API socket keeps both endpoints aligned.
#[test]
fn auto_detect_socket_path_consistency() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");
    let client_socket = runtime_dir.join("flock-client.sock");

    // Run `flock` with custom socket paths.
    let flock = spawn_flock_auto(&config_home, &runtime_dir, &api_socket, &client_socket);

    // Wait for both sockets to appear at the custom paths.
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(10));

    // Verify sockets exist at the specified paths.
    assert!(
        api_socket.exists(),
        "API socket should exist at custom path"
    );
    assert!(
        client_socket.exists(),
        "client socket should exist at custom path"
    );

    // Verify API responds (server is using the custom API socket path).
    let response = ping_socket(&api_socket);
    assert!(
        response.contains("pong"),
        "API should respond at custom path: {response}"
    );

    // Verify client socket accepts connections (server is using the custom
    // client socket path).
    let _stream = UnixStream::connect(&client_socket)
        .expect("should connect to client socket at custom path");

    cleanup_spawned_flock(flock, base);
}

/// `flock --no-session` bypasses server/client and runs
/// monolithically. No server process is spawned. No client socket is created.
#[test]
fn no_session_flag_runs_monolithically() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");
    let client_socket = runtime_dir.join("flock-client.sock");

    // Run `flock --no-session` — monolithic mode, no server/client.
    let flock = spawn_flock_no_session(&config_home, &runtime_dir, &api_socket);

    // Wait for the API socket (monolithic mode creates it).
    wait_for_socket(&api_socket, Duration::from_secs(10));

    // Verify the API socket exists and responds.
    let response = ping_socket(&api_socket);
    assert!(
        response.contains("pong"),
        "monolithic API should respond: {response}"
    );

    let rpc = |method: &str, params: Value| -> Value {
        let mut stream = UnixStream::connect(&api_socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        writeln!(
            stream,
            "{}",
            serde_json::json!({"id":"mail", "method":method, "params":params})
        )
        .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };
    let started = rpc(
        "agent.start",
        serde_json::json!({"name":"mailbox", "argv":["/bin/sh","-c","while read line; do :; done"], "cwd":config_home}),
    );
    let pane = &started["result"]["agent"]["pane_id"];
    assert!(pane.is_string(), "{started}");
    let sent = rpc(
        "msg.send",
        serde_json::json!({"to":{"type":"pane", "pane":pane},"body":"monolithic custody", "intent":"fyi"}),
    );
    assert!(sent["result"]["message_key"].is_object(), "{sent}");
    let inbox = rpc("msg.read", serde_json::json!({"pane":pane}));
    assert_eq!(
        inbox["result"]["messages"][0]["body"], "monolithic custody",
        "{inbox}"
    );

    // Verify NO client socket was created — this is the key distinction
    // between monolithic mode and server/client mode.
    assert!(
        !client_socket.exists(),
        "no client socket should exist in monolithic mode"
    );

    // Verify the API socket is served by the monolithic process itself,
    // not by a separate server. We can check this by verifying the client
    // PID matches what would be serving the socket — in monolithic mode,
    // there is only one flock process.
    let client_pid = flock.child.process_id().expect("should have PID");
    assert!(
        process_exists(client_pid),
        "monolithic process should be running"
    );

    cleanup_spawned_flock(flock, base);
}

/// CLI subcommands work through the server's JSON API socket.
#[test]
fn cli_subcommands_work_through_server() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");
    let client_socket = runtime_dir.join("flock-client.sock");

    // Start a server.
    let server = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(10));

    // Test `flock workspace list` through the server's API socket.
    let output = run_cli(&api_socket, &["workspace", "list"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "workspace list should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    // The response should be valid JSON with a "result" field.
    assert!(
        stdout.contains("result"),
        "workspace list output should contain 'result': {stdout}"
    );

    // Test `flk pane list` through the server's API socket.
    let output = run_cli(&api_socket, &["pane", "list"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "pane list should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("result"),
        "pane list output should contain 'result': {stdout}"
    );

    cleanup_spawned_flock(server, base);
}

/// Verify that the server spawned by auto-detect
/// persists after the client exits, and a new `flock` can reattach.
#[test]
fn auto_detect_server_persists_and_reattaches() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");
    let client_socket = runtime_dir.join("flock-client.sock");

    // Run `flock` — auto-detect spawns server + attaches client.
    let mut client1 = spawn_flock_auto(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(10));

    // Verify API responds.
    let response = ping_socket(&api_socket);
    assert!(
        response.contains("pong"),
        "API should respond before client exit: {response}"
    );

    // Kill the first client.
    let client1_pid = client1.child.process_id().expect("client1 should have PID");
    let _ = client1.child.kill();
    let _ = wait_for_pid_exit(client1_pid, Duration::from_secs(2));
    drop(client1);

    // Wait a moment for the server to process the disconnect.
    thread::sleep(Duration::from_millis(500));

    // Verify server is still running after client exit — the API should
    // still respond because the server is a separate daemon process.
    let response = ping_socket(&api_socket);
    assert!(
        response.contains("pong"),
        "API should still respond after client exit (server persists): {response}"
    );

    // The client socket should still exist (server is still listening).
    assert!(
        client_socket.exists(),
        "client socket should still exist after client exit"
    );

    // Run `flock` again — should detect the running server and reattach.
    let client2 = spawn_flock_auto(&config_home, &runtime_dir, &api_socket, &client_socket);
    thread::sleep(Duration::from_millis(500));

    // Verify the new client is running.
    let client2_pid = client2.child.process_id().expect("client2 should have PID");
    assert!(
        process_exists(client2_pid),
        "second client should be running"
    );

    // Verify API still responds (same server).
    let response = ping_socket(&api_socket);
    assert!(
        response.contains("pong"),
        "API should still respond after reattach: {response}"
    );

    cleanup_spawned_flock(client2, PathBuf::from("/nonexistent"));
    cleanup_test_base(&base);
}

/// Verify that the default API and client
/// sockets live in the app config directory when no env override is set.
#[test]
fn auto_detect_default_socket_path_from_config_dir() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");

    // Don't set FLOCK_SOCKET_PATH or FLOCK_CLIENT_SOCKET_PATH.
    // The default paths should come from the app config directory, not XDG_RUNTIME_DIR.
    let app_dir_name = if cfg!(debug_assertions) {
        "flock-dev"
    } else {
        "flock"
    };
    let api_socket = config_home.join(app_dir_name).join("flock.sock");
    let client_socket = config_home.join(app_dir_name).join("flock-client.sock");

    // Spawn server with XDG_RUNTIME_DIR set to a different directory to prove it is ignored.
    fs::create_dir_all(config_home.join(app_dir_name)).unwrap();
    fs::create_dir_all(&runtime_dir).unwrap();
    register_runtime_dir(&runtime_dir);
    fs::write(
        config_home.join(app_dir_name).join("config.toml"),
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
    cmd.env("XDG_CONFIG_HOME", &config_home);
    cmd.env("XDG_RUNTIME_DIR", &runtime_dir);
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("FLOCK_ENV");
    // Explicitly remove socket overrides to test default path resolution.
    cmd.env_remove("FLOCK_SOCKET_PATH");
    cmd.env_remove("FLOCK_CLIENT_SOCKET_PATH");

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_flock_pid(child.process_id());
    drop(pair.slave);
    let server = SpawnedFlock {
        _master: pair.master,
        child,
    };

    // Wait for sockets to appear at the default config-dir paths.
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(10));

    // Verify both sockets exist.
    assert!(api_socket.exists(), "API socket should exist in config dir");
    assert!(
        client_socket.exists(),
        "client socket should exist in config dir"
    );

    // Verify API responds.
    let response = ping_socket(&api_socket);
    assert!(
        response.contains("pong"),
        "API should respond at config-dir path: {response}"
    );

    cleanup_spawned_flock(server, base);
}

#[test]
fn auto_detect_writes_client_and_server_logs_to_separate_files() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");
    let client_socket = runtime_dir.join("flock-client.sock");

    let spawned = spawn_flock_auto(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(10));

    let app_dir_name = if cfg!(debug_assertions) {
        "flock-dev"
    } else {
        "flock"
    };
    let log_dir = config_home.join(app_dir_name);
    let client_log = log_dir.join("flock-client.log");
    let server_log = log_dir.join("flock-server.log");
    let monolith_log = log_dir.join("flock.log");

    wait_for_log_contains(
        &client_log,
        "\"event\":\"app.startup\"",
        Duration::from_secs(10),
    );
    assert!(
        fs::read_to_string(&client_log)
            .unwrap_or_default()
            .contains("\"subsystem\":\"client\""),
        "client startup record must be attributed to the client subsystem"
    );
    wait_for_log_contains(
        &server_log,
        "\"event\":\"app.startup\"",
        Duration::from_secs(10),
    );
    assert!(
        fs::read_to_string(&server_log)
            .unwrap_or_default()
            .contains("\"subsystem\":\"server\""),
        "server startup record must be attributed to the server subsystem"
    );

    let monolith_content = fs::read_to_string(&monolith_log).unwrap_or_default();
    assert!(
        !monolith_content.contains("\"subsystem\":\"client\""),
        "persistent client logs should not land in flock.log: {monolith_content}"
    );

    cleanup_spawned_flock(spawned, base);
}

#[test]
fn no_session_writes_startup_logs_to_monolith_file() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");

    let spawned = spawn_flock_no_session(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));

    let app_dir_name = if cfg!(debug_assertions) {
        "flock-dev"
    } else {
        "flock"
    };
    let log_dir = config_home.join(app_dir_name);
    let monolith_log = log_dir.join("flock.log");

    wait_for_log_contains(
        &monolith_log,
        "\"event\":\"app.startup\"",
        Duration::from_secs(10),
    );
    assert!(
        fs::read_to_string(&monolith_log)
            .unwrap_or_default()
            .contains("\"subsystem\":\"app\""),
        "monolith startup record must be attributed to the app subsystem"
    );

    cleanup_spawned_flock(spawned, base);
}

#[test]
fn auto_detect_respects_nested_guard_before_auto_attach() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");
    let client_socket = runtime_dir.join("flock-client.sock");

    let server = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(10));

    let baseline = read_json_line({
        let mut stream = UnixStream::connect(&api_socket).unwrap();
        writeln!(
            stream,
            r#"{{"id":"ws_before","method":"workspace.list","params":{{}}}}"#
        )
        .unwrap();
        stream
    });
    let baseline_count = baseline["result"]["workspaces"]
        .as_array()
        .map(|workspaces| workspaces.len())
        .unwrap_or(0);

    let output = Command::new(env!("CARGO_BIN_EXE_flk"))
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("FLOCK_SOCKET_PATH", &api_socket)
        .env_remove("FLOCK_CLIENT_SOCKET_PATH")
        .env("FLOCK_ENV", "1")
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "nested launch should fail before auto-attach"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("nested flock is disabled by default"),
        "stderr should mention nested-launch guard: {stderr}"
    );

    let after = read_json_line({
        let mut stream = UnixStream::connect(&api_socket).unwrap();
        writeln!(
            stream,
            r#"{{"id":"ws_after","method":"workspace.list","params":{{}}}}"#
        )
        .unwrap();
        stream
    });
    let after_count = after["result"]["workspaces"]
        .as_array()
        .map(|workspaces| workspaces.len())
        .unwrap_or(0);
    assert_eq!(
        after_count, baseline_count,
        "nested launch should not auto-attach or mutate server state"
    );

    cleanup_spawned_flock(server, base);
}
