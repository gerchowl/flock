//! Multi-node fleet fixture: spawn N real `flk` servers on one machine, wire
//! them into an arbitrary gossip topology over a dispatching fake-`ssh` shim,
//! and read their rendered frames like screenshots.
//!
//! This is the "real network setup" vehicle without containers: every node is
//! the actual binary with its own HOME, XDG directories, API socket and
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

use super::{read_server_message, register_runtime_dir, register_spawned_flock_pid, wait_for_file};

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
    /// Extra TOML appended to this node's config, for a case that needs a
    /// setting the shared fixture does not carry.
    pub extra_config: &'static str,
    pub mesh: MeshMode,
    pub push_concurrency: Option<usize>,
}

impl NodeSpec {
    pub const fn new(
        name: &'static str,
        repo: &'static str,
        peers: &'static [&'static str],
    ) -> Self {
        Self {
            name,
            repo,
            peers,
            extra_config: "",
            mesh: MeshMode::Native,
            push_concurrency: None,
        }
    }

    pub const fn with_push_concurrency(mut self, limit: usize) -> Self {
        self.push_concurrency = Some(limit);
        self
    }

    pub const fn with_mesh(mut self, mesh: MeshMode) -> Self {
        self.mesh = mesh;
        self
    }

    pub const fn with_config(mut self, extra_config: &'static str) -> Self {
        self.extra_config = extra_config;
        self
    }
}

/// Native runs the real handshake. Fault modes refuse mesh negotiation or
/// corrupt a possession proof while using the same real relay process.
#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MeshMode {
    Native,
    Disabled,
    ForgedSignature,
    ForgedChallenge,
    RelayReset,
    LegacyDialer,
    VersionMismatch(u32),
}

/// A spawned node, with everything a test needs to talk to it.
pub struct Node {
    pub name: String,
    pub home: PathBuf,
    pub config_home: PathBuf,
    pub runtime_dir: PathBuf,
    pub api_socket: PathBuf,
    pub client_socket: PathBuf,
    pub repo: PathBuf,
    shim_dir: PathBuf,
    mesh: MeshMode,
    push_concurrency: Option<usize>,
    _master: Option<Box<dyn MasterPty + Send>>,
    child: Option<Box<dyn Child + Send + Sync>>,
}

impl Drop for Node {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Node {
    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let pid = child.process_id();
            let group = pid.expect("fleet node pid") as i32;
            // Edges deliberately own separate groups, so stop them while the
            // server can still reap its ssh children, then stop the server.
            kill_edges(
                self.shim_dir.parent().unwrap(),
                Some(&format!("{}-", self.name)),
            );
            unsafe {
                libc::kill(-group, libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_millis(400);
            while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
                std::thread::yield_now();
            }
            // The leader may already be gone while another group member lives.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
            let _ = child.wait();
            kill_edges(
                self.shim_dir.parent().unwrap(),
                Some(&format!("{}-", self.name)),
            );
            super::unregister_spawned_flock_pid(pid);
        }
        self._master = None;
    }

    pub fn process_id(&self) -> u32 {
        self.child
            .as_ref()
            .and_then(|child| child.process_id())
            .expect("running node")
    }

    /// Restart only this sandbox server, preserving its config and state.
    pub fn restart(&mut self) {
        self.stop();
        // A hard stop can leave socket files behind. Readiness must observe
        // the replacement listener, never a stale filesystem entry.
        for socket in [&self.api_socket, &self.client_socket] {
            let _ = fs::remove_file(socket);
        }
        self.start();
        self.wait_ready();
    }

    /// Restart a sandbox as a different mesh protocol version.
    pub fn restart_with_mesh(&mut self, mesh: MeshMode) {
        self.mesh = mesh;
        self.restart();
    }

