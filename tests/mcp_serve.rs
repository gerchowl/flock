// TracedCommand (logging redesign PR-3) polices flock's shipped code; this
// harness is a plain integration test that spawns the mcp server subprocess,
// so it opts out of the source-code lint. Same posture as the other tests/
// harnesses (cli_wrapper, live_handoff, …).
#![allow(clippy::disallowed_methods)]
#![allow(clippy::print_stderr)]

//! Integration test for `flk mcp serve`.
//!
//! Boots a real flock server (same PTY harness as `tests/api_ping.rs`), then
//! spawns `flk mcp serve` as a plain child process wired to pipes with
//! `FLOCK_SOCKET_PATH` pointing at the server socket. Drives the whole
//! JSON-RPC handshake (initialize → tools/list → tools/call → refusal check)
//! through the child's stdin/stdout the way a real MCP client would.

mod support;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, Child as PtyChild, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};
use support::{
    cleanup_test_base, client_handshake, drain_messages, register_runtime_dir,
    register_spawned_flock_pid, send_clipboard_file, send_set_frame_subscription,
    unregister_spawned_flock_pid, wait_for_file,
};

// ---- harness (a lean copy of api_ping.rs' spawn helpers) -----------------

fn unique_test_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!("/tmp/mcp-{}-{nanos}", std::process::id()))
}

fn test_lock() -> MutexGuard<'static, ()> {
    // Shared with api_ping.rs's harness lock so parallel test binaries don't
    // race on the shared /tmp namespace — one flock server per test at a time.
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn wait_for_socket(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() && std::os::unix::net::UnixStream::connect(path).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("socket did not appear at {}", path.display());
}

struct SpawnedFlock {
    _master: Box<dyn MasterPty + Send>,
    child: Box<dyn PtyChild + Send + Sync>,
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

fn spawn_flock(config_home: &Path, runtime_dir: &Path, socket_path: &Path) -> SpawnedFlock {
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
    cmd.env("FLOCK_SOCKET_PATH", socket_path);
    cmd.env_remove("FLOCK_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("FLOCK_ENV");

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_flock_pid(child.process_id());

    SpawnedFlock {
        _master: pair.master,
        child,
    }
}

/// The MCP client: `flk mcp serve` wired to std pipes. Speaks JSON-RPC 2.0,
/// one line per message, matching real MCP clients (Claude Desktop, `mcp`).
struct McpClient {
    child: Child,
    stdout: BufReader<std::process::ChildStdout>,
    stdin: std::process::ChildStdin,
    stderr: Option<std::process::ChildStderr>,
}

impl McpClient {
    fn spawn(socket_path: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_flk"))
            .args(["mcp", "serve"])
            .env("FLOCK_SOCKET_PATH", socket_path)
            .env_remove("FLOCK_CLIENT_SOCKET_PATH")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn flk mcp serve");
        let stdout = BufReader::new(child.stdout.take().expect("mcp stdout"));
        let stdin = child.stdin.take().expect("mcp stdin");
        let stderr = child.stderr.take();
        Self {
            child,
            stdout,
            stdin,
            stderr,
        }
    }

    fn send(&mut self, message: &Value) {
        let mut buf = serde_json::to_vec(message).unwrap();
        buf.push(b'\n');
        self.stdin.write_all(&buf).unwrap();
        self.stdin.flush().unwrap();
    }

    fn recv(&mut self, timeout: Duration) -> Value {
        // Blocking read with a watchdog thread that kills the child on
        // timeout, so a hung server never wedges the test suite.
        let (tx, rx) = std::sync::mpsc::channel();
        let child_pid = self.child.id();
        let killer = thread::spawn(move || {
            if rx.recv_timeout(timeout).is_err() {
                unsafe {
                    libc::kill(child_pid as libc::pid_t, libc::SIGKILL);
                }
            }
        });
        let mut line = String::new();
        let read = self.stdout.read_line(&mut line);
        // Signal the watchdog we finished (regardless of outcome).
        let _ = tx.send(());
        let _ = killer.join();
        let n = read.expect("read mcp stdout");
        assert!(n > 0, "mcp server closed stdout early");
        serde_json::from_str(line.trim()).unwrap_or_else(|e| {
            panic!("mcp server produced non-JSON line: {line:?} ({e})");
        })
    }

    fn shutdown(mut self) {
        // Closing stdin triggers the serve loop's clean EOF-exit path.
        drop(self.stdin);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        // Fall back to a kill so nothing lingers.
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(mut err) = self.stderr.take() {
            let mut sink = String::new();
            let _ = err.read_to_string(&mut sink);
            if !sink.is_empty() {
                eprintln!("mcp stderr: {sink}");
            }
        }
    }
}

// ---- tests ---------------------------------------------------------------

#[test]
fn mcp_stdio_handshake_and_tool_call_round_trip() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("flock.sock");

    let server = spawn_flock(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let mut mcp = McpClient::spawn(&socket_path);

    // 1. initialize — MCP handshake. Server advertises `tools` capability.
    mcp.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}},
    }));
    let init = mcp.recv(Duration::from_secs(5));
    assert_eq!(init["id"], 1, "init response: {init}");
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(init["result"]["capabilities"]["tools"], json!({}));
    assert_eq!(init["result"]["serverInfo"]["name"], "flock");

    // 2. initialized notification — must NOT produce a response line. Any
    // stray line here would confuse a real client, so we deliberately send it
    // followed by a request that DOES expect a response, then assert we get
    // the request's response (not two lines).
    mcp.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

    // 3. tools/list — the closed table, in order.
    mcp.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}));
    let list = mcp.recv(Duration::from_secs(5));
    assert_eq!(list["id"], 2);
    let tools = list["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec![
            "flock_agent_list",
            "flock_agent_get",
            "flock_agent_read",
            "flock_agent_fork",
            "flock_agent_lineage",
            "flock_msg_send",
            "flock_msg_reply",
            "flock_msg_list",
            "flock_msg_read",
            "flock_msg_mute",
            "flock_pane_read",
            "flock_worktree_list",
            "flock_agent_start",
            "flock_agent_history",
        ]
    );
    for tool in tools {
        assert!(tool["description"].as_str().is_some_and(|s| !s.is_empty()));
        assert_eq!(tool["inputSchema"]["type"], "object");
    }

    // 4. tools/call → flock_agent_list. A freshly-booted server has no
    // detected agents; we just verify the round trip's shape.
    mcp.send(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {"name": "flock_agent_list", "arguments": {}},
    }));
    let call = mcp.recv(Duration::from_secs(5));
    assert_eq!(call["id"], 3, "call response: {call}");
    let text = call["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("expected text content, got {call}"));
    let inner: Value = serde_json::from_str(text).unwrap();
    assert_eq!(inner["type"], "agent_list");
    assert!(inner["agents"].is_array());

    // 5. tools/call with a hidden verb — must refuse with the design's
    // `not_exposed_via_mcp` tag, without touching the flock server.
    mcp.send(&json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {"name": "flock_pane_close", "arguments": {"pane_id": "p1"}},
    }));
    let refusal = mcp.recv(Duration::from_secs(5));
    assert_eq!(refusal["id"], 4);
    assert_eq!(refusal["error"]["code"], -32000);
    assert_eq!(refusal["error"]["data"]["refusal"], "not_exposed_via_mcp");

    mcp.shutdown();
    drop(server);
    cleanup_test_base(&base);
}

