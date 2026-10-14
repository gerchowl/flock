// TracedCommand (logging redesign PR-3) polices shipped code; this harness
// exec's raw lsof/pgrep to inspect the running system under test.
#![allow(clippy::disallowed_methods)]

mod support;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use support::{
    cleanup_test_base, client_handshake, register_runtime_dir, register_spawned_flock_pid,
    send_input, unregister_spawned_flock_pid, wait_for_disconnect,
};

struct SpawnedFlock {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

struct RequestError {
    retryable: bool,
    message: String,
}

impl Drop for SpawnedFlock {
    fn drop(&mut self) {
        let _timing = support::timing::phase(support::timing::Phase::Teardown);
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
/// `unique_test_dir()` (a short per-binary prefix plus OS entropy), and the
/// spawn helpers redirect `XDG_CONFIG_HOME` and
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

/// Atomically reserve a short random path, independent of PID reuse after a kill.
/// Entropy or creation errors fail loudly. Abandoned directories remain untouched.
fn unique_test_dir() -> PathBuf {
    let mut random = [0; 8];
    getrandom::fill(&mut random).expect("test directory entropy");
    let path = PathBuf::from(format!("/tmp/hlh-{:016x}", u64::from_ne_bytes(random)));
    fs::create_dir(&path).expect("reserve isolated handoff directory");
    path
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
    spawn_server_exe(
        Path::new(env!("CARGO_BIN_EXE_flk")),
        config_home,
        runtime_dir,
        api_socket,
        extra_env,
    )
}

/// Normal starts announce readiness after app and both listener initialization.
/// EOF/mismatches fail in the shared reader, and nextest bounds silent children.
fn spawn_server_exe(
    exe: &Path,
    config_home: &Path,
    runtime_dir: &Path,
    api_socket: &Path,
    extra_env: &[(&str, &str)],
) -> SpawnedFlock {
    let _timing = support::timing::phase(support::timing::Phase::Spawn);
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
    let mut cmd = CommandBuilder::new(exe);
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
    drop(pair.slave);
    let spawned = SpawnedFlock {
        master: pair.master,
        child,
    };
    support::readiness::wait_for_server(spawned.master.as_ref(), api_socket);
    spawned
}

fn spawn_named_session_server(
    config_home: &Path,
    runtime_dir: &Path,
    session_name: &str,
) -> SpawnedFlock {
    let _timing = support::timing::phase(support::timing::Phase::Spawn);
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
    drop(pair.slave);
    let spawned = SpawnedFlock {
        master: pair.master,
        child,
    };
    support::readiness::wait_for_server(
        spawned.master.as_ref(),
        &config_home
            .join("flock-dev/sessions")
            .join(session_name)
            .join("flock.sock"),
    );
    spawned
}

fn spawn_default_session_server(config_home: &Path, runtime_dir: &Path) -> SpawnedFlock {
    let _timing = support::timing::phase(support::timing::Phase::Spawn);
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
    drop(pair.slave);
    let spawned = SpawnedFlock {
        master: pair.master,
        child,
    };
    support::readiness::wait_for_server(
        spawned.master.as_ref(),
        &config_home.join("flock-dev/flock.sock"),
    );
    spawned
}

fn try_request(
    socket_path: &Path,
    request: serde_json::Value,
) -> Result<serde_json::Value, RequestError> {
    try_request_bounded(socket_path, request, None)
}

/// Bound reads and writes when checking that recovery leaves the loop usable.
/// Handoff calls elsewhere keep their existing protocol/runner failure ceilings.
fn try_request_bounded(
    socket_path: &Path,
    request: serde_json::Value,
    ceiling: Option<Duration>,
) -> Result<serde_json::Value, RequestError> {
    let mut stream = UnixStream::connect(socket_path).map_err(|err| RequestError {
        retryable: true,
        message: format!("connect {}: {err}", socket_path.display()),
    })?;
    stream
        .set_read_timeout(ceiling)
        .expect("API read failure ceiling");
    stream
        .set_write_timeout(ceiling)
        .expect("API write failure ceiling");
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

/// An alive API peer that never replies must fail inside the socket ceiling.
#[test]
fn recovery_request_fails_when_peer_stays_alive_and_silent() {
    let base = unique_test_dir();
    let socket = base.join("silent.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let (release, gate) = std::sync::mpsc::channel();
    let peer = thread::spawn(move || {
        let (_stream, _) = listener.accept().unwrap();
        gate.recv_timeout(Duration::from_secs(10)).unwrap();
    });
    let result = try_request_bounded(
        &socket,
        serde_json::json!({"id":"silent", "method":"ping", "params":{}}),
        Some(Duration::from_millis(50)),
    );
    release.send(()).unwrap();
    peer.join().unwrap();
    cleanup_test_base(&base);
    let error = result.expect_err("silent API peer must fail");
    assert!(error.message.contains("read response"), "{}", error.message);
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

/// Verify handoff/rollback API usability after the wire handshake completes.
/// Importer output is /dev/null (src/server/handoff.rs::spawn_handoff_import),
/// so replacement readiness is signalled over READY/OWNED, not the PTY banner.
/// Keep these bounded checks and the independent replacement PID assertions.
fn wait_for_api(socket_path: &Path, timeout: Duration) {
    let _timing = support::timing::wait("ready: api");
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
fn wait_for_replacement_server_pid(
    runtime_dir: &Path,
    _config_home: &Path,
    old_pid: u32,
    timeout: Duration,
) -> u32 {
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
fn wait_for_replacement_server_pid(
    _runtime_dir: &Path,
    config_home: &Path,
    old_pid: u32,
    timeout: Duration,
) -> u32 {
    wait_for_handoff_importer_pid(config_home, old_pid, timeout)
}

/// The importer the server `old_pid` spawned in the sandbox under
/// `config_home`, found by the handoff socket on its command line. Unlike the
/// runtime-dir sweep this also finds an importer running a binary other than
/// the test build, such as a store copy. Scoped to the sandbox so a reused pid
/// never selects an importer outside it (#1261).
fn wait_for_handoff_importer_pid(config_home: &Path, old_pid: u32, timeout: Duration) -> u32 {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(pid) = support::handoff_importer_pids(config_home, old_pid).first() {
            return adopt_replacement_server(*pid);
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "handoff importer for server {old_pid} did not appear under {}",
        config_home.display()
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
/// #1261: the importer lookup sees only importers inside this test's sandbox.
///
/// Pids are reused, so the server pid in a handoff socket's name is no proof of
/// ownership: an operator's live importer can carry the same name. Two decoys
/// carry `flock-handoff-<pid>.sock` with `--handoff-import`, one outside the
/// sandbox and one with the name alone, and neither may be selected. A third,
/// inside the sandbox, proves the lookup still finds a real match.
#[test]
fn handoff_importer_lookup_ignores_importers_outside_the_sandbox() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let elsewhere = base.join("elsewhere");
    let old_pid = 60058;
    let socket_name = format!("flock-handoff-{old_pid}.sock");
    // Each decoy leads its own process group, so killing the group also takes
    // the `sleep` the shell forked and nothing outlives the test.
    let decoy = |args: Vec<String>| {
        use std::os::unix::process::CommandExt as _;
        std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30; exit 0", "decoy"])
            .args(args)
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    };
    let outside = elsewhere.join("flock-dev").join(&socket_name);
    let inside = config_home.join("flock-dev").join(&socket_name);
    let mut decoys = vec![
        decoy(vec![
            "server".into(),
            "--handoff-import".into(),
            outside.display().to_string(),
            "token".into(),
        ]),
        decoy(vec![inside.display().to_string()]),
    ];
    let outside_pids: Vec<u32> = decoys.iter().map(|child| child.id()).collect();
    let real = decoy(vec![
        "server".into(),
        "--handoff-import".into(),
        inside.display().to_string(),
        "token".into(),
    ]);

    let real_pid = real.id();
    decoys.push(real);

    // Every decoy's argv must be readable before the lookup runs, or a decoy
    // that was not selected might only not have exec'd yet.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !decoys.iter().all(|child| {
        support::process_table::process_info(child.id())
            .is_ok_and(|info| info.argv.iter().any(|arg| arg == "decoy"))
    }) {
        assert!(Instant::now() < deadline, "a decoy never started");
        thread::sleep(Duration::from_millis(25));
    }
    let selected = support::handoff_importer_pids(&config_home, old_pid);
    for child in &mut decoys {
        unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
        let _ = child.wait();
    }
    let _ = fs::remove_dir_all(&base);

    assert_eq!(selected, vec![real_pid], "decoys outside: {outside_pids:?}");
}

#[test]
fn live_server_holds_one_pty_master_fd_per_pane() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
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
    let replacement_pid = wait_for_replacement_server_pid(
        &runtime_dir,
        &config_home,
        server_pid,
        Duration::from_secs(10),
    );
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
    UnixStream::connect(&client_socket).expect("handoff client listener is bound");
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
    UnixStream::connect(&client_socket).expect("handoff client listener is bound");
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
    let request = |socket: &Path, value| {
        try_request_bounded(socket, value, Some(Duration::from_secs(10)))
            .unwrap_or_else(|error| panic!("{}", error.message))
    };
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
            &config_home,
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
    UnixStream::connect(&client_socket).expect("handoff client listener is bound");

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
        // API readiness precedes the worker's failure event. Observe its final
        // reason rather than assuming ping also completed custody recovery.
        let status = support::fleet::wait_until(
            "mesh recovery failure",
            Duration::from_secs(10),
            || {
                let status = request(
                    &api_socket,
                    serde_json::json!({"id":"mesh-status", "method":"peers.enrollment", "params":{}}),
                );
                assert!(status.get("error").is_none(), "{status}");
                match &status["result"]["mesh_suspended_reason"] {
                    serde_json::Value::String(reason)
                        if reason == "injected mesh store open failure" =>
                    {
                        Some(status)
                    }
                    // The worker clears RECOVERING before publishing its reason.
                    serde_json::Value::Null => None,
                    serde_json::Value::String(reason)
                        if reason == "recovering store after handoff" =>
                    {
                        None
                    }
                    other => panic!("unexpected mesh recovery reason: {other}"),
                }
            },
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
            // The existing file gate holds real recovery open while the app answers.
            let status = request(
                &api_socket,
                serde_json::json!({"id":"slow-open-status", "method":"peers.enrollment", "params":{}}),
            );
            assert_eq!(
                status["result"]["mesh_suspended_reason"],
                "recovering store after handoff"
            );
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
                assert_ok(request(
                    &api_socket,
                    serde_json::json!({
                        "id":"responsive-after-rollback", "method":"ping", "params":{}
                    }),
                ));
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
        }
        fs::remove_file(&fail_file).unwrap();
        support::fleet::wait_until("mesh recovery completion", Duration::from_secs(15), || {
            let status = request(
                &api_socket,
                serde_json::json!({"id":"mesh-recovered", "method":"peers.enrollment", "params":{}}),
            );
            assert_ok(status.clone());
            assert_eq!(status["result"]["type"], "peers_enrollment", "{status}");
            // Successful enrollment omits the optional reason after recovery.
            match status["result"]["mesh_suspended_reason"].as_str() {
                None if status["result"]["mesh_suspended_reason"].is_null() => Some(()),
                Some("recovering store after handoff") => None,
                Some("injected mesh store open failure") if store_failure => None,
                other => panic!("unexpected recovery state {other:?}: {status}"),
            }
        });
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

/// More panes than one SCM_RIGHTS batch carries, with a remainder, so the
/// handoff needs three batches (64, 64, 22) (#471).
const MANY_PANES: usize = 150;

/// Open [`MANY_PANES`] panes, one workspace each, and return their ids in
/// creation order.
fn create_many_panes(api_socket: &Path) -> Vec<String> {
    (0..MANY_PANES)
        .map(|idx| {
            let created = request(
                api_socket,
                serde_json::json!({
                    "id": format!("test:workspace:create-{idx}"),
                    "method": "workspace.create",
                    "params": {"cwd": "/tmp", "focus": false}
                }),
            );
            created["result"]["root_pane"]["pane_id"]
                .as_str()
                .unwrap_or_else(|| panic!("workspace {idx} has no root pane: {created}"))
                .to_string()
        })
        .collect()
}

/// Prove a pane's pty is still wired to the server: the shell must evaluate
/// arithmetic, so the needle never appears in the typed text itself.
fn assert_pane_answers(api_socket: &Path, pane_id: &str, tag: usize) {
    assert_ok(request(
        api_socket,
        serde_json::json!({
            "id": format!("test:pane:answer-{tag}"),
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": format!("echo $(({tag}+1000))ok"), "keys": ["Enter"]}
        }),
    ));
    wait_for_output(api_socket, pane_id, &format!("{}ok", tag + 1000));
}

#[test]
fn live_handoff_carries_more_panes_than_one_fd_batch() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    register_runtime_dir(&runtime_dir);
    let server_pid = spawned
        .child
        .process_id()
        .expect("test server should expose pid");

    let panes = create_many_panes(&api_socket);
    wait_for_server_ptmx_fd_count(server_pid, MANY_PANES, Duration::from_secs(30));

    let handoff = request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    );
    assert_ok(handoff);
    let replacement_pid = wait_for_replacement_server_pid(
        &runtime_dir,
        &config_home,
        server_pid,
        Duration::from_secs(30),
    );
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(30));
    wait_for_server_ptmx_fd_count(replacement_pid, MANY_PANES, Duration::from_secs(30));

    // First, both batch boundaries, and last: each sits in a different batch.
    for idx in [0, 63, 64, 127, 128, MANY_PANES - 1] {
        assert_pane_answers(&api_socket, &panes[idx], idx);
    }

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_failing_mid_fd_transfer_leaves_every_pane_with_the_old_server() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");

    // The importer takes the first batch, then refuses: the transfer is cut
    // with 64 of 150 descriptors already in the new process.
    let spawned = spawn_server_with_env(
        &config_home,
        &runtime_dir,
        &api_socket,
        &[("FLOCK_TEST_HANDOFF_IMPORT_FAIL", "mid_fds")],
    );
    register_runtime_dir(&runtime_dir);
    let server_pid = spawned
        .child
        .process_id()
        .expect("test server should expose pid");

    let panes = create_many_panes(&api_socket);
    wait_for_server_ptmx_fd_count(server_pid, MANY_PANES, Duration::from_secs(30));

    let failed = request(
        &api_socket,
        serde_json::json!({"id":"test:handoff-fail","method":"server.live_handoff","params":{}}),
    );
    let message = failed["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("test handoff import failure after 64 of 150 fds"),
        "{failed}"
    );

    wait_for_api(&api_socket, Duration::from_secs(10));
    assert_eq!(
        unsafe { libc::kill(server_pid as libc::pid_t, 0) },
        0,
        "the old server must survive a refused handoff"
    );
    wait_for_server_ptmx_fd_count(server_pid, MANY_PANES, Duration::from_secs(10));
    for idx in [0, 63, 64, MANY_PANES - 1] {
        assert_pane_answers(&api_socket, &panes[idx], idx);
    }

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    drop(spawned);
    cleanup_test_base(&base);
}

/// Every `agent_delegated` event in the isolated server's durable log, as
/// (architect, delegate, via, reissued).
fn delegation_edges(config_home: &Path) -> Vec<(String, String, String, bool)> {
    let mut logs = Vec::new();
    let mut dirs = vec![config_home.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path
                .file_name()
                .is_some_and(|name| name == "event-log.jsonl")
            {
                logs.push(path);
            }
        }
    }
    assert_eq!(logs.len(), 1, "one event log in the sandbox: {logs:?}");
    fs::read_to_string(&logs[0])
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event["envelope"]["event"] == "agent_delegated")
        .map(|event| {
            let data = &event["envelope"]["data"];
            (
                data["architect"].as_str().unwrap().to_string(),
                data["delegate"].as_str().unwrap().to_string(),
                data["via"].as_str().unwrap().to_string(),
                data["reissued"].as_bool().unwrap_or(false),
            )
        })
        .collect()
}

/// #959: the edge starts where the value starts. An agent pane runs the real
/// `flk agent start`, so the server attests the caller by walking that
/// process's ancestry to the pane, and the test ends at the event on the
/// durable log. A live handoff then folds the log in the new server, which
/// re-issues the still-open edge. A start from this test process, which no
/// pane is an ancestor of, records nothing and says so.
#[test]
fn delegation_edge_from_an_agent_pane_is_logged_and_reissued_across_live_handoff() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");
    let child_out = base.join("child-start.json");
    let started_marker = base.join("child-started");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    register_runtime_dir(&runtime_dir);