    fn start(&mut self) {
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
        cmd.cwd(&self.repo);
        cmd.env_clear();
        for (key, value) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if matches!(name.as_ref(), "PATH" | "TMPDIR" | "USER" | "LANG")
                || name.starts_with("LC_")
            {
                cmd.env(key, value);
            }
        }
        for (key, value) in super::environment::isolated_env(&self.config_home, &self.runtime_dir) {
            cmd.env(key, value);
        }
        cmd.env("HOME", &self.home);
        cmd.env("XDG_DATA_HOME", self.home.join("data"));
        cmd.env("XDG_STATE_HOME", self.home.join("state"));
        cmd.env("XDG_CACHE_HOME", self.home.join("cache"));
        cmd.env("FLOCK_SOCKET_PATH", &self.api_socket);
        cmd.env("SHELL", "/bin/sh");
        cmd.env("FLOCK_DISABLE_SOUND", "1");
        // Debug-only substitute for sshd ancestry in the local ssh fixture.
        cmd.env("FLOCK_TEST_RELAY_ANCESTOR", "flk");
        cmd.env("FLOCK_FLEET_SOURCE", &self.name);
        if let Some(limit) = self.push_concurrency {
            cmd.env("FLOCK_TEST_MESH_PUSH_CONCURRENCY", limit.to_string());
        }
        if let MeshMode::VersionMismatch(version) = self.mesh {
            cmd.env("FLOCK_TEST_MESH_VERSION", version.to_string());
        }
        let outer_path = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{}:{outer_path}", self.shim_dir.display()));
        super::environment::assert_pty_isolated(&cmd);
        let child = super::environment::spawn_pty(pair.slave.as_ref(), cmd).unwrap();
        let pid = child.process_id().expect("fleet node pid") as i32;
        assert_eq!(
            unsafe { libc::getpgid(pid) },
            pid,
            "node owns its process group"
        );
        register_spawned_flock_pid(child.process_id());
        drop(pair.slave);
        // Drain output so a full PTY cannot block a headless fixture server.
        let mut reader = pair.master.try_clone_reader().unwrap();
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut reader, &mut std::io::sink());
        });
        self.child = Some(child);
        self._master = Some(pair.master);
    }

    fn wait_ready(&self) {
        wait_until("server ping", Duration::from_secs(30), || {
            let mut stream = UnixStream::connect(&self.api_socket).ok()?;
            stream
                .set_read_timeout(Some(Duration::from_millis(200)))
                .ok()?;
            stream
                .set_write_timeout(Some(Duration::from_millis(200)))
                .ok()?;
            stream
                .write_all(b"{\"id\":\"ready\",\"method\":\"ping\",\"params\":{}}\n")
                .ok()?;
            let mut response = String::new();
            std::io::BufRead::read_line(&mut std::io::BufReader::new(stream), &mut response)
                .ok()?;
            let value: serde_json::Value = serde_json::from_str(&response).ok()?;
            value.get("result").map(|_| ())
        });
        wait_for_file(&self.client_socket, Duration::from_secs(30));
    }

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

/// Hold mesh message deliveries on one edge while other traffic proceeds.
/// Dropping the gate also releases it, including when a test assertion fails.
pub struct MessageGate {
    path: PathBuf,
}

impl MessageGate {
    pub fn wait_entered(&self, timeout: Duration) {
        wait_until("SSH message command to enter gate", timeout, || {
            self.path.join("entered").exists().then_some(())
        });
    }

    pub fn release(&self) {
        fs::write(self.path.join("release"), b"").unwrap();
    }
}

