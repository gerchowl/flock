//! #426 — a server that lost its macOS session must say so, and handoff must
//! refuse to pretend otherwise.
//!
//! This is the end-to-end half. The primitive itself is proven against a real
//! unreachable passwd database in `platform::macos`; here the same trick is used
//! to stand up a real `flk server` inside one, so the assertions run against a
//! real socket, a real `flk status`, and a real `live-handoff` refusal.
//!
//! ## What is and is not demonstrated
//!
//! These tests produce the *mechanism* the issue reports — a process whose
//! mach route to opendirectoryd is gone, so `getpwuid` returns NULL and every
//! pane's `ssh`/`sudo`/DNS fails — by denying `mach-lookup`. Verified alongside
//! on the development machine: under the same sandbox `whoami` prints a bare
//! uid and `id -un` fails, matching the issue's symptom exactly.
//!
//! What they do **not** do is produce a genuinely orphaned server: a real one
//! requires unloading the user's launchd domain under a live flock, which would
//! risk the machine and every live session on it. So the orphaning itself is
//! not demonstrated here, only the fault it causes. That limit is deliberate
//! and worth stating rather than papering over with a hand-built fake.
//!
//! `#[cfg(target_os = "macos")]` is not a coverage gate — it is the shape the
//! ratchet in `scripts/platform_test_gap.py` treats as *present*, because these
//! tests exist on the platform whose bug this is.

#![cfg(target_os = "macos")]
// The harness drives the compiled binary and `sandbox-exec` directly; the
// traced-command funnel is a product-code concern, and every other test harness
// in `tests/` says the same.
#![allow(clippy::disallowed_methods)]

mod support;

use std::fs;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use support::environment::Command;

use support::{cleanup_test_base, register_runtime_dir, wait_for_socket};

/// Resolve a program through `PATH` rather than hardcoding a path, per the
/// hermetic-tests gate.
fn program_path(name: &str) -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH must be set for a test");
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| panic!("`{name}` must be on PATH for this test"))
}

/// Denying `mach-lookup` is not a mock of the failure, it is the failure's
/// mechanism: opendirectoryd becomes unreachable over mach, so `getpwuid`
/// returns NULL exactly as it does for a server whose launchd session is gone.
const BROKEN_SESSION_PROFILE: &str = "(version 1)(allow default)(deny mach-lookup)";

struct Harness {
    base: PathBuf,
    socket: PathBuf,
    config_home: PathBuf,
    runtime_dir: PathBuf,
    server: Option<std::process::Child>,
}