    let command = format!(
        "FLOCK_SOCKET_PATH={socket} {flk} agent start child --cwd /tmp -- /bin/sh -c 'exec sleep 600' > {out} 2>&1; echo done > {marker}; exec sleep 600",
        socket = api_socket.display(),
        flk = env!("CARGO_BIN_EXE_flk"),
        out = child_out.display(),
        marker = started_marker.display(),
    );
    let architect = request(
        &api_socket,
        serde_json::json!({
            "id": "test:architect",
            "method": "agent.start",
            "params": {"name": "architect", "cwd": "/tmp", "argv": ["/bin/sh", "-c", command]}
        }),
    );
    assert_ok(architect.clone());
    let architect_id = architect["result"]["agent"]["agent_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        architect["result"]["edge"], "none (unattested)",
        "the test process is not in any pane"
    );
    support::wait_for_file(&started_marker, Duration::from_secs(10));
    let child: serde_json::Value = serde_json::from_str(&fs::read_to_string(&child_out).unwrap())
        .unwrap_or_else(|err| panic!("child start output: {err}"));
    let child_id = child["result"]["agent"]["agent_id"]
        .as_str()
        .unwrap_or_else(|| panic!("child start failed: {child}"))
        .to_string();
    assert_eq!(
        child["result"]["edge"],
        serde_json::json!(format!("architect {architect_id}"))
    );
    assert_eq!(
        delegation_edges(&config_home),
        [(
            architect_id.clone(),
            child_id.clone(),
            "start".to_string(),
            false
        )],
        "exactly one edge, with the calling pane's agent as architect"
    );

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));
    // A request the app loop answers, so the replacement server is past the
    // once-per-start fold rather than only listening.
    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:list","method":"agent.list","params":{}}),
    ));

    let edges = delegation_edges(&config_home);
    assert!(
        edges.contains(&(
            architect_id.clone(),
            child_id.clone(),
            "start".to_string(),
            true
        )),
        "the handed-off server re-issues the open edge: {edges:?}"
    );
    assert!(
        !edges.iter().any(|edge| edge.1 == architect_id),
        "the operator-started architect has no architect of its own: {edges:?}"
    );

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

