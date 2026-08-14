//! Multi-node fleet fixture: spawn N real `flk` servers on one machine, wire
//! them into an arbitrary gossip topology over a dispatching fake-`ssh` shim,
//! and read their rendered frames like screenshots.
//!
//! This is the "real network setup" vehicle without containers: every node is
//! the actual binary with its own config home, runtime dir, API socket and
//! client socket. The only fake is `ssh` — a shim that maps a peer NAME onto
//! that node's socket and execs the command locally. Everything above it (the
//! poll round, the relay merge, the snapshot pass-through, the sidebar render,
//! the switch handshake) is production code.
//!
//! Speed comes from three places, since the default cadence would make every
//! case a 20-second wait:
//!   - `[gossip] poll_interval_secs = 1`, `initial_delay_secs = 0` — a chain
//!     converges in a couple of seconds instead of 3s + 15s per hop.
//!   - `name = "<node>"` in each config, so co-located nodes have DISTINCT
//!     fleet identities. Without it every node reports the same
//!     `short_host_name()` and the relay's loop prevention (drop entries whose
//!     origin is me) eats the whole topology.
//!   - A shared read-only fleet per test binary ([`shared`]), so N assertions
//!     pay for one fleet.

#![allow(dead_code)]
// Test scaffolding runs real `git` to build its fake fleet. The TracedCommand
// funnel (logging redesign PR-3) is about the product's subprocesses, not the
// harness's — and this module is compiled into every test crate that pulls in
// `support`, so the allow has to live here rather than on one crate root.
#![allow(clippy::disallowed_methods)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::Deserialize;

use super::{
    read_server_message, register_runtime_dir, register_spawned_flock_pid, wait_for_file,
    wait_for_socket,
};

/// ServerMessage bincode variant indices (declaration order in wire.rs).
pub const VARIANT_FRAME: u32 = 1;
pub const VARIANT_SWITCH_SERVER: u32 = 9;

/// Origin namespace for fixture repos, so a project row is recognisable in a
/// frame without colliding with anything else on screen.
const REPO_NAMESPACE: &str = "flock-fleet-test";

/// One node's declared shape: its fleet identity, the repo its workspace
/// lives in, and the nodes it polls as `[[peers]]`.
#[derive(Debug, Clone)]
pub struct NodeSpec {
    pub name: &'static str,
    /// Repo folder + origin slug. Renders as `<namespace>/<repo>` in a
    /// remote-only project row.
    pub repo: &'static str,
    /// Node names this one polls. The shim resolves them by name.
    pub peers: &'static [&'static str],
}

impl NodeSpec {
    pub const fn new(
        name: &'static str,
        repo: &'static str,
        peers: &'static [&'static str],
    ) -> Self {
        Self { name, repo, peers }
    }
}

