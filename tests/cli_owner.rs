//! E2E (#1231): `flk owner bind|unbind|list|show` through the real binary
//! against an isolated server (ADR-0035 §1).
//!
//! The agent caller is real: `flk owner bind` runs inside a pane `agent.start`
//! launched, so the server attests it by process ancestry, not by a fixture
//! flag, and its refusal must exit 13 (`exit::POLICY_REFUSED`).

// Integration tests drive the compiled binary through raw Command; the
// TracedCommand funnel polices flock's own subprocesses, not the harness's.
#![allow(clippy::disallowed_methods)]

mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde_json::Value;
use support::environment::Command;
use support::{
    cleanup_test_base, register_runtime_dir, register_spawned_flock_pid,
    unregister_spawned_flock_pid, wait_for_socket,
};

const REPO: &str = "example/widgets";

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
        // Bounded reap, as `tests/agent_settled.rs` does: a blocking wait
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
    let base = PathBuf::from(format!("/tmp/fown-{}-{nanos}", std::process::id()));
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket = runtime_dir.join("flock.sock");
    fs::create_dir_all(config_home.join(app_dir_name())).unwrap();
    fs::create_dir_all(&runtime_dir).unwrap();
    register_runtime_dir(&runtime_dir);
    fs::write(
        config_home.join(app_dir_name()).join("config.toml"),
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
    for (key, value) in support::environment::isolated_env(&config_home, &runtime_dir) {
        cmd.env(key, value);
    }
    cmd.env("FLOCK_SOCKET_PATH", &socket);
    cmd.env_remove("FLOCK_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    for key in [
        "FLOCK_ENV",
        "FLOCK_HOST_NAME",
        "FLOCK_DISABLE_SOUND",
        "FLOCK_PANE_ID",
        "FLOCK_AGENT_ID",
    ] {
        cmd.env_remove(key);
    }
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

fn request(server: &Server, json: &Value) -> Value {
    let mut stream = UnixStream::connect(&server.socket).unwrap();
    writeln!(stream, "{json}").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

/// `flk args` as the operator: a shell outside every pane of this server.
fn cli(server: &Server, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_flk"))
        .args(args)
        .env("FLOCK_SOCKET_PATH", &server.socket)
        .env_remove("FLOCK_CLIENT_SOCKET_PATH")
        .env_remove("FLOCK_PANE_ID")
        .env_remove("FLOCK_AGENT_ID")
        .env_remove("FLOCK_ENV")
        .output()
        .unwrap()
}

fn json_out(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "stdout is not one JSON value ({err}): {:?} / stderr {:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// A directory the server accepts as a git checkout.
fn checkout(server: &Server) -> String {
    let dir = server.base.join("widgets");
    fs::create_dir_all(dir.join(".git")).unwrap();
    dir.display().to_string()
}

fn bind_args<'a>(repo: &'a str, checkout: &'a str) -> Vec<&'a str> {
    vec![
        "owner",
        "bind",
        repo,
        "--checkout",
        checkout,
        "--harness",
        "claude",
        "--profile",
        "main",
    ]
}

/// Wait for a file the pane writes when its command is done.
fn written(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(text) = fs::read_to_string(path) {
            if text.ends_with('\n') {
                return text;
            }
        }
        assert!(Instant::now() < deadline, "the pane never wrote {path:?}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// `list` and `show` read back what `bind` recorded, and `--json` changes
/// nothing on success: the reply is the server's result either way.
#[test]
fn list_and_show_round_trip_a_bind() {
    let server = start_server();
    let checkout = checkout(&server);

    let bound = cli(&server, &bind_args(REPO, &checkout));
    assert_eq!(bound.status.code(), Some(0), "{bound:?}");
    let bound = json_out(&bound);
    assert_eq!(bound["result"]["type"], "owner_bound", "{bound}");
    assert_eq!(bound["result"]["rebound"], false);
    let binding = bound["result"]["binding"].clone();
    assert_eq!(binding["checkout"], checkout.as_str());
    assert_eq!(binding["harness"], "claude");
    assert_eq!(binding["profiles"], serde_json::json!(["main"]));

    let shown = cli(&server, &["owner", "show", REPO, "--json"]);
    assert_eq!(shown.status.code(), Some(0), "{shown:?}");
    assert_eq!(json_out(&shown)["result"]["binding"], binding);

    let listed = cli(&server, &["owner", "list"]);
    assert_eq!(listed.status.code(), Some(0), "{listed:?}");
    assert_eq!(
        json_out(&listed)["result"]["bindings"],
        serde_json::json!([binding])
    );

    let unbound = cli(&server, &["owner", "unbind", REPO]);
    assert_eq!(unbound.status.code(), Some(0), "{unbound:?}");
    assert_eq!(json_out(&unbound)["result"]["binding"], binding);
    let listed = cli(&server, &["owner", "list"]);
    assert_eq!(
        json_out(&listed)["result"]["bindings"],
        serde_json::json!([])
    );
}

/// `unbind` and `show` of a repo nobody bound report it gone: exit 4, and
/// under `--json` the envelope names `not_found` with `exit: 4`.
#[test]
fn unbind_of_an_unknown_repo_reports_gone() {
    let server = start_server();

    let plain = cli(&server, &["owner", "unbind", "example/nobody"]);
    assert_eq!(plain.status.code(), Some(4), "{plain:?}");
    assert!(
        String::from_utf8_lossy(&plain.stderr).contains("not_found"),
        "{plain:?}"
    );

    let enveloped = cli(&server, &["owner", "unbind", "example/nobody", "--json"]);
    assert_eq!(enveloped.status.code(), Some(4), "{enveloped:?}");
    let envelope = json_out(&enveloped);
    assert_eq!(envelope["error"]["code"], "not_found", "{envelope}");
    assert_eq!(envelope["error"]["exit"], 4, "{envelope}");
    assert_eq!(envelope["error"]["retry"], "never", "{envelope}");

    let shown = cli(&server, &["owner", "show", "example/nobody"]);
    assert_eq!(shown.status.code(), Some(4), "{shown:?}");
}

/// ADR-0035 §1 acceptance: an attested agent calling `bind` or `unbind`
/// exits 13 with `operator_only`, and the binding is untouched. The caller is
/// a `flk` the agent's own pane runs.
#[test]
fn an_attested_agent_caller_is_refused_with_13() {
    let server = start_server();
    let checkout = checkout(&server);
    let bound = cli(&server, &bind_args(REPO, &checkout));
    assert_eq!(bound.status.code(), Some(0), "{bound:?}");
    let before = json_out(&cli(&server, &["owner", "show", REPO]));

    let created = request(
        &server,
        &serde_json::json!({"id":"ws","method":"workspace.create",
            "params":{"cwd": server.base.display().to_string(), "focus": true}}),
    );
    let workspace_id = created["result"]["workspace"]["workspace_id"]
        .as_str()
        .unwrap_or_else(|| panic!("workspace.create: {created}"))
        .to_string();
    let flk = env!("CARGO_BIN_EXE_flk");
    let socket = server.socket.display();
    let bind_out = server.base.join("bind.out");
    let bind_json = server.base.join("bind.json");
    let unbind_out = server.base.join("unbind.out");
    let script = format!(
        "FLOCK_SOCKET_PATH='{socket}' '{flk}' owner bind example/other --checkout '{checkout}' \
         --harness claude --json > '{json}'; echo $? > '{bind}'; \
         FLOCK_SOCKET_PATH='{socket}' '{flk}' owner unbind {REPO} > /dev/null 2>&1; \
         echo $? > '{unbind}'; read -r line",
        bind = bind_out.display(),
        json = bind_json.display(),
        unbind = unbind_out.display(),
    );
    let started = request(
        &server,
        &serde_json::json!({"id":"start","method":"agent.start","params":{
            "name": "rogue", "workspace_id": workspace_id,
            "argv": ["/bin/sh", "-c", script]}}),
    );
    assert!(started.get("result").is_some(), "agent.start: {started}");

    assert_eq!(written(&bind_out).trim(), "13");
    assert_eq!(written(&unbind_out).trim(), "13");
    let envelope: Value = serde_json::from_str(&fs::read_to_string(&bind_json).unwrap()).unwrap();
    assert_eq!(envelope["error"]["code"], "operator_only", "{envelope}");
    assert_eq!(envelope["error"]["exit"], 13, "{envelope}");

    assert_eq!(json_out(&cli(&server, &["owner", "show", REPO])), before);
    let listed = json_out(&cli(&server, &["owner", "list"]));
    assert_eq!(
        listed["result"]["bindings"].as_array().map(Vec::len),
        Some(1),
        "{listed}"
    );
}

/// A usage error never reaches the server: exit 2.
#[test]
fn a_bind_without_its_required_flags_exits_2() {
    let server = start_server();
    for args in [
        &["owner", "bind", REPO, "--harness", "claude"][..],
        &[
            "owner",
            "bind",
            REPO,
            "--checkout",
            "--profile",
            "--harness",
            "claude",
        ],
    ] {
        let output = cli(&server, args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
    }
    let listed = json_out(&cli(&server, &["owner", "list"]));
    assert_eq!(listed["result"]["bindings"], serde_json::json!([]));
}

/// `flk owner help`, `--help` and `-h` print every verb's usage on stderr
/// and exit 0, as dev's `flk owner policy` dispatcher did (#1195). Help is
/// answered before any request, so no server is needed.
#[test]
fn every_help_form_prints_the_usage_and_exits_0() {
    let dir = std::env::temp_dir().join(format!("fown-help-{}", std::process::id()));
    let mut problems = Vec::new();
    for form in ["help", "--help", "-h"] {
        let output = Command::new(env!("CARGO_BIN_EXE_flk"))
            .args(["owner", form])
            .env("FLOCK_SOCKET_PATH", dir.join("no-server.sock"))
            .env_remove("FLOCK_CLIENT_SOCKET_PATH")
            .env_remove("FLOCK_PANE_ID")
            .env_remove("FLOCK_AGENT_ID")
            .env_remove("FLOCK_ENV")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.code() != Some(0) {
            problems.push(format!("{form}: exit {:?}", output.status.code()));
        }
        for usage in [
            "usage: flk owner bind",
            "flk owner show",
            "flk owner policy show",
        ] {
            if !stderr.contains(usage) {
                problems.push(format!("{form}: {usage:?} not on stderr"));
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}