/// #1179 end to end: a real `flk` run inside an agent's pane. The agent it
/// starts reads `route` and its pane carries `FLOCK_QUESTIONS=route`; widening
/// a delegate, through `agent start` and `delegate start`, exits 13 and starts
/// nothing; the policy survives a live handoff.
#[test]
fn question_policy_follows_the_starter_refuses_widening_and_survives_live_handoff() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");
    fs::create_dir_all(&base).unwrap();
    let brief = base.join("brief.md");
    fs::write(&brief, "never typed\n").unwrap();
    let done = base.join("done");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    register_runtime_dir(&runtime_dir);

    let command = format!(
        "export FLOCK_SOCKET_PATH={socket}; \
         {flk} agent start child --cwd /tmp -- /bin/sh -c 'printf %s \"$FLOCK_QUESTIONS\" > {base}/hint; exec sleep 600' > {base}/child.json 2>&1; \
         {flk} agent start wide --cwd /tmp --questions ask_human -- /bin/sh -c 'exec sleep 600' > {base}/wide.out 2>&1; echo $? > {base}/wide.code; \
         {flk} delegate start widedel --brief {brief} --cwd /tmp --unattended > {base}/del.out 2>&1; echo $? > {base}/del.code; \
         echo done > {done}; exec sleep 600",
        socket = api_socket.display(),
        flk = env!("CARGO_BIN_EXE_flk"),
        base = base.display(),
        brief = brief.display(),
        done = done.display(),
    );
    let architect = request(
        &api_socket,
        serde_json::json!({
            "id": "test:architect",
            "method": "agent.start",
            "params": {"name": "architect", "cwd": "/tmp", "argv": ["/bin/sh", "-c", command]}
        }),
    );
    assert_ok(architect.clone());
    assert_eq!(architect["result"]["agent"]["questions"], "ask_human");
    support::wait_for_file(&done, Duration::from_secs(30));

    let child: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(base.join("child.json")).unwrap())
            .unwrap_or_else(|err| panic!("child start output: {err}"));
    assert_eq!(child["result"]["agent"]["questions"], "route", "{child}");
    support::wait_for_file(&base.join("hint"), Duration::from_secs(10));
    assert_eq!(fs::read_to_string(base.join("hint")).unwrap(), "route");
    let wide = fs::read_to_string(base.join("wide.out")).unwrap();
    assert_eq!(
        fs::read_to_string(base.join("wide.code")).unwrap().trim(),
        "13",
        "{wide}"
    );
    assert!(wide.contains("questions_widening_refused"), "{wide}");
    let del = fs::read_to_string(base.join("del.out")).unwrap();
    assert_eq!(
        fs::read_to_string(base.join("del.code")).unwrap().trim(),
        "13",
        "{del}"
    );
    let names = |socket: &Path| -> Vec<String> {
        request(
            socket,
            serde_json::json!({"id":"test:list","method":"agent.list","params":{}}),
        )["result"]["agents"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|agent| agent["name"].as_str().map(str::to_string))
            .collect()
    };
    let before = names(&api_socket);
    assert!(
        !before
            .iter()
            .any(|name| name == "wide" || name == "widedel"),
        "a refused start creates nothing: {before:?}"
    );

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));
    let got = request(
        &api_socket,
        serde_json::json!({"id":"test:get","method":"agent.get","params":{"target":"child"}}),
    );
    assert_eq!(got["result"]["agent"]["questions"], "route", "{got}");

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