/// A spawned node, with everything a test needs to talk to it.
pub struct Node {
    pub name: String,
    pub config_home: PathBuf,
    pub runtime_dir: PathBuf,
    pub api_socket: PathBuf,
    pub client_socket: PathBuf,
    pub repo: PathBuf,
    _master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl Drop for Node {
    fn drop(&mut self) {
        let pid = self.child.process_id();
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Belt and braces: a server that never finished starting can outlive
        // `kill` long enough to hold its runtime dir, and a wedged fixture
        // that leaks servers is worse than a failing test.
        if let Some(pid) = pid {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
        super::unregister_spawned_flock_pid(pid);
    }
}

impl Node {
    /// Attach a protocol client and complete the handshake. Returns the
    /// connected stream, ready to read frames from.
    pub fn attach(&self) -> UnixStream {
        self.attach_sized(90, 30)
    }

    pub fn attach_sized(&self, cols: u16, rows: u16) -> UnixStream {
        let mut stream = UnixStream::connect(&self.client_socket)
            .unwrap_or_else(|e| panic!("client socket for {} should connect: {e}", self.name));
        let (_, error) = super::client_handshake(&mut stream, super::PROTOCOL_VERSION, cols, rows)
            .expect("handshake should complete");
        assert!(error.is_none(), "handshake rejected: {error:?}");
        stream
    }

    /// Create a workspace over the JSON API socket (fresh servers have none).
    pub fn create_workspace(&self, cwd: &Path) -> String {
        let mut stream = UnixStream::connect(&self.api_socket).expect("API socket should connect");
        let request = format!(
            "{{\"id\":\"test:ws\",\"method\":\"workspace.create\",\"params\":{{\"cwd\":\"{}\",\"focus\":true}}}}\n",
            cwd.display()
        );
        stream.write_all(request.as_bytes()).unwrap();
        stream.flush().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut reader = std::io::BufReader::new(stream);
        let mut response = String::new();
        std::io::BufRead::read_line(&mut reader, &mut response).unwrap();
        assert!(
            response.contains("\"result\""),
            "workspace.create failed on {}: {response}",
            self.name
        );
        response
    }

    /// Raw JSON-API call against this node's socket; returns the response line.
    pub fn api(&self, request: &str) -> String {
        let mut stream = UnixStream::connect(&self.api_socket).expect("API socket should connect");
        stream.write_all(request.as_bytes()).unwrap();
        if !request.ends_with('\n') {
            stream.write_all(b"\n").unwrap();
        }
        stream.flush().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut reader = std::io::BufReader::new(stream);
        let mut response = String::new();
        std::io::BufRead::read_line(&mut reader, &mut response).unwrap();
        response
    }
}

/// A spawned fleet. Dropping it kills every node and removes the base dir.
pub struct Fleet {
    pub base: PathBuf,
    pub nodes: Vec<Node>,
}

impl Drop for Fleet {
    fn drop(&mut self) {
        self.nodes.clear();
        super::cleanup_test_base(&self.base);
    }
}

impl Fleet {
    pub fn node(&self, name: &str) -> &Node {
        self.nodes
            .iter()
            .find(|node| node.name == name)
            .unwrap_or_else(|| panic!("no node named {name} in the fleet"))
    }

    /// The `<namespace>/<repo>` identity a node's workspace renders under.
    pub fn project_label(repo: &str) -> String {
        format!("{REPO_NAMESPACE}/{repo}")
    }

    /// A frame-safe needle for that row. The sidebar is ~24 columns wide and
    /// truncates with an ellipsis, so the full label never appears verbatim —
    /// match on the prefix that survives, which still separates `alpha` from
    /// `beta` from `gamma`.
    pub fn project_needle(repo: &str) -> String {
        Self::project_label(repo).chars().take(18).collect()
    }
}

fn unique_base(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!(
        "/tmp/flock-fleet-{tag}-{}-{nanos}",
        std::process::id()
    ))
}

fn init_repo(path: &Path, slug: &str) {
    fs::create_dir_all(path).unwrap();
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(path)
        .status()
        .unwrap();
    assert!(status.success(), "git init failed for {}", path.display());
    let status = std::process::Command::new("git")
        .args([
            "remote",
            "add",
            "origin",
            &format!("git@github.com:{REPO_NAMESPACE}/{slug}.git"),
        ])
        .current_dir(path)
        .status()
        .unwrap();
    assert!(status.success(), "git remote add failed");
}

/// Spawn a fleet from `specs`. Nodes are started in reverse declaration order
/// so a pollee is usually listening before its poller's first round, then every
/// node gets one workspace in its own repo.
pub fn spawn(tag: &str, specs: &[NodeSpec]) -> Fleet {
    let base = unique_base(tag);
    fs::create_dir_all(&base).unwrap();
    let bin_dir = PathBuf::from(env!("CARGO_BIN_EXE_flk"))
        .parent()
        .unwrap()
        .to_path_buf();

    // --- Paths first: the shim must know every node before any node starts.
    struct Paths {
        config_home: PathBuf,
        runtime_dir: PathBuf,
        api_socket: PathBuf,
        client_socket: PathBuf,
        repo: PathBuf,
    }
    let paths: Vec<Paths> = specs
        .iter()
        .map(|spec| Paths {
            config_home: base.join(format!("config-{}", spec.name)),
            runtime_dir: base.join(format!("runtime-{}", spec.name)),
            api_socket: base.join(format!("{}.sock", spec.name)),
            // Derived from FLOCK_SOCKET_PATH: `-client` before `.sock`.
            client_socket: base.join(format!("{}-client.sock", spec.name)),
            repo: base.join(spec.repo),
        })
        .collect();

    // --- The dispatching fake ssh. Real invocations look like
    //     `ssh -o BatchMode=yes … <target> "sh -lc '<cmd>'"`, so the LAST arg
    //     is the command and the one before it is the target.
    let shim_dir = base.join("bin");
    fs::create_dir_all(&shim_dir).unwrap();
    let mut shim = String::from("#!/bin/sh\nprev=\"\"; last=\"\"\nfor a in \"$@\"; do prev=\"$last\"; last=\"$a\"; done\ncase \"$prev\" in\n");
    for (spec, path) in specs.iter().zip(&paths) {
        shim.push_str(&format!(
            "  {}) SOCK='{}'; CFG='{}' ;;\n",
            spec.name,
            path.api_socket.display(),
            path.config_home.display(),
        ));
    }
    shim.push_str(&format!(
        "  *) echo \"fake-ssh: unknown target $prev\" >&2; exit 255 ;;\nesac\nFLOCK_SOCKET_PATH=\"$SOCK\" XDG_CONFIG_HOME=\"$CFG\" PATH='{}':\"$PATH\" exec sh -c \"$last\"\n",
        bin_dir.display(),
    ));
    let shim_path = shim_dir.join("ssh");
    fs::write(&shim_path, shim).unwrap();
    fs::set_permissions(&shim_path, fs::Permissions::from_mode(0o755)).unwrap();

    // --- Configs + repos.
    for (spec, path) in specs.iter().zip(&paths) {
        init_repo(&path.repo, spec.repo);
        let mut config = format!(
            "onboarding = false\nname = \"{}\"\n\n[gossip]\npoll_interval_secs = 1\ninitial_delay_secs = 0\nstale_after_secs = 60\n",
            spec.name
        );
        for peer in spec.peers {
            // ssh_target defaults to the peer name, which is what the shim
            // dispatches on.
            config.push_str(&format!("\n[[peers]]\nname = \"{peer}\"\n"));
        }
        // Debug builds read the flock-dev app dir; release builds read flock.
        for app_dir in ["flock", "flock-dev"] {
            let dir = path.config_home.join(app_dir);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("config.toml"), &config).unwrap();
        }
    }

