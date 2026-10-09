//! Identity evidence starts at real server boot and ends at both public APIs.
// Integration fixtures launch the binary, so they cannot use its private logging funnel.
#![expect(clippy::disallowed_methods)]

mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
        while !server.socket.exists() {
            assert!(
                server.child.try_wait().unwrap().is_none(),
                "server exited before binding API"
            );
            assert!(Instant::now() < deadline, "server startup timed out");
            std::thread::sleep(Duration::from_millis(20));
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