/// A Claude session id that passes `valid_session_id`. A UUID, like the real
/// ones, so the transcript file name has the shape Claude writes.
const IDLE_CLAUDE_SESSION: &str = "0975c14a-1111-4222-8333-444455556666";

fn visible_text(api_socket: &Path, pane_id: &str) -> String {
    let response = request(
        api_socket,
        serde_json::json!({
            "id": "test:pane:visible",
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
    response["result"]["read"]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// #977: one idle, hook-confirmed Claude pane with unread mail, carried across
/// a live handoff with no client attached. Each assertion after the handoff
/// is one of the issue's symptoms.
#[test]
fn live_handoff_keeps_an_idle_claude_pane_readable_repainted_and_its_mail_queued() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    register_runtime_dir(&runtime_dir);
    let server_pid = spawned
        .child
        .process_id()
        .expect("test server should expose pid");

    // An idle TUI in miniature: it paints once, then draws again only when a
    // SIGWINCH asks it to, the way claude, codex and opencode behave at rest.
    let script = "n=0; trap 'n=$((n+1)); printf \"\\033[2J\\033[HREPAINTED-%s %s\\n\" \"$n\" \"$(stty size)\"' WINCH; \
                  printf 'IDLE-SCREEN %s\\n' \"$(stty size)\"; while :; do sleep 0.1; done";
    let started = request(
        &api_socket,
        serde_json::json!({
            "id": "test:agent-start",
            "method": "agent.start",
            "params": {
                "name": "idle-claude",
                "cwd": "/tmp",
                "focus": true,
                "argv": ["/bin/sh", "-c", script]
            }
        }),
    );
    assert_ok(started.clone());
    let pane_id = started["result"]["agent"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    wait_for_output(&api_socket, &pane_id, "IDLE-SCREEN");
    // The hook report the real claude integration sends: it names the session,
    // which is what licenses a transcript read.
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:claude-hook",
            "method": "pane.report_agent",
            "params": {
                "pane_id": pane_id,
                "source": "flock:claude",
                "agent": "claude",
                "state": "idle",
                "agent_session_id": IDLE_CLAUDE_SESSION
            }
        }),
    ));
    // Move the operator elsewhere. A detached server resizes only the panes
    // of the active tab on its first render, so a pane in a background
    // workspace, where most of a fleet's agents sit, gets no SIGWINCH from
    // the importer's layout pass and keeps the empty screen it arrived with.
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:foreground",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    ));

    let project = config_home.join("home/.claude/projects/-tmp");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join(format!("{IDLE_CLAUDE_SESSION}.jsonl")),
        format!(
            "{}\n",
            serde_json::json!({
                "type": "user",
                "message": {"role": "user", "content": "before the handoff"}
            })
        ),
    )
    .unwrap();
    let history_request = serde_json::json!({
        "id": "test:agent-history",
        "method": "agent.history",
        "params": {"target": pane_id}
    });
    let before = request(&api_socket, history_request.clone());
    assert_eq!(
        before["result"]["history"]["session_id"], IDLE_CLAUDE_SESSION,
        "the hook-confirmed session must be readable before the handoff: {before}"
    );

    // fyi never wakes the pane, so the message stays unread in its inbox.
    let sent = request(
        &api_socket,
        serde_json::json!({
            "id": "test:msg-send",
            "method": "msg.send",
            "params": {
                "to": {"type": "pane", "pane": pane_id},
                "body": "unread across the handoff",
                "intent": "fyi",
                "correlation_id": "c-977-unread"
            }
        }),
    );
    assert_ok(sent.clone());
    let status_request = serde_json::json!({
        "id": "test:msg-status",
        "method": "msg.status",
        "params": {"correlation_id": "c-977-unread"}
    });
    let status_before = request(&api_socket, status_request.clone());
    assert!(
        status_before["result"]["reference"].is_object(),
        "the message must travel the mesh store path the issue observed: {status_before}"
    );
    assert_eq!(
        status_before["result"]["state"], "queued",
        "unread mail is queued, not delivered: {status_before}"
    );

    // An operator attached at some point, so every pane already has the size
    // the detached layout gives it, and the importer's own layout pass, which
    // resizes only what changed, sends this pane no SIGWINCH. The pane must
    // repaint for that resize first, or its later repaint proves nothing.
    let protocol = request(
        &api_socket,
        serde_json::json!({"id":"test:protocol","method":"ping","params":{}}),
    )["result"]["protocol"]
        .as_u64()
        .unwrap() as u32;
    let mut client_stream = UnixStream::connect(runtime_dir.join("flock-client.sock")).unwrap();
    let (_, error) = client_handshake(&mut client_stream, protocol, 80, 24).unwrap();
    assert!(error.is_none(), "client handshake failed: {error:?}");
    wait_for_output(&api_socket, &pane_id, "REPAINTED-");
    drop(client_stream);
    thread::sleep(Duration::from_millis(500));
    let settled_screen = visible_text(&api_socket, &pane_id);
    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    wait_for_replacement_server_pid(
        &runtime_dir,
        &config_home,
        server_pid,
        Duration::from_secs(30),
    );
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(30));

    // (a) No client attaches. The importer itself must get the idle pane to
    // draw again: its screen arrives empty, since agent panes replay no history.
    let deadline = Instant::now() + Duration::from_secs(10);
    let repainted = |screen: &str| screen.contains("REPAINTED-") && screen != settled_screen;
    let mut screen = visible_text(&api_socket, &pane_id);
    while !repainted(&screen) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
        screen = visible_text(&api_socket, &pane_id);
    }
    assert!(
        repainted(&screen),
        "an idle agent pane came back blank after a headless live handoff: \
         {screen:?}, before the handoff it showed {settled_screen:?}"
    );

    // (b) Still unread, so still queued.
    let status_after = request(&api_socket, status_request);
    assert_eq!(
        status_after["result"]["state"], "queued",
        "msg status must not claim delivery of unread mail after a handoff: {status_after}"
    );
    let queued = request(
        &api_socket,
        serde_json::json!({"id":"test:msg-list","method":"msg.list","params":{"pane": pane_id}}),
    );
    assert!(
        queued.to_string().contains("c-977-unread"),
        "the message is still in the inbox: {queued}"
    );

    // (c) The same process holds the same session, so it stays readable.
    let after = request(&api_socket, history_request);
    assert_eq!(
        after["result"]["history"]["session_id"], IDLE_CLAUDE_SESSION,
        "a hook-confirmed claude session must stay readable across a live handoff: {after}"
    );

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