    // --- Spawn, leaf-first.
    let mut nodes: Vec<Node> = Vec::new();
    for (spec, path) in specs.iter().zip(&paths).rev() {
        fs::create_dir_all(&path.runtime_dir).unwrap();
        register_runtime_dir(&path.runtime_dir);

        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 30,
                cols: 90,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();

        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_flk"));
        cmd.arg("server");
        cmd.cwd(&path.repo);
        cmd.env("XDG_CONFIG_HOME", &path.config_home);
        cmd.env("XDG_RUNTIME_DIR", &path.runtime_dir);
        cmd.env("FLOCK_SOCKET_PATH", &path.api_socket);
        cmd.env_remove("FLOCK_CLIENT_SOCKET_PATH");
        cmd.env("SHELL", "/bin/sh");
        cmd.env_remove("FLOCK_ENV");
        cmd.env("FLOCK_DISABLE_SOUND", "1");
        let outer_path = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{}:{outer_path}", shim_dir.display()));

        let child = pair.slave.spawn_command(cmd).unwrap();
        register_spawned_flock_pid(child.process_id());
        drop(pair.slave);

        // Drain the PTY like a real terminal would. Nobody reads a test
        // node's screen, and a server whose PTY buffer fills up BLOCKS on
        // write — which looks exactly like a server that never finished
        // starting, and wedges the fixture rather than failing it.
        if let Ok(mut reader) = pair.master.try_clone_reader() {
            std::thread::spawn(move || {
                let mut sink = [0u8; 8192];
                while let Ok(n) = std::io::Read::read(&mut reader, &mut sink) {
                    if n == 0 {
                        break;
                    }
                }
            });
        }

        nodes.push(Node {
            name: spec.name.to_string(),
            config_home: path.config_home.clone(),
            runtime_dir: path.runtime_dir.clone(),
            api_socket: path.api_socket.clone(),
            client_socket: path.client_socket.clone(),
            repo: path.repo.clone(),
            _master: pair.master,
            child,
        });
    }
    nodes.reverse();