impl Drop for MessageGate {
    fn drop(&mut self) {
        let _ = fs::write(self.path.join("release"), b"");
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
        self.kill_edges(None);
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

    pub fn node_mut(&mut self, name: &str) -> &mut Node {
        self.nodes
            .iter_mut()
            .find(|node| node.name == name)
            .unwrap_or_else(|| panic!("no node named {name} in the fleet"))
    }

    /// Block new dials along one directed edge, leaving the reverse untouched.
    pub fn refuse_edge(&self, from: &str, to: &str) {
        self.node(from);
        self.node(to);
        fs::write(self.base.join(format!("refuse-edge-{from}-{to}")), b"").unwrap();
    }

    pub fn allow_edge(&self, from: &str, to: &str) {
        fs::remove_file(self.base.join(format!("refuse-edge-{from}-{to}"))).unwrap();
    }

    /// Arm a message-only gate once per directed edge. The shim acknowledges
    /// entry before waiting, so tests can probe responsiveness without sleeps.
    pub fn gate_message_edge(&self, from: &str, to: &str) -> MessageGate {
        self.node(from);
        self.node(to);
        let path = self.base.join(format!("gate-message-{from}-{to}"));
        fs::create_dir(&path).unwrap();
        MessageGate { path }
    }

    /// Wait until the shim has spawned the held relay, before partitioning it.
    pub fn wait_for_edge(&self, from: &str, to: &str, timeout: Duration) {
        wait_until("held edge", timeout, || {
            (!self.edge_pids(Some(&format!("{from}-{to}-"))).is_empty()).then_some(())
        });
    }

    /// Wait for a held ssh child, then cut it and its relay subprocesses.
    /// Refuse the edge first when a test needs the partition to persist.
    pub fn kill_edge(&self, from: &str, to: &str, timeout: Duration) -> usize {
        wait_until("held edge to kill", timeout, || {
            let count = self.kill_edges(Some(&format!("{from}-{to}-")));
            (count > 0).then_some(count)
        })
    }

    pub fn edge_pids(&self, prefix: Option<&str>) -> Vec<(PathBuf, i32)> {
        edge_pids(&self.base, prefix)
    }

    fn kill_edges(&self, prefix: Option<&str>) -> usize {
        kill_edges(&self.base, prefix)
    }

    pub fn node_id(&self, name: &str) -> String {
        node_id(self.node(name))
    }

    pub fn wait_route(&self, node: &str, target: &str, present: bool) {
        let target_id = self.node_id(target);
        wait_until("mesh route status", Duration::from_secs(30), || {
            let response: serde_json::Value = serde_json::from_str(
                &self
                    .node(node)
                    .api(r#"{"id":"route","method":"peers.enrollment","params":{}}"#),
            )
            .unwrap();
            let routes = response["result"]["routes"]
                .as_array()
                .expect("wait_route requires slice 2e's peers.enrollment routes status");
            (routes.iter().any(|route| route["node"] == target_id) == present).then_some(())
        });
    }

    pub fn set_allow_from(&self, node: &str, list: &[&str]) {
        let node = self.node(node);
        for app in ["flock", "flock-dev"] {
            let path = node.config_home.join(app).join("config.toml");
            let mut config: toml::Value =
                toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            let table = config
                .as_table_mut()
                .unwrap()
                .entry("msg")
                .or_insert_with(|| toml::Value::Table(Default::default()));
            table.as_table_mut().unwrap().insert(
                "allow_from".into(),
                toml::Value::Array(
                    list.iter()
                        .map(|s| toml::Value::String((*s).into()))
                        .collect(),
                ),
            );
            fs::write(path, toml::to_string(&config).unwrap()).unwrap();
        }
        let response: serde_json::Value = serde_json::from_str(
            &node.api(r#"{"id":"reload","method":"server.reload_config","params":{}}"#),
        )
        .unwrap();
        assert!(
            response.get("result").is_some(),
            "reload failed: {response}"
        );
    }

    /// Make every new ssh dial to `name` fail with "Connection refused", as a
    /// broken edge would (#410). Held connections are unaffected.
    pub fn refuse_ssh_to(&self, name: &str) {
        fs::write(self.base.join(format!("refuse-ssh-{name}")), b"").unwrap();
    }

    pub fn allow_ssh_to(&self, name: &str) {
        fs::remove_file(self.base.join(format!("refuse-ssh-{name}"))).unwrap();
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
        .args(["-c", "init.defaultBranch=main", "init", "-q"])
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
    spawn_with_startup_probe(tag, specs, |_, _| {})
}

/// Observe startup before a named node starts, to exercise transport readiness races.
pub fn spawn_with_startup_probe(
    tag: &str,
    specs: &[NodeSpec],
    mut before_start: impl FnMut(&Fleet, &str),
) -> Fleet {
    let base = unique_base(tag);
    fs::create_dir_all(&base).unwrap();
    let bin_dir = PathBuf::from(env!("CARGO_BIN_EXE_flk"))
        .parent()
        .unwrap()
        .to_path_buf();

    // --- Paths first: the shim must know every node before any node starts.
    struct Paths {
        home: PathBuf,
        config_home: PathBuf,
        runtime_dir: PathBuf,
        api_socket: PathBuf,
        client_socket: PathBuf,
        repo: PathBuf,
    }
    let paths: Vec<Paths> = specs
        .iter()
        .map(|spec| Paths {
            home: base.join(format!("home-{}", spec.name)),
            config_home: base.join(format!("config-{}", spec.name)),
            runtime_dir: base.join(format!("runtime-{}", spec.name)),
            api_socket: base.join(format!("{}.sock", spec.name)),
            // Derived from FLOCK_SOCKET_PATH: `-client` before `.sock`.
            client_socket: base.join(format!("{}-client.sock", spec.name)),
            repo: base.join(spec.repo),
        })
        .collect();

    // The shim owns a process group per held edge and records its pid. Its
    // dispatch table contains only this fixture's sockets and sandbox paths.
    let shim_dir = base.join("bin");
    fs::create_dir_all(&shim_dir).unwrap();
    fs::create_dir_all(base.join("edges")).unwrap();
    let manifest: serde_json::Map<String, serde_json::Value> = specs
        .iter()
        .zip(&paths)
        .map(|(spec, path)| {
            (
                spec.name.to_string(),
                serde_json::json!({
                    "home": path.home, "config": path.config_home, "runtime": path.runtime_dir,
                    "socket": path.api_socket, "mesh": spec.mesh,
                }),
            )
        })
        .collect();
    fs::write(
        base.join("nodes.json"),
        serde_json::to_vec(&serde_json::json!({
            "nodes": manifest, "bin": bin_dir,
        }))
        .unwrap(),
    )
    .unwrap();
    let shim_path = shim_dir.join("ssh");
    fs::write(&shim_path, include_str!("fleet_ssh.py")).unwrap();
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
        config.push_str(spec.extra_config);
        // Debug builds read the flock-dev app dir; release builds read flock.
        for app_dir in ["flock", "flock-dev"] {
            let dir = path.config_home.join(app_dir);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("config.toml"), &config).unwrap();
        }
    }

    // Construct the owner before starting children so startup failures clean up.
    let nodes = specs
        .iter()
        .zip(&paths)
        .map(|(spec, path)| {
            for dir in [&path.home, &path.config_home, &path.runtime_dir] {
                fs::create_dir_all(dir).unwrap();
            }
            // Remote commands use a login shell. System profiles can reset
            // PATH, so the sandbox supplies its own path to the test binary.
            let fixture_path = format!(
                "{}:{}:{}",
                bin_dir.display(),
                shim_dir.display(),
                std::env::var("PATH").unwrap_or_default()
            );
            fs::write(
                path.home.join(".profile"),
                format!("export PATH='{}'\n", fixture_path.replace('\'', "'\\''")),
            )
            .unwrap();
            register_runtime_dir(&path.runtime_dir);
            Node {
                name: spec.name.to_string(),
                home: path.home.clone(),
                config_home: path.config_home.clone(),
                runtime_dir: path.runtime_dir.clone(),
                api_socket: path.api_socket.clone(),
                client_socket: path.client_socket.clone(),
                repo: path.repo.clone(),
                shim_dir: shim_dir.clone(),
                mesh: spec.mesh,
                push_concurrency: spec.push_concurrency,
                _master: None,
                child: None,
            }
        })
        .collect();
    let mut fleet = Fleet { base, nodes };
    for index in (0..fleet.nodes.len()).rev() {
        before_start(&fleet, &fleet.nodes[index].name);
        let node = &mut fleet.nodes[index];
        node.start();
        node.wait_ready();
        // Initial startup only: later outages must still reach the real relay.
        fs::write(fleet.base.join(format!("ready-{}", node.name)), b"").unwrap();
        node.create_workspace(&node.repo);
    }
    fleet
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

/// Laptop and edge-less spoke reached by one hub. The wide laptop sidebar
/// keeps `via nodeb` routing evidence visible in the MCP regression.
pub const HUB_SPOKES: &[NodeSpec] = &[
    NodeSpec::new("nodea", "alpha", &[]).with_config("\n[ui]\nsidebar_width = 44\n"),
    NodeSpec::new("nodeb", "beta", &["nodea", "nodec"]),
    NodeSpec::new("nodec", "gamma", &[]),
];

/// Laptop dials a hub that dials an edge-less spoke.
pub const LAPTOP_HUB_SPOKE: &[NodeSpec] = CHAIN_ABC;

/// Two independent hubs dial the same spoke using multi-edge enrollment.
pub const TWO_HUBS: &[NodeSpec] = &[
    NodeSpec::new("nodea", "alpha", &["nodec"]),
    NodeSpec::new("nodeb", "beta", &["nodec"]),
    NodeSpec::new("nodec", "gamma", &[]),
];

/// A dials B and C, both of which dial D: equal-length paths and a cycle.
pub const MESH_DIAMOND: &[NodeSpec] = &[
    NodeSpec::new("nodea", "alpha", &["nodeb", "nodec"]),
    NodeSpec::new("nodeb", "beta", &["noded.example"]),
    NodeSpec::new("nodec", "gamma", &["noded.example"]),
    NodeSpec::new("noded.example", "delta", &[]),
];

/// Laptop A dials hubs B and C, which both dial the edge-less spoke D.
pub const LAPTOP_TWO_HUBS: &[NodeSpec] = MESH_DIAMOND;

/// Only A can dial B. Traffic in either direction uses that held edge.
pub const ONE_WAY_PAIR: &[NodeSpec] = &[
    NodeSpec::new("nodea", "alpha", &["nodeb"]),
    NodeSpec::new("nodeb", "beta", &[]),
];

/// Poll an observable condition up to a deadline. Each probe must itself be
/// bounded (socket probes should set read/write timeouts).
pub fn wait_until<T>(what: &str, timeout: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "timed out waiting for {what}");
        std::thread::sleep(left.min(Duration::from_millis(25)));
    }
}

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

/// Like [`wait_for_row`], but for a row whose index can still move. Waits for
/// one frame holding every needle in `ready`, then keeps draining until a
/// quiet window passes with no frame (or the screen stops changing), and
/// returns where `row_needle` sits on the latest screen. Clicking an index
/// taken from the first frame that merely contains the row races any later
/// re-layout above it, and the click then lands on the wrong row.
pub fn wait_for_settled_row(
    stream: &mut UnixStream,
    ready: &[&str],
    row_needle: &str,
    timeout: Duration,
) -> Result<usize, String> {
    let mut rows = wait_for_screen_matching(stream, ready, timeout)?;
    for _ in 0..20 {
        let next = latest_screen(stream, Duration::from_millis(500));
        if next.is_empty() || next == rows {
            break;
        }
        rows = next;
    }
    rows.iter()
        .position(|row| row.contains(row_needle))
        .ok_or_else(|| {
            format!(
                "{row_needle:?} left the screen while settling:\n{}",
                rows.join("\n")
            )
        })
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
    // What arrived instead, so a timeout reads like a screenshot of what the
    // server did with the click rather than a bare "timed out".
    let mut variants: Vec<u32> = Vec::new();
    let mut last_screen = String::new();
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
            Ok((VARIANT_FRAME, payload)) => {
                variants.push(VARIANT_FRAME);
                if let Some(frame) = decode_frame_payload(&payload) {
                    last_screen = frame_rows(&frame).join("\n");
                }
            }
            Ok((variant, _)) => variants.push(variant),
            Err(_) => continue,
        }
    }
    Err(format!(
        "timed out waiting for SwitchServer; {} messages seen (variants {variants:?}); last screen:\n{last_screen}",
        variants.len()
    ))
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