/// A Nix-shaped install: every version lives at its own store path forever,
/// and the profile's `bin/flk` is a symlink that a switch repoints.
fn nix_style_install(base: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let old = install_store_build(base, "aaaa-flock-old");
    let new = install_store_build(base, "bbbb-flock-new");
    let profile_flk = link_profile(base, &new);
    (old, new, profile_flk)
}

fn store_bin(base: &Path, name: &str) -> PathBuf {
    let bin = base.join("store").join(name).join("bin");
    fs::create_dir_all(&bin).unwrap();
    bin.join("flk")
}

fn install_store_build(base: &Path, name: &str) -> PathBuf {
    let exe = store_bin(base, name);
    // A hard link is a second path to the same build, the way two store
    // paths of one derivation would be; copy when the temp dir is on
    // another filesystem.
    if fs::hard_link(env!("CARGO_BIN_EXE_flk"), &exe).is_err() {
        fs::copy(env!("CARGO_BIN_EXE_flk"), &exe).unwrap();
    }
    fs::canonicalize(&exe).unwrap()
}

/// A store path holding an older flk: the test build with its reported
/// version overridden, so one build plays two versions. With
/// `answers_version = false` its `--version` fails, as an unprobeable binary.
fn install_store_stub(base: &Path, name: &str, version: &str, answers_version: bool) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let exe = store_bin(base, name);
    let version_reply = if answers_version {
        format!("echo 'flk {version}'; exit 0")
    } else {
        "exit 1".to_string()
    };
    fs::write(
        &exe,
        format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then {version_reply}; fi\nFLOCK_TEST_BUILD_VERSION={version} exec '{}' \"$@\"\n",
            env!("CARGO_BIN_EXE_flk")
        ),
    )
    .unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
    fs::canonicalize(&exe).unwrap()
}