    // Startup patience: a fleet of real servers coming up next to a parallel
    // test suite is slower than a single one, and the failure mode of being
    // too impatient here is a confusing panic in an unrelated assertion.
    for node in &nodes {
        wait_for_socket(&node.api_socket, Duration::from_secs(30));
        node.create_workspace(&node.repo.clone());
        wait_for_file(&node.client_socket, Duration::from_secs(30));
    }

    Fleet { base, nodes }
}

// ---------------------------------------------------------------------------
// Shared fleet: build once per test binary, reuse across read-only cases.
// ---------------------------------------------------------------------------

/// A/B/C chain: `nodea` polls `nodeb`, `nodeb` polls `nodec`. `nodec` is
/// reachable from `nodea` only by relay — the two-hop case the gossip
/// visibility rules have to cover.
pub const CHAIN_ABC: &[NodeSpec] = &[
    NodeSpec::new("nodea", "alpha", &["nodeb"]),
    NodeSpec::new("nodeb", "beta", &["nodec"]),
    NodeSpec::new("nodec", "gamma", &[]),
];

static SHARED: OnceLock<Mutex<Option<Fleet>>> = OnceLock::new();

/// Run `body` against a per-binary shared fleet, built on first use. A `Node`
/// owns a PTY master (`Send` but not `Sync`), so the fixture is handed out
/// under the lock rather than as a `&'static` — which also serialises the cases
/// that use it, keeping one test's keystrokes out of another's frame stream.
///
/// Use for read-only assertions. Anything that mutates fleet state (focus,
/// filters, workspace creation) should [`spawn`] its own.
pub fn with_shared<R>(tag: &str, specs: &[NodeSpec], body: impl FnOnce(&Fleet) -> R) -> R {
    let cell = SHARED.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let fleet = guard.get_or_insert_with(|| spawn(tag, specs));
    body(fleet)
}

