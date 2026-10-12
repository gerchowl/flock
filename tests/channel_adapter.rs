//! The channel adapter contract (ADR-0028 §8, #1037) against a real isolated
//! server, driven by a fake channel adapter that starts the way the contract
//! says an adapter must: `flk adapter hello` first, and no start on refusal.
#![allow(clippy::disallowed_methods)] // The harness drives the compiled binary.

mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use support::environment::Command;
use support::{
    cleanup_test_base, register_runtime_dir, register_spawned_flock_pid,
    unregister_spawned_flock_pid,
};

fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "flock-dev"
    } else {
        "flock"
    }
}

fn test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn unique_test_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    PathBuf::from(format!("/tmp/hchan-{}-{nanos}", std::process::id()))
}

/// Where the fake adapter runs: the server's isolated environment.
struct Env {
    base: PathBuf,
    config_home: PathBuf,
    runtime_dir: PathBuf,
    socket: PathBuf,
}

impl Env {
    fn new() -> Self {
        let base = unique_test_dir();
        Self {
            config_home: base.join("config"),
            runtime_dir: base.join("runtime"),
            socket: base.join("api.sock"),
            base,
        }
    }

    fn data_dir(&self) -> PathBuf {
        self.config_home.join(app_dir_name())
    }

    fn flk(&self, args: &[&str]) -> std::process::Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_flk"));
        command.args(args);
        for (key, value) in support::environment::isolated_env(&self.config_home, &self.runtime_dir)
        {
            command.env(key, value);
        }
        command
            .env("FLOCK_SOCKET_PATH", &self.socket)
            .env_remove("FLOCK_CLIENT_SOCKET_PATH")
            .env_remove("FLOCK_SESSION")
            .env_remove("FLOCK_ENV")
            .env_remove("FLOCK_HOST_NAME")
            .output()
            .unwrap()
    }
}

/// The reference adapter test double. A real adapter (Telegram,
/// gerchowl/telegram-bot) does the same before it touches its chat.
struct FakeChannel;

#[derive(Debug)]
struct Started {
    hello: serde_json::Value,
}

#[derive(Debug)]
struct Refused {
    exit: Option<i32>,
    code: Option<String>,
    stderr: String,
}

impl FakeChannel {
    fn start(env: &Env, binding: &str, token_file: Option<&Path>) -> Result<Started, Refused> {
        let mut args = vec!["adapter", "hello", binding];
        if let Some(path) = token_file {
            args.extend(["--token-file", path.to_str().unwrap()]);
        }
        let output = env.flk(&args);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if output.status.success() {
            let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            return Ok(Started {
                hello: response["result"].clone(),
            });
        }
        let code = stderr
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find_map(|value| value["error"]["code"].as_str().map(str::to_owned));
        Err(Refused {
            exit: output.status.code(),
            code,
            stderr,
        })
    }
}

struct SpawnedFlock {
    _master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl Drop for SpawnedFlock {
    fn drop(&mut self) {
        let pid = self.child.process_id();
        let _ = self.child.kill();
        // Reap without blocking, as api_ping.rs does: a blocking wait on the
        // pty child can hang after the kill.
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
        }
        unregister_spawned_flock_pid(pid);
    }
}