/// #286 end to end, starting where the bytes really start.
///
/// A dropped file reaches the server as `ClientMessage::ClipboardImage` on the
/// client socket — that is the wire, and everything above it (the OS drag
/// event, the local path read) is what #79 already documented as un-CI-able.
/// From there this drives a REAL `flk mcp serve` and asserts the file comes
/// back as an MCP resource it can list and read.
///
/// Then it drops the client connection and lists again. Before #286 the staged
/// file was deleted with the connection that produced it, so that second
/// listing is the whole durability claim.
#[test]
fn a_dropped_file_is_listed_and_read_as_an_mcp_resource() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("flock.sock");
    let client_socket = runtime_dir.join("flock-client.sock");

    let server = spawn_flock(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));

    // Attach a client and hand a file over the way a drop does.
    let mut stream = UnixStream::connect(&client_socket).expect("connect to client socket");
    let (_version, error) = client_handshake(&mut stream, support::PROTOCOL_VERSION, 80, 24)
        .expect("handshake should succeed");
    assert!(error.is_none(), "handshake error: {error:?}");
    // Stop frame streaming so the server's writer never blocks on a test
    // client that is not draining renders.
    send_set_frame_subscription(&mut stream, false).expect("pause frames");
    drain_messages(&mut stream);
    send_clipboard_file(&mut stream, "md", b"# handed over\n").expect("send dropped file");

    let mut mcp = McpClient::spawn(&socket_path);
    mcp.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}},
    }));
    let init = mcp.recv(Duration::from_secs(5));
    assert_eq!(
        init["result"]["capabilities"]["resources"],
        json!({}),
        "the server has to advertise resources for a client to ask for them: {init}"
    );

    // The drop is asynchronous relative to the socket call, so poll the
    // resource list rather than assuming an ordering the wire does not give.
    let mut resource = Value::Null;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut next_id = 2;
    while Instant::now() < deadline {
        mcp.send(
            &json!({"jsonrpc": "2.0", "id": next_id, "method": "resources/list", "params": {}}),
        );
        let listed = mcp.recv(Duration::from_secs(5));
        next_id += 1;
        let resources = listed["result"]["resources"]
            .as_array()
            .unwrap_or_else(|| panic!("expected a resources array, got {listed}"));
        if let Some(found) = resources.first() {
            resource = found.clone();
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        resource["uri"]
            .as_str()
            .is_some_and(|uri| uri.starts_with("flock://handoff/")),
        "the dropped file never appeared as a resource: {resource}"
    );
    assert_eq!(resource["mimeType"], "text/markdown");
    assert_eq!(resource["size"], 14);

    // Read it back by URI. The bytes never went through the pane.
    mcp.send(&json!({
        "jsonrpc": "2.0",
        "id": next_id,
        "method": "resources/read",
        "params": {"uri": resource["uri"]},
    }));
    let read = mcp.recv(Duration::from_secs(5));
    assert_eq!(read["id"], next_id, "read response: {read}");
    next_id += 1;
    assert_eq!(read["result"]["contents"][0]["text"], "# handed over\n");
    assert_eq!(read["result"]["contents"][0]["uri"], resource["uri"]);

    // The connection that handed the file over goes away. The resource does
    // not: that is the difference between a paste and a durable record.
    drop(stream);
    thread::sleep(Duration::from_millis(500));
    mcp.send(&json!({
        "jsonrpc": "2.0",
        "id": next_id,
        "method": "resources/read",
        "params": {"uri": resource["uri"]},
    }));
    let after = mcp.recv(Duration::from_secs(5));
    assert_eq!(
        after["result"]["contents"][0]["text"], "# handed over\n",
        "the handed-over file must outlive the client that handed it over: {after}"
    );

    mcp.shutdown();
    drop(server);
    cleanup_test_base(&base);
}