// ---------------------------------------------------------------------------
// Frames as screenshots
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct FrameWire {
    pub cells: Vec<CellWire>,
    pub width: u16,
    pub height: u16,
    cursor: Option<CursorWire>,
    hyperlinks: Vec<String>,
    graphics: Vec<u8>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct CellWire {
    pub symbol: String,
    fg: u32,
    bg: u32,
    modifier: u16,
    skip: bool,
    hyperlink: Option<u32>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct CursorWire {
    x: u16,
    y: u16,
    visible: bool,
    shape: u8,
}

pub fn decode_frame_payload(payload: &[u8]) -> Option<FrameWire> {
    bincode::serde::decode_from_slice(payload, bincode::config::standard())
        .ok()
        .map(|(frame, _): (FrameWire, usize)| frame)
}

pub fn frame_rows(frame: &FrameWire) -> Vec<String> {
    let width = frame.width.max(1) as usize;
    frame
        .cells
        .chunks(width)
        .map(|row| row.iter().map(|cell| cell.symbol.as_str()).collect())
        .collect()
}

/// Read frames until one contains `needle`; return its 0-based row index.
/// The error carries the last screen, so a failure reads like a screenshot.
pub fn wait_for_row(
    stream: &mut UnixStream,
    needle: &str,
    timeout: Duration,
) -> Result<usize, String> {
    wait_for_screen_matching(stream, &[needle], timeout)
        .map(|rows| rows.iter().position(|row| row.contains(needle)).unwrap())
}

/// Read frames until ONE frame satisfies every needle; return its rows.
/// (Sequential single-needle waits consume frames between checks and stall
/// when the server has no reason to re-render.)
pub fn wait_for_screen_matching(
    stream: &mut UnixStream,
    needles: &[&str],
    timeout: Duration,
) -> Result<Vec<String>, String> {
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    let mut last_screen = String::new();
    while Instant::now() < deadline {
        match read_server_message(stream) {
            Ok((VARIANT_FRAME, payload)) => {
                if let Some(frame) = decode_frame_payload(&payload) {
                    let rows = frame_rows(&frame);
                    last_screen = rows.join("\n");
                    if needles
                        .iter()
                        .all(|needle| rows.iter().any(|row| row.contains(needle)))
                    {
                        return Ok(rows);
                    }
                }
            }
            Ok(_) => continue,
            Err(_) => continue,
        }
    }
    Err(format!(
        "timed out waiting for {needles:?} in one frame; last screen:\n{last_screen}"
    ))
}

/// The most recent frame drained within `settle`, as rows. Use for negative
/// assertions ("this must NOT be on screen") where waiting proves nothing.
pub fn latest_screen(stream: &mut UnixStream, settle: Duration) -> Vec<String> {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let deadline = Instant::now() + settle;
    let mut rows = Vec::new();
    while Instant::now() < deadline {
        if let Ok((VARIANT_FRAME, payload)) = read_server_message(stream) {
            if let Some(frame) = decode_frame_payload(&payload) {
                rows = frame_rows(&frame);
            }
        }
    }
    rows
}

/// Wait for a SwitchServer and return `(ssh_target, trailing bytes)` — the
/// trailing bytes are `fleet`, `focus_workspace`, `proxy_jump` in wire order.
pub fn wait_for_switch_server(
    stream: &mut UnixStream,
    timeout: Duration,
) -> Result<(String, Vec<u8>), String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match read_server_message(stream) {
            Ok((VARIANT_SWITCH_SERVER, payload)) => {
                let (len, offset) = super::decode_varint_u32(&payload, 0)?;
                let end = offset + len as usize;
                let bytes = payload
                    .get(offset..end)
                    .ok_or_else(|| "truncated SwitchServer payload".to_string())?;
                let target = String::from_utf8(bytes.to_vec()).map_err(|e| e.to_string())?;
                return Ok((target, payload[end..].to_vec()));
            }
            Ok(_) => continue,
            Err(_) => continue,
        }
    }
    Err("timed out waiting for SwitchServer".into())
}

/// Send `ClientMessage::FocusWorkspace { workspace_id }` (#80). The
/// variant is index 8 — appended after `SetFrameSubscription` (7).
pub fn send_focus_workspace(stream: &mut UnixStream, workspace_id: &str) {
    let mut buf = super::encode_varint_u32(8);
    buf.extend(super::encode_varint_u32(workspace_id.len() as u32));
    buf.extend_from_slice(workspace_id.as_bytes());
    let framed = super::frame_message(&buf);
    stream
        .write_all(&framed)
        .expect("write focus-workspace should send");
    stream.flush().expect("flush focus-workspace");
}

/// The `focus_workspace` field of a captured `SwitchServer` tail. The tail is
/// `fleet: Option<FleetSnapshot>`, `focus_workspace: Option<String>`,
/// `proxy_jump: Option<String>` — so strip the last option, then read the next.
pub fn switch_focus_workspace(tail: &[u8]) -> Option<String> {
    let (_proxy_jump, rest) = trailing_option_string(tail);
    let (focus, _) = trailing_option_string(rest);
    focus
}