fn spawn_server(env: &Env, config_toml: &str) -> SpawnedFlock {
    fs::create_dir_all(env.data_dir()).unwrap();
    fs::create_dir_all(&env.runtime_dir).unwrap();
    register_runtime_dir(&env.runtime_dir);
    fs::write(env.data_dir().join("config.toml"), config_toml).unwrap();
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
    for (key, value) in support::environment::isolated_env(&env.config_home, &env.runtime_dir) {
        cmd.env(key, value);
    }
    cmd.env("FLOCK_SOCKET_PATH", &env.socket);
    cmd.env_remove("FLOCK_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("FLOCK_ENV");
    cmd.env_remove("FLOCK_HOST_NAME");
    support::environment::assert_pty_isolated(&cmd);
    let child = support::environment::spawn_pty(pair.slave.as_ref(), cmd).unwrap();
    register_spawned_flock_pid(child.process_id());
    support::wait_for_socket(&env.socket, Duration::from_secs(20));
    SpawnedFlock {
        _master: pair.master,
        child,
    }
}

const CONFIG: &str = r#"onboarding = false
name = "atlas"

[[channels]]
name = "phone"
owner_node = "atlas"
target = "agent:agent_atlas_fixture"
scopes = ["notify", "reply"]

[[channels]]
name = "elsewhere"
owner_node = "kiln"
target = "fleet"
scopes = ["notify"]
"#;

fn server_logs(env: &Env) -> String {
    let mut text = String::new();
    for entry in fs::read_dir(env.data_dir()).unwrap().flatten() {
        if entry.path().extension().is_some_and(|ext| ext == "log") {
            text.push_str(&fs::read_to_string(entry.path()).unwrap_or_default());
        }
    }
    text
}

/// #1037: the owner node issues a 0600 token per owned binding, and an
/// adapter presenting it is accepted with the binding's target and scopes.
#[test]
fn a_good_hello_starts_the_adapter_with_the_binding_scopes() {
    let _guard = test_lock();
    let env = Env::new();
    let server = spawn_server(&env, CONFIG);
    let token = env.data_dir().join("channel-tokens/phone.token");
    support::wait_for_file(&token, Duration::from_secs(5));
    let mode = fs::metadata(&token).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert!(
        !env.data_dir()
            .join("channel-tokens/elsewhere.token")
            .exists(),
        "a node issues tokens only for the bindings it owns"
    );

    let started = FakeChannel::start(&env, "phone", None).expect("adapter starts");
    assert_eq!(started.hello["type"], "channel_hello");
    assert_eq!(started.hello["binding"], "phone");
    assert_eq!(started.hello["target"], "agent:agent_atlas_fixture");
    assert_eq!(
        started.hello["scopes"],
        serde_json::json!(["notify", "reply"])
    );
    assert!(!started
        .hello
        .to_string()
        .contains(fs::read_to_string(&token).unwrap().trim()));

    drop(server);
    cleanup_test_base(&env.base);
}

/// #1037: a wrong token is refused with a registry code and logged with the
/// binding and reason, and neither token ever reaches the log. A binding owned
/// by another node is refused with its own code.
#[test]
fn a_bad_token_is_refused_and_logged_without_the_token() {
    let _guard = test_lock();
    let env = Env::new();
    let server = spawn_server(&env, CONFIG);
    let token = env.data_dir().join("channel-tokens/phone.token");
    support::wait_for_file(&token, Duration::from_secs(5));
    let issued = fs::read_to_string(&token).unwrap().trim().to_owned();
    let wrong = "0123456789abcdef-not-the-issued-token";
    let wrong_file = env.base.join("wrong.token");
    fs::write(&wrong_file, wrong).unwrap();

    let refused = FakeChannel::start(&env, "phone", Some(&wrong_file)).unwrap_err();
    assert_eq!(
        refused.code.as_deref(),
        Some("channel_unauthorized"),
        "{refused:?}"
    );
    assert_eq!(refused.exit, Some(1), "{refused:?}");
    assert!(!refused.stderr.contains(wrong), "{refused:?}");

    let unknown = FakeChannel::start(&env, "nosuch", Some(&wrong_file)).unwrap_err();
    assert_eq!(unknown.code.as_deref(), Some("channel_unauthorized"));
    let not_owner = FakeChannel::start(&env, "elsewhere", Some(&wrong_file)).unwrap_err();
    assert_eq!(not_owner.code.as_deref(), Some("channel_not_owner"));

    let deadline = Instant::now() + Duration::from_secs(5);
    let logs = loop {
        let logs = server_logs(&env);
        if logs.contains("bad_token") || Instant::now() >= deadline {
            break logs;
        }
        thread::sleep(Duration::from_millis(25));
    };
    let refusal = logs
        .lines()
        .find(|line| line.contains("channel.hello.refused") && line.contains("bad_token"))
        .unwrap_or_else(|| panic!("no refusal logged:\n{logs}"));
    assert!(refusal.contains("\"binding\":\"phone\""), "{refusal}");
    assert!(logs.contains("not_owner") && logs.contains("unknown_binding"));
    assert!(!logs.contains(wrong), "the presented token reached the log");
    assert!(!logs.contains(&issued), "the issued token reached the log");

    drop(server);
    cleanup_test_base(&env.base);
}

/// #1037 with #868: against a server older than `channel.hello` the adapter
/// refuses to start with `server_version_gap` (exit 78), and the hello is
/// never sent.
#[test]
fn the_adapter_refuses_to_start_against_a_server_without_channel_hello() {
    let env = Env::new();
    fs::create_dir_all(&env.base).unwrap();
    let listener = UnixListener::bind(&env.socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stopped = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&stopped);
    let server = thread::spawn(move || {
        let mut methods = Vec::new();
        while !stop.load(Ordering::SeqCst) {
            let stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(err) => panic!("accept: {err}"),
            };
            stream.set_nonblocking(false).unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            methods.push(request["method"].as_str().unwrap().to_owned());
            let pong = serde_json::json!({"id": request["id"], "result": {
                "type": "pong", "version": "2.0.0", "protocol": support::PROTOCOL_VERSION - 1,
            }});
            writeln!(&stream, "{pong}").unwrap();
        }
        methods
    });
    let token_file = env.base.join("phone.token");
    fs::write(&token_file, "fixture-token").unwrap();

    let refused = FakeChannel::start(&env, "phone", Some(&token_file)).unwrap_err();
    assert_eq!(refused.exit, Some(78), "{refused:?}");
    assert_eq!(
        refused.code.as_deref(),
        Some("server_version_gap"),
        "{refused:?}"
    );
    stopped.store(true, Ordering::SeqCst);
    assert_eq!(server.join().unwrap(), ["ping"], "only the probe was sent");
    cleanup_test_base(&env.base);
}