fn link_profile(base: &Path, target: &Path) -> PathBuf {
    let profile_bin = base.join("profile/bin");
    fs::create_dir_all(&profile_bin).unwrap();
    let profile_flk = profile_bin.join("flk");
    std::os::unix::fs::symlink(target, &profile_flk).unwrap();
    profile_flk
}

fn run_handoff_cli(
    exe: &Path,
    extra_args: &[&str],
    cwd: &Path,
    config_home: &Path,
    runtime_dir: &Path,
    api_socket: &Path,
) -> (bool, String) {
    let output = support::environment::Command::new(exe)
        .args(["server", "live-handoff"])
        .args(extra_args)
        .current_dir(cwd)
        .envs(support::environment::isolated_env(config_home, runtime_dir))
        .env("XDG_STATE_HOME", runtime_dir.join("state"))
        .env("FLOCK_SOCKET_PATH", api_socket)
        .env(
            "FLOCK_CLIENT_SOCKET_PATH",
            runtime_dir.join("flock-client.sock"),
        )
        .env_remove("FLOCK_ENV")
        .env_remove("FLOCK_TEST_BUILD_VERSION")
        .output()
        .expect("client runs");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn server_version(api_socket: &Path) -> String {
    request(
        api_socket,
        serde_json::json!({"id":"version","method":"ping","params":{}}),
    )["result"]["version"]
        .as_str()
        .expect("server reports a version")
        .to_string()
}

fn client_version() -> String {
    let output = support::environment::Command::new(env!("CARGO_BIN_EXE_flk"))
        .arg("--version")
        .env_remove("FLOCK_TEST_BUILD_VERSION")
        .output()
        .expect("flk --version runs");
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .strip_prefix("flk ")
        .expect("flk --version banner")
        .to_string()
}

fn stop_and_clean(api_socket: &Path, base: &Path) {
    let _ = try_request(
        api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(base);
}

fn process_argv0(pid: u32) -> String {
    let output = support::environment::Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .expect("ps runs");
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// #958: a server running from an old store path, handed off by the client
/// the profile now points at, must come back as the client's binary. Before
/// the fix the CLI sent no import path and the server re-executed its own,
/// which on Nix is the old version again, and still reported success.
#[test]
fn live_handoff_cli_hands_off_to_the_invoking_client_binary() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");
    let (old_exe, new_exe, profile_flk) = nix_style_install(&base);

    let spawned = spawn_server_exe(&old_exe, &config_home, &runtime_dir, &api_socket, &[]);
    register_runtime_dir(&runtime_dir);
    let server_pid = spawned
        .child
        .process_id()
        .expect("test server should expose pid");
    assert_eq!(process_argv0(server_pid), old_exe.display().to_string());

    let output = support::environment::Command::new(&profile_flk)
        .args(["server", "live-handoff"])
        .envs(support::environment::isolated_env(
            &config_home,
            &runtime_dir,
        ))
        .env("XDG_STATE_HOME", runtime_dir.join("state"))
        .env("FLOCK_SOCKET_PATH", &api_socket)
        .env(
            "FLOCK_CLIENT_SOCKET_PATH",
            runtime_dir.join("flock-client.sock"),
        )
        .env_remove("FLOCK_ENV")
        .output()
        .expect("client runs");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "handoff failed:\n{stderr}");

    let replacement_pid =
        wait_for_handoff_importer_pid(&config_home, server_pid, Duration::from_secs(30));
    drop(spawned);
    assert_eq!(
        process_argv0(replacement_pid),
        new_exe.display().to_string(),
        "the replacement must run the client's binary, not the old server's"
    );
    let version = request(
        &api_socket,
        serde_json::json!({"id":"version","method":"ping","params":{}}),
    )["result"]["version"]
        .as_str()
        .expect("replacement reports a version")
        .to_string();
    assert!(
        stderr.contains(&format!(
            "live handoff complete: server flk {version} -> flk {version}"
        )),
        "the report must name both versions; stderr was:\n{stderr}"
    );

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

/// #958 end to end: an older server on its own store path upgrades to the
/// client the profile points at, and the report names both versions.
#[test]
fn live_handoff_upgrades_an_older_server_to_the_client_version() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");
    let old_exe = install_store_stub(&base, "aaaa-flock-old", "0.0.1", true);
    let new_exe = install_store_build(&base, "bbbb-flock-new");
    let profile_flk = link_profile(&base, &new_exe);
    let client = client_version();

    let spawned = spawn_server_exe(&old_exe, &config_home, &runtime_dir, &api_socket, &[]);
    register_runtime_dir(&runtime_dir);
    let server_pid = spawned.child.process_id().expect("server pid");
    assert_eq!(server_version(&api_socket), "0.0.1");

    let (ok, stderr) = run_handoff_cli(
        &profile_flk,
        &[],
        &base,
        &config_home,
        &runtime_dir,
        &api_socket,
    );
    assert!(ok, "handoff failed:\n{stderr}");
    let replacement_pid =
        wait_for_handoff_importer_pid(&config_home, server_pid, Duration::from_secs(30));
    drop(spawned);
    assert_eq!(
        process_argv0(replacement_pid),
        new_exe.display().to_string()
    );
    assert_eq!(server_version(&api_socket), client);
    assert!(
        stderr.contains(&format!(
            "live handoff complete: server flk 0.0.1 -> flk {client}"
        )),
        "stderr was:\n{stderr}"
    );

    stop_and_clean(&api_socket, &base);
}

/// An explicit `--import-exe` of the old version is refused before anything
/// moves, and a relative path is resolved against the client's cwd.
#[test]
fn live_handoff_refuses_a_relative_import_exe_of_the_old_version() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");
    let old_exe = install_store_stub(&base, "aaaa-flock-old", "0.0.1", true);
    let client = client_version();

    let spawned = spawn_server_exe(&old_exe, &config_home, &runtime_dir, &api_socket, &[]);
    register_runtime_dir(&runtime_dir);

    let (ok, stderr) = run_handoff_cli(
        Path::new(env!("CARGO_BIN_EXE_flk")),
        &["--import-exe", "store/aaaa-flock-old/bin/flk"],
        &base,
        &config_home,
        &runtime_dir,
        &api_socket,
    );
    assert!(
        !ok,
        "a handoff to the old version must not succeed:\n{stderr}"
    );
    assert!(stderr.contains("refusing live handoff"), "{stderr}");
    assert!(stderr.contains("flk 0.0.1"), "{stderr}");
    assert!(stderr.contains(&format!("flk {client}")), "{stderr}");
    assert_eq!(server_version(&api_socket), "0.0.1");

    stop_and_clean(&api_socket, &base);
    drop(spawned);
}

