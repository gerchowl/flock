//! `flk owner policy adopt` is an operator verb (ADR-0035 §6.2, #1195).
//!
//! The acceptance case end to end: a real `flk server` in a PTY, a repo whose
//! default branch carries a `.flk.toml`, and the CLI run twice, once from a
//! shell inside a flk pane (an attested agent caller, refused with exit 13)
//! and once from the test process, which runs in no pane (the operator).

// TracedCommand polices flock's shipped code; this harness drives the
// compiled flock binary and git through raw Command.
#![allow(clippy::disallowed_methods)]

mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde_json::{json, Value};
use support::{
    cleanup_test_base, register_runtime_dir, register_spawned_flock_pid,
    unregister_spawned_flock_pid, wait_for_socket,
};

const WITHIN: Duration = Duration::from_secs(20);

struct Server {
    _master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    base: PathBuf,
    socket: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let pid = self.child.process_id();
        let _ = self.child.kill();
        // Bounded reap, as `tests/delegate_safety.rs` does: a blocking wait
        // never returns on macOS.
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
        cleanup_test_base(&self.base);
    }
}

fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "flock-dev"
    } else {
        "flock"
    }
}

fn start_server() -> Server {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let base = PathBuf::from(format!("/tmp/hown-{}-{nanos}", std::process::id()));
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket = runtime_dir.join("flock.sock");
    fs::create_dir_all(config_home.join(app_dir_name())).unwrap();
    fs::create_dir_all(&runtime_dir).unwrap();
    fs::create_dir_all(base.join("state")).unwrap();
    fs::create_dir_all(base.join("home")).unwrap();
    register_runtime_dir(&runtime_dir);
    fs::write(
        config_home.join(app_dir_name()).join("config.toml"),
        "onboarding = false\n",
    )
    .unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_flk"));
    cmd.arg("server");
    cmd.cwd(&base);
    for (key, value) in support::environment::isolated_env(&config_home, &runtime_dir) {
        cmd.env(key, value);
    }
    cmd.env("XDG_STATE_HOME", base.join("state"));
    cmd.env("HOME", base.join("home"));
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("FLOCK_SOCKET_PATH", &socket);
    cmd.env_remove("FLOCK_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("FLOCK_ENV");
    cmd.env_remove("FLOCK_HOST_NAME");
    support::environment::assert_pty_isolated(&cmd);
    let child = support::environment::spawn_pty(pair.slave.as_ref(), cmd).unwrap();
    register_spawned_flock_pid(child.process_id());
    wait_for_socket(&socket, Duration::from_secs(5));
    Server {
        _master: pair.master,
        child,
        base,
        socket,
    }
}

fn request(server: &Server, method: &str, params: Value) -> Value {
    let mut stream = UnixStream::connect(&server.socket).unwrap();
    let line = json!({"id": "t", "method": method, "params": params});
    writeln!(stream, "{line}").unwrap();
    stream.flush().unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    serde_json::from_str(&response).unwrap()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false", "-c", "user.name=Flock Test"])
        .args(["-c", "user.email=flock@example.invalid", "-C"])
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// An upstream repo with a committed `.flk.toml` and a clone of it, whose
/// `origin/HEAD` names the upstream's default branch.
fn repo_with_policy(base: &Path) -> (PathBuf, String) {
    let upstream = base.join("upstream");
    fs::create_dir_all(&upstream).unwrap();
    git(&upstream, &["init", "--quiet", "-b", "main"]);
    fs::write(
        upstream.join(".flk.toml"),
        "[owner.standup]\ntime = \"09:00\"\n",
    )
    .unwrap();
    git(&upstream, &["add", ".flk.toml"]);
    git(&upstream, &["commit", "--quiet", "-m", "policy"]);
    let clone = base.join("clone");
    git(
        base,
        &[
            "clone",
            "--quiet",
            upstream.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    let blob = git(&upstream, &["rev-parse", "HEAD:.flk.toml"]);
    (clone, blob)
}

fn pane_text(server: &Server, pane: &str) -> String {
    let read = request(
        server,
        "pane.read",
        json!({"pane_id": pane, "source": "recent", "lines": 200}),
    );
    read["result"]["read"]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn an_agent_calling_adopt_is_refused_with_exit_13_and_the_operator_is_not() {
    let server = start_server();
    let (clone, blob) = repo_with_policy(&server.base);
    let created = request(&server, "workspace.create", json!({"label": "agent"}));
    let pane = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .expect("root pane id")
        .to_string();

    // A shell inside a flk pane: an attested agent caller (ADR-0014).
    let command = format!(
        "cd '{}' && FLOCK_SOCKET_PATH='{}' '{}' owner policy adopt {blob}; echo \"ADOPT_EXIT=$?\"",
        clone.display(),
        server.socket.display(),
        env!("CARGO_BIN_EXE_flk"),
    );
    let sent = request(
        &server,
        "pane.send_input",
        json!({"pane_id": pane, "text": command, "keys": ["Enter"]}),
    );
    assert!(sent.get("error").is_none(), "{sent}");
    let deadline = Instant::now() + WITHIN;
    let mut text = String::new();
    while Instant::now() < deadline && !text.contains("ADOPT_EXIT=1") {
        text = pane_text(&server, &pane);
        thread::sleep(Duration::from_millis(50));
    }
    assert!(text.contains("ADOPT_EXIT=13"), "{text}");
    assert!(text.contains("operator_only"), "{text}");

    let shown = request(
        &server,
        "owner.policy.show",
        json!({"repo": clone.to_str().unwrap()}),
    );
    assert!(shown["result"]["adopted"].is_null(), "{shown}");
    assert_eq!(shown["result"]["pending"], true, "{shown}");

    // The test process runs in no pane: the operator.
    let out = support::environment::Command::new(env!("CARGO_BIN_EXE_flk"))
        .args(["owner", "policy", "adopt", &blob])
        .current_dir(&clone)
        .env("FLOCK_SOCKET_PATH", &server.socket)
        .env_remove("FLOCK_CLIENT_SOCKET_PATH")
        .env("XDG_STATE_HOME", server.base.join("state"))
        .env("XDG_CONFIG_HOME", server.base.join("config"))
        .env("HOME", server.base.join("home"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let adopted: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        adopted["result"]["adopted"]["policy_version"],
        blob.as_str()
    );
    assert_eq!(adopted["result"]["policy"]["standup"]["time"], "09:00");
}