#[derive(serde::Deserialize)]
struct EdgeProcesses {
    shim: i32,
    relay: i32,
}

fn edge_records(base: &Path, prefix: Option<&str>) -> Vec<(PathBuf, EdgeProcesses)> {
    let mut live = Vec::new();
    for entry in fs::read_dir(base.join("edges"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".pending") || prefix.is_some_and(|p| !name.starts_with(p)) {
            continue;
        }
        let record = fs::read(entry.path())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<EdgeProcesses>(&bytes).ok());
        if let Some(record) = record.filter(|r| r.shim > 1 && r.relay > 1) {
            // PeerStream can SIGKILL the shim before its finally block runs.
            // The relay's unique environment marker still proves ownership.
            if shim_is_ours(base, record.shim) || relay_is_ours(&entry.path(), record.relay) {
                live.push((entry.path(), record));
                continue;
            }
        }
        let _ = fs::remove_file(entry.path());
    }
    live
}

fn edge_pids(base: &Path, prefix: Option<&str>) -> Vec<(PathBuf, i32)> {
    edge_records(base, prefix)
        .into_iter()
        .map(|(path, record)| (path, record.shim))
        .collect()
}

pub fn group_members(group: i32) -> Vec<u32> {
    super::process_table::list_process_ids()
        .unwrap()
        .into_iter()
        .filter(|pid| unsafe { libc::getpgid(*pid as i32) } == group)
        .collect()
}