/// The verification path: a binary whose version cannot be probed is handed
/// off to, comes back as the old version, and the CLI exits 1 instead of
/// reporting success.
#[test]
fn live_handoff_fails_when_the_replacement_is_still_the_old_version() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");
    let old_exe = install_store_stub(&base, "aaaa-flock-old", "0.0.1", false);
    let client = client_version();

    let spawned = spawn_server_exe(&old_exe, &config_home, &runtime_dir, &api_socket, &[]);
    register_runtime_dir(&runtime_dir);
    let server_pid = spawned.child.process_id().expect("server pid");

    let old_path = old_exe.display().to_string();
    let (ok, stderr) = run_handoff_cli(
        Path::new(env!("CARGO_BIN_EXE_flk")),
        &["--import-exe", &old_path],
        &base,
        &config_home,
        &runtime_dir,
        &api_socket,
    );
    assert!(
        !ok,
        "a same-version re-exec must not report success:\n{stderr}"
    );
    // src/cli/server.rs::judge_handoff emits this refusal after querying the
    // replacement. run_handoff_cli waits for that command's actual exit/output.
    assert!(stderr.contains("same server version"), "{stderr}");
    assert!(stderr.contains(&format!("flk {client}")), "{stderr}");
    wait_for_handoff_importer_pid(&config_home, server_pid, Duration::from_secs(30));
    drop(spawned);
    assert_eq!(server_version(&api_socket), "0.0.1");

    stop_and_clean(&api_socket, &base);
}

