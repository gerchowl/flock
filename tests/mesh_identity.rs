//! Identity evidence starts at real server boot and ends at both public APIs.

mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use support::environment::Command;

use serde_json::{json, Value};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("mi-{}-{nonce:x}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        Self(base)
    }

    fn command(&self, session: &str, state: &Path) -> Command {
        let config = self.0.join("config");
        for app_dir in ["flock", "flock-dev"] {
            fs::create_dir_all(config.join(app_dir).join("sessions").join(session)).unwrap();
            fs::write(
                config.join(app_dir).join("config.toml"),
                "onboarding = false\n[checks]\nenable = false\n",
            )
            .unwrap();
        }
        let runtime = self.0.join("run");
        fs::create_dir_all(&runtime).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_flk"));
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .envs(support::environment::isolated_env(&config, &runtime))
            .env("HOME", &self.0)
            .env("XDG_STATE_HOME", state)
            .env("FLOCK_SOCKET_PATH", self.0.join(format!("{session}.sock")))
            .env(
                "FLOCK_CLIENT_SOCKET_PATH",
                self.0.join(format!("{session}-c.sock")),
            )
            .env("FLOCK_HOST_NAME", "node.example")
            .env("SHELL", "/bin/sh")
            .env("FLOCK_SESSION", session)
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        support::environment::assert_command_isolated(&cmd);
        cmd
    }

    fn start(&self, session: &str, state: &Path) -> Server {
        self.start_command(session, self.command(session, state))
    }

    fn start_command(&self, session: &str, mut cmd: Command) -> Server {
        support::environment::assert_command_isolated(&cmd);
        let child = cmd.arg("server").stderr(Stdio::null()).spawn().unwrap();
        support::register_spawned_flock_pid(Some(child.id()));
        let mut server = Server {
            child,
            socket: self.0.join(format!("{session}.sock")),
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        // Binding creates the path before the server repairs a restrictive umask.
        while UnixStream::connect(&server.socket).is_err() {
            assert!(
                server.child.try_wait().unwrap().is_none(),
                "server exited before binding API"
            );
            assert!(Instant::now() < deadline, "server startup timed out");
            std::thread::yield_now();
        }
        server
    }

    fn identity_path(&self) -> PathBuf {
        let app = if cfg!(debug_assertions) {
            "flock-dev"
        } else {
            "flock"
        };
        self.0.join("state").join(app).join("mesh/identity.json")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Server {
    child: Child,
    socket: PathBuf,
}

impl Server {
    fn request(&self, method: &str) -> Value {
        let mut stream = UnixStream::connect(&self.socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        writeln!(
            stream,
            "{}",
            json!({"id":"identity", "method":method, "params":{}})
        )
        .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        serde_json::from_str::<Value>(&line).unwrap()["result"].clone()
    }

    fn identity(&self) -> String {
        let pong = self.request("ping");
        let id = pong["capabilities"]["node_id"]
            .as_str()
            .expect("Pong node id")
            .to_string();
        assert_eq!(id.len(), 64);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(self.request("peers.summary")["node_id"], id);
        id
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let pid = self.child.id();
        let _ = self.child.kill();
        let _ = self.child.wait();
        support::unregister_spawned_flock_pid(Some(pid));
        let _ = fs::remove_file(&self.socket);
    }
}

#[test]
fn mesh_identity_survives_server_restart_and_is_shared_by_named_sessions() {
    let fixture = Fixture::new();
    let state = fixture.0.join("state");
    let first = fixture.start("first", &state);
    let id = first.identity();
    let second = fixture.start("second", &state);
    assert_eq!(second.identity(), id);
    drop(first);
    let restarted = fixture.start("first", &state);
    assert_eq!(restarted.identity(), id);
    assert_eq!(
        fs::metadata(fixture.identity_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let independent = fixture.start("independent", &fixture.0.join("other-state"));
    assert_ne!(independent.identity(), id);
}

#[test]
fn mesh_identity_boot_refuses_corrupt_file_with_its_path() {
    let fixture = Fixture::new();
    let state = fixture.0.join("state");
    drop(fixture.start("first", &state));
    let path = fixture.identity_path();
    fs::write(&path, b"corrupt identity").unwrap();
    let output = fixture
        .command("refused", &state)
        .arg("server")
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains(&path.display().to_string()));
    assert!(!fixture.0.join("refused.sock").exists());
}

#[test]
fn mesh_identity_strict_umask_still_creates_private_writable_files() {
    let fixture = Fixture::new();
    let state = fixture.0.join("state");
    let mut cmd = fixture.command("strict", &state);
    // SAFETY: umask is async-signal-safe and changes only the forked child.
    unsafe {
        cmd.pre_exec(|| {
            libc::umask(0o277);
            Ok(())
        });
    }
    let first = fixture.start_command("strict", cmd);
    let id = first.identity();
    drop(first);
    let path = fixture.identity_path();
    for file in [&path, &path.with_file_name("identity.lock")] {
        assert_eq!(
            fs::metadata(file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert_eq!(fixture.start("reopened", &state).identity(), id);
}

#[test]
fn mesh_identity_unbound_warning_reaches_ping_summary_and_status() {
    let fixture = Fixture::new();
    let state = fixture.0.join("state");
    let initial = fixture.start("initial", &state);
    let id = initial.identity();
    drop(initial);
    let path = fixture.identity_path();
    let mut record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    record["machine_binding"] = Value::Null;
    fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    let server = fixture.start("unbound", &state);
    assert_eq!(server.identity(), id);
    let warning = server.request("ping")["capabilities"]["clone_detection_warning"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(warning.starts_with("clone detection unavailable: "));
    for _ in 0..3 {
        assert_eq!(
            server.request("peers.summary")["clone_detection_warning"],
            warning
        );
    }
    for json_output in [false, true] {
        let mut cmd = fixture.command("unbound", &state);
        cmd.args(["status", "server"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if json_output {
            cmd.arg("--json");
        }
        let output = cmd.output().unwrap();
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        if json_output {
            assert_eq!(
                serde_json::from_str::<Value>(&text).unwrap()["capabilities"]
                    ["clone_detection_warning"],
                warning
            );
        } else {
            assert!(text.contains(&warning));
        }
    }

    let app_dir = if cfg!(debug_assertions) {
        "flock-dev"
    } else {
        "flock"
    };
    let log = fs::read_to_string(
        fixture
            .0
            .join("config")
            .join(app_dir)
            .join("sessions/unbound/flock-server.log"),
    )
    .unwrap();
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("mesh.clone_detection.unavailable"))
            .count(),
        1
    );
}

fn recorded_edge_processes(fleet: &support::fleet::Fleet) -> Vec<u32> {
    fleet.wait_for_edge("nodea", "nodeb", Duration::from_secs(10));
    let mut pids = Vec::new();
    for (path, shim) in fleet.edge_pids(Some("nodea-nodeb-")) {
        let record: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        pids.push(shim as u32);
        pids.extend(support::fleet::group_members(
            record["relay"].as_i64().unwrap() as i32,
        ));
    }
    assert!(
        pids.len() >= 2,
        "test must observe the shim and its relay descendants"
    );
    pids
}

fn assert_processes_gone(pids: &[u32]) {
    support::fleet::wait_until("shim descendants to exit", Duration::from_secs(5), || {
        pids.iter()
            .all(|pid| unsafe { libc::kill(*pid as i32, 0) } == -1)
            .then_some(())
    });
}

#[test]
fn node_stop_reaps_shim_grandchildren() {
    use support::fleet::{self, ONE_WAY_PAIR};
    let mut fleet = fleet::spawn("node-stop", ONE_WAY_PAIR);
    let old_pid = fleet.node("nodea").process_id();
    assert_eq!(unsafe { libc::getpgid(old_pid as i32) }, old_pid as i32);
    let pids = recorded_edge_processes(&fleet);
    fleet.refuse_edge("nodea", "nodeb");
    fleet.node_mut("nodea").stop();
    assert_processes_gone(&pids);
    fleet.node_mut("nodea").restart();
    assert_ne!(fleet.node("nodea").process_id(), old_pid);
    assert_processes_gone(&pids);
}

#[test]
fn kill_edge_leaves_no_orphans() {
    use support::fleet::{self, ONE_WAY_PAIR};
    let fleet = fleet::spawn("edge-kill", ONE_WAY_PAIR);
    let pids = recorded_edge_processes(&fleet);
    fleet.refuse_edge("nodea", "nodeb");
    assert!(fleet.kill_edge("nodea", "nodeb", Duration::from_secs(10)) > 0);
    assert_processes_gone(&pids);
    assert!(!fleet.node_id("nodea").is_empty());
    assert!(!fleet.node_id("nodeb").is_empty());
}

#[test]
fn killed_shim_still_leaves_an_owned_relay_to_clean_up() {
    use support::fleet::{self, ONE_WAY_PAIR};
    let mut fleet = fleet::spawn("shim-kill", ONE_WAY_PAIR);
    let pids = recorded_edge_processes(&fleet);
    fleet.refuse_edge("nodea", "nodeb");
    for (path, shim) in fleet.edge_pids(Some("nodea-nodeb-")) {
        unsafe {
            libc::kill(shim, libc::SIGKILL);
        }
        // Model pid reuse using this test's own pid. The record still owns the
        // relay, but cleanup must not signal the unrelated replacement shim.
        let mut record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        record["shim"] = json!(std::process::id());
        fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
    }
    fleet.node_mut("nodea").stop();
    assert_processes_gone(&pids);
}

#[test]
fn mesh_topology_helpers_observe_routes_and_reload_allow_policy() {
    use support::fleet::{self, LAPTOP_TWO_HUBS};
    let fleet = fleet::spawn("topologies", LAPTOP_TWO_HUBS);
    fleet.wait_route("nodea", "noded.example", true);
    fleet.set_allow_from("noded.example", &["nodea"]);
    // Updating an existing msg table must replace allow_from, not append a
    // second TOML table that makes reload fail.
    fleet.set_allow_from("noded.example", &["nodeb", "nodec"]);
    fleet.refuse_edge("nodea", "nodeb");
    fleet.refuse_edge("nodea", "nodec");
    fleet.kill_edge("nodea", "nodeb", Duration::from_secs(10));
    fleet.kill_edge("nodea", "nodec", Duration::from_secs(10));
    fleet.wait_route("nodea", "noded.example", false);
}

#[test]
fn no_test_spawns_flk_outside_the_isolation_wrapper() {
    let raw = regex::Regex::new(concat!(
        r"(?:std::process::",
        r"Command::new\(\s*env!|use\s+std::process::\{[^}]*\bCommand\b|use\s+std::process::",
        r"Command\s*;|process::\{[^}]*\bCommand\b)"
    ))
    .unwrap();
    fn scan(dir: &Path, violations: &mut Vec<PathBuf>, raw: &regex::Regex) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                scan(&path, violations, raw);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs")
                || path.ends_with("support/environment.rs")
            {
                continue;
            }
            let source = fs::read_to_string(&path).unwrap();
            let code = source
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            let compact: String = code.chars().filter(|c| !c.is_whitespace()).collect();
            let binary = ["CARGO_BIN_EXE_", "flk"].concat();
            let direct = [".spawn_", "command("].concat();
            if compact.contains(&direct) || (code.contains(&binary) && raw.is_match(&code)) {
                violations.push(path);
            }
        }
    }
    let mut violations = Vec::new();
    scan(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests"),
        &mut violations,
        &raw,
    );
    assert!(
        violations.is_empty(),
        "flk launches must use support::environment's isolation wrapper: {violations:?}"
    );
}