impl Harness {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        // Short, because `sun_path` caps near 104 bytes on macOS and a deep
        // scratch dir fails to bind in a way that looks like "server did not
        // become ready".
        let base = PathBuf::from(format!("/tmp/fs-{label}-{}-{nanos}", std::process::id()));
        let config_home = base.join("cfg");
        let runtime_dir = base.join("rt");
        fs::create_dir_all(config_home.join("flock")).expect("config home");
        fs::create_dir_all(&runtime_dir).expect("runtime dir");
        fs::write(
            config_home.join("flock/config.toml"),
            "onboarding = false\n",
        )
        .expect("seed config");
        register_runtime_dir(&runtime_dir);
        Self {
            socket: runtime_dir.join("flock.sock"),
            base,
            config_home,
            runtime_dir,
            server: None,
        }
    }

    fn spawn_server(&mut self, broken: bool) {
        let mut command = if broken {
            let mut sandbox = Command::new(program_path("sandbox-exec"));
            sandbox
                .arg("-p")
                .arg(BROKEN_SESSION_PROFILE)
                .arg(env!("CARGO_BIN_EXE_flk"));
            sandbox
        } else {
            Command::new(env!("CARGO_BIN_EXE_flk"))
        };
        command
            .arg("server")
            .envs(support::environment::isolated_env(
                &self.config_home,
                &self.runtime_dir,
            ))
            .env("FLOCK_SOCKET_PATH", &self.socket)
            .env_remove("FLOCK_CLIENT_SOCKET_PATH")
            .env_remove("FLOCK_ENV")
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        support::environment::assert_command_isolated(&command);
        let child = command.spawn().expect("server should spawn");
        self.server = Some(child);
        wait_for_socket(&self.socket, Duration::from_secs(30));
    }

    /// Run a `flk` subcommand as a **healthy** client against this server.
    ///
    /// The client is deliberately not sandboxed. That is the whole shape of the
    /// report: the operator's terminal works fine, and it is the *server* that
    /// is poisoned — which is also why the client cannot detect this for itself
    /// and the server has to say so over the socket.
    fn client(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_flk"))
            .args(args)
            .envs(support::environment::isolated_env(
                &self.config_home,
                &self.runtime_dir,
            ))
            .env("FLOCK_SOCKET_PATH", &self.socket)
            .env_remove("FLOCK_CLIENT_SOCKET_PATH")
            .env_remove("FLOCK_ENV")
            .output()
            .expect("client should run")
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(mut child) = self.server.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        cleanup_test_base(&self.base);
    }
}

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// How long to allow the server to *confirm* a broken session.
///
/// The verdict needs consecutive Broken readings, not one, so a test that
/// asserts on the warning has to wait for the confirmation rather than for the
/// socket. Polling for it rather than sleeping a fixed amount keeps this honest
/// on a loaded machine without turning into a flake: the deadline is generous and
/// the assertion below it is the real one.
fn wait_for_confirmed_broken(harness: &Harness) {
    let deadline = Instant::now() + Duration::from_secs(40);
    while Instant::now() < deadline {
        if stdout_of(&harness.client(&["status"])).contains("session: broken") {
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    panic!("the server never confirmed a broken session");
}

/// The headline requirement: a healthy `flk status` must report the server's
/// broken session instead of printing an entirely reassuring block.
#[test]
fn status_reports_a_broken_session() {
    let mut harness = Harness::new("broken");
    harness.spawn_server(true);
    wait_for_confirmed_broken(&harness);

    let output = harness.client(&["status"]);
    let text = stdout_of(&output);
    assert!(
        output.status.success(),
        "status should still succeed; stdout: {text}"
    );
    assert!(
        text.contains("session: broken"),
        "flk status must name the broken session; stdout was:\n{text}"
    );
    // The symptom, not just the verdict — "session: broken" alone leaves the
    // reader to work out what that means for their agents.
    assert!(
        text.contains("cannot resolve names") && text.contains("use sudo"),
        "flk status must say what is broken; stdout was:\n{text}"
    );
}

/// The control: a healthy server must not be warned about. Without this, the
/// test above would also pass if the warning were unconditional.
#[test]
fn status_is_quiet_for_a_healthy_session() {
    let mut harness = Harness::new("healthy");
    harness.spawn_server(false);

    let text = stdout_of(&harness.client(&["status"]));
    assert!(
        text.contains("status: running"),
        "sanity: the healthy server should be running; stdout was:\n{text}"
    );
    assert!(
        !text.contains("session: broken"),
        "a healthy server must not be reported as broken; stdout was:\n{text}"
    );
}

/// `flk status --json` is the machine-readable surface, so the fault has to be
/// in it too — a warning that only exists in the human rendering is invisible
/// to anything watching the server.
#[test]
fn status_json_reports_a_broken_session() {
    let mut harness = Harness::new("json");
    harness.spawn_server(true);
    wait_for_confirmed_broken(&harness);

    let text = stdout_of(&harness.client(&["status", "--json"]));
    let value: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|err| panic!("status --json: {err}\n{text}"));
    assert_eq!(
        value["server"]["session_health"].as_str(),
        Some("broken"),
        "the JSON surface must carry the fault; stdout was:\n{text}"
    );
    // And the healthy server reports its own health, not a null that a caller
    // has to interpret.
    assert!(value["server"]["status"].as_str() == Some("running"));
}

/// The second bug: handoff propagates the fault, so it must refuse — and say
/// why, since "handoff did not work" is exactly the wrong thing to leave an
/// operator with when handoff is the tool they reached for.
#[test]
fn live_handoff_refuses_from_a_broken_session() {
    let mut harness = Harness::new("handoff");
    harness.spawn_server(true);
    wait_for_confirmed_broken(&harness);
    // Handoff's threshold is deliberately stricter than the banner's, so
    // confirming the banner is not enough to make this test meaningful. Wait for
    // the extra reading rather than asserting against the looser state.
    std::thread::sleep(Duration::from_secs(7));

    let output = harness.client(&["server", "live-handoff"]);
    let text = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        !output.status.success(),
        "live-handoff must not report success; stderr was:\n{text}"
    );
    assert!(
        text.contains("inherits"),
        "the refusal must explain that handoff propagates the fault; stderr was:\n{text}"
    );
    assert!(
        text.contains("flk server stop"),
        "the refusal must name the recovery; stderr was:\n{text}"
    );
    assert!(
        !text.contains('{'),
        "the refusal is written for a person and must not be JSON; stderr was:\n{text}"
    );
}

/// The control for the refusal: a healthy server still hands off. Without this,
/// the refusal test would also pass if handoff were simply broken.
/// The control for the refusal.
///
/// Previously this asserted only that the text `refusing live handoff` was
/// absent, which a wholly broken handoff would satisfy — a control that cannot
/// fail is false confidence, and it is exactly the shape that let the review's
/// point 5 stand. It now asserts the handoff **succeeded**: the command must
/// exit 0 and report the completion it reports on a healthy server. If handoff
/// regressed entirely, this goes red.
#[test]
fn live_handoff_still_succeeds_for_a_healthy_session() {
    let mut harness = Harness::new("handoff-ok");
    harness.spawn_server(false);

    let output = harness.client(&["server", "live-handoff"]);
    let text = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "a healthy server must still hand off; stderr was:\n{text}"
    );
    assert!(
        text.contains("live handoff complete"),
        "a healthy handoff must report success, not just the absence of a refusal; stderr was:\n{text}"
    );
}

/// The server's own log is where the operator lands after the UI has already
/// shown them a red banner. The transition is logged once — not on every
/// five-second refresh, which is what makes `grep WARN` useless (#318).
#[test]
fn a_broken_session_is_logged_once_not_repeated() {
    let mut harness = Harness::new("log");
    harness.spawn_server(true);

    // Past one TTL (5s), so at least two probes have happened: the one at
    // startup and one refresh. A per-probe log line would make this 2+.
    std::thread::sleep(Duration::from_secs(7));
    let log = harness.config_home.join("flock-dev/flock-server.log");
    let text = fs::read_to_string(&log).unwrap_or_default();
    let count = text
        .lines()
        .filter(|line| line.contains("server has no usable user session"))
        .count();
    assert_eq!(
        count, 1,
        "the lost-session warning must be logged once per transition, saw {count} in:\n{text}"
    );
}