/// A client older than the server (a stale `flk` earlier on PATH) refuses the
/// default handoff instead of downgrading the server to itself.
#[test]
fn live_handoff_refuses_a_default_downgrade() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");
    let client = client_version();

    let spawned = spawn_server_with_env(
        &config_home,
        &runtime_dir,
        &api_socket,
        &[("FLOCK_TEST_BUILD_VERSION", "99.0.0")],
    );
    register_runtime_dir(&runtime_dir);

    let (ok, stderr) = run_handoff_cli(
        Path::new(env!("CARGO_BIN_EXE_flk")),
        &[],
        &base,
        &config_home,
        &runtime_dir,
        &api_socket,
    );
    assert!(!ok, "a downgrade must not succeed:\n{stderr}");
    assert!(stderr.contains("older than"), "{stderr}");
    assert!(stderr.contains("flk 99.0.0"), "{stderr}");
    assert!(stderr.contains(&format!("flk {client}")), "{stderr}");
    assert_eq!(server_version(&api_socket), "99.0.0");

    stop_and_clean(&api_socket, &base);
    drop(spawned);
}

/// #981, #1011: a pane whose child never reads its input must not wedge every
/// handoff anonymously. The refusal names the pane and its pending bytes, the
/// old server keeps running, and `skip_undrained` hands off past it, reporting
/// what it dropped.
#[test]
fn live_handoff_names_undrained_pane_and_skip_undrained_proceeds() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("flock.sock");
    let marker = base.join("stuck.pid");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
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
    // Non-canonical with no reader: the tty input queue fills and the PTY
    // master stops accepting writes, as a pane that never reads does.
    let command = format!(
        "stty -icanon -echo; echo STUCK $$ > {}; exec sleep 60",
        marker.display()
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
    thread::sleep(Duration::from_millis(200));
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:flood",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": "x".repeat(256 * 1024)}
        }),
    ));

    let failed = request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    );
    assert_eq!(
        failed["error"]["code"].as_str(),
        Some("handoff_undrained"),
        "an undrained pane must fail the handoff by name: {failed}"
    );
    let message = failed["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains(&format!("{pane_id} (")) && message.contains("bytes pending"),
        "refusal must name {pane_id} and its pending bytes: {message}"
    );
    wait_for_api(&api_socket, Duration::from_secs(5));
    assert_eq!(unsafe { libc::kill(child_pid as libc::pid_t, 0) }, 0);

    let skipped = request(
        &api_socket,
        serde_json::json!({
            "id": "test:handoff:skip",
            "method": "server.live_handoff",
            "params": {"skip_undrained": true}
        }),
    );
    assert_eq!(
        skipped["result"]["type"].as_str(),
        Some("live_handoff"),
        "skip_undrained handoff should succeed and report: {skipped}"
    );
    let dropped = skipped["result"]["skipped_undrained"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        dropped
            .iter()
            .any(|pane| pane["pane_id"].as_str() == Some(&pane_id)
                && pane["pending_bytes"].as_u64().unwrap_or(0) > 0),
        "skipped_undrained must list {pane_id} with its dropped bytes: {skipped}"
    );
    drop(spawned);
    thread::sleep(Duration::from_millis(300));
    wait_for_api(&api_socket, Duration::from_secs(10));
    assert_eq!(
        unsafe { libc::kill(child_pid as libc::pid_t, 0) },
        0,
        "the stuck pane's process survives the handoff"
    );

    unsafe {
        libc::kill(child_pid as libc::pid_t, libc::SIGKILL);
    }
    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}