/// Split one trailing bincode `Option<String>` off `tail`: `None` is a single
/// `0x00`; `Some` is `0x01`, a one-byte length varint (ids are short), then the
/// utf8 bytes.
///
/// KNOWN LIMIT: `Some("")` encodes as `0x01 0x00` and is read here as `None` —
/// a trailing `0x00` is taken as the None tag without looking further back,
/// because in general the preceding byte is indistinguishable from payload.
/// Safe for the fields this parses: a workspace id is never empty (the emit
/// side filters blank ids out), and a ProxyJump identity never is either.
/// Anything else that can legitimately be an empty string needs a real decode,
/// not this.
fn trailing_option_string(tail: &[u8]) -> (Option<String>, &[u8]) {
    if tail.last() == Some(&0) {
        return (None, &tail[..tail.len() - 1]);
    }
    for n in 3..=tail.len().min(48) {
        let suffix = &tail[tail.len() - n..];
        if suffix[0] == 1 && suffix[1] as usize == n - 2 {
            if let Ok(text) = std::str::from_utf8(&suffix[2..]) {
                return (Some(text.to_string()), &tail[..tail.len() - n]);
            }
        }
    }
    (None, tail)
}

/// Workspace ids on a node, in `workspace.list` order.
pub fn workspace_ids(node: &Node) -> Vec<String> {
    let response = node.api(r#"{"id":"test:ls","method":"workspace.list","params":{}}"#);
    let value: serde_json::Value = serde_json::from_str(&response)
        .unwrap_or_else(|e| panic!("workspace.list should parse ({e}): {response}"));
    value["result"]["workspaces"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|ws| ws["workspace_id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The id of the space whose label contains `needle`, for asserting that a
/// switch named the space that was actually clicked rather than merely some
/// space.
pub fn workspace_id_by_label(node: &Node, needle: &str) -> String {
    let response = node.api(r#"{"id":"test:ls","method":"workspace.list","params":{}}"#);
    let value: serde_json::Value = serde_json::from_str(&response)
        .unwrap_or_else(|e| panic!("workspace.list should parse ({e}): {response}"));
    let rows = value["result"]["workspaces"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    rows.iter()
        .find(|ws| {
            ws["label"]
                .as_str()
                .is_some_and(|label| label.contains(needle))
        })
        .and_then(|ws| ws["workspace_id"].as_str().map(str::to_string))
        .unwrap_or_else(|| {
            panic!(
                "no space labelled like {needle:?} on {}: {response}",
                node.name
            )
        })
}

/// Whether `workspace_id` is the focused space on `node`.
pub fn workspace_focused(node: &Node, workspace_id: &str) -> bool {
    let response = node.api(r#"{"id":"test:ls","method":"workspace.list","params":{}}"#);
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&response) else {
        return false;
    };
    value["result"]["workspaces"]
        .as_array()
        .map(|rows| {
            rows.iter().any(|ws| {
                ws["workspace_id"].as_str() == Some(workspace_id)
                    && ws["focused"].as_bool() == Some(true)
            })
        })
        .unwrap_or(false)
}

pub fn wait_until_focused(node: &Node, workspace_id: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if workspace_focused(node, workspace_id) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Click a rendered row with an SGR mouse press+release at `col` (0-based).
pub fn click_row(stream: &mut UnixStream, row: usize, col: u16) {
    let sgr_row = (row as u16) + 1;
    let sgr_col = col + 1;
    super::send_input(stream, format!("\x1b[<0;{sgr_col};{sgr_row}M").as_bytes())
        .expect("mouse press should send");
    super::send_input(stream, format!("\x1b[<0;{sgr_col};{sgr_row}m").as_bytes())
        .expect("mouse release should send");
}