fn shim_is_ours(base: &Path, pid: i32) -> bool {
    let script = base.join("bin/ssh");
    super::process_table::process_info(pid as u32)
        .is_ok_and(|info| info.argv.iter().any(|arg| Path::new(arg) == script))
}

fn relay_is_ours(path: &Path, group: i32) -> bool {
    let marker = format!("FLOCK_FLEET_EDGE={}", path.display());
    group_members(group).iter().any(|pid| {
        super::process_table::process_environment(*pid)
            .is_ok_and(|vars| vars.iter().any(|v| v == &marker))
    })
}

fn kill_edges(base: &Path, prefix: Option<&str>) -> usize {
    let edges = edge_records(base, prefix);
    for (path, edge) in &edges {
        // An orphan's record does not authorize signaling a reused shim pid.
        // When alive, the shim handles TERM and reaps the relay itself.
        if shim_is_ours(base, edge.shim) {
            unsafe {
                libc::kill(edge.shim, libc::SIGTERM);
            }
        } else if relay_is_ours(path, edge.relay) {
            unsafe {
                libc::kill(-edge.relay, libc::SIGTERM);
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while edges
        .iter()
        .any(|(path, edge)| shim_is_ours(base, edge.shim) || relay_is_ours(path, edge.relay))
        && Instant::now() < deadline
    {
        std::thread::yield_now();
    }
    for (path, edge) in &edges {
        if relay_is_ours(path, edge.relay) {
            unsafe {
                libc::kill(-edge.relay, libc::SIGKILL);
            }
        }
        if shim_is_ours(base, edge.shim) {
            unsafe {
                libc::kill(-edge.shim, libc::SIGKILL);
            }
        }
        let _ = fs::remove_file(path);
    }
    edges.len()
}

pub fn node_id(node: &Node) -> String {
    let response: serde_json::Value =
        serde_json::from_str(&node.api(r#"{"id":"identity","method":"ping","params":{}}"#))
            .unwrap();
    response["result"]["capabilities"]["node_id"]
        .as_str()
        .expect("node identity")
        .to_owned()
}
