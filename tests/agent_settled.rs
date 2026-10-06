//! Acceptance tests for package P553 r2 (#553): one documented `settled` wait.
//!
//! Written by the conductor before the build and frozen: the builder must not
//! edit this file. Each test names the edge-table row (E*) it covers.
//!
//! `settled` is OBSERVED QUIESCENCE, not proof that a turn produced a result:
//! the agent's status, as the server observed it, entered `working` after a
//! cursor and then held idle/done with no state transition at all for the
//! settle window.
//!
//! The agent status is driven through `pane.report_agent` with a non-native
//! source (`flock:pi`), so every state sequence here is deterministic.
//! `a0_*` calibrates that harness and passes on main as well; every other test
//! fails on main.

// TracedCommand (logging redesign PR-3) polices flock's shipped code; this
// harness drives the compiled flock binary through raw Command.
#![allow(clippy::disallowed_methods)]

mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use support::{
    cleanup_test_base, register_runtime_dir, register_spawned_flock_pid,
    unregister_spawned_flock_pid, wait_for_socket,
};

/// Short settle window so the suite stays fast; the default is a contract
/// of its own (E11).
const SETTLE: &str = "600";
const SETTLE_MS: u64 = 600;
/// Time for a freshly spawned waiter to take its first sample.
const WAITER_START: Duration = Duration::from_millis(400);

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
        // Bounded reap, as `tests/cli_wrapper.rs` does: a blocking
        // `child.wait()` here never returns on macOS, and every test in the
        // binary then hangs at teardown until CI cancels the job.
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
    let base = PathBuf::from(format!("/tmp/hfin-{}-{nanos}", std::process::id()));
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
    cmd.env("XDG_CONFIG_HOME", &config_home);
    cmd.env("XDG_RUNTIME_DIR", &runtime_dir);
    cmd.env("FLOCK_SOCKET_PATH", &socket);
    cmd.env_remove("FLOCK_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("FLOCK_ENV");
    cmd.env_remove("FLOCK_HOST_NAME");
    cmd.env_remove("FLOCK_DISABLE_SOUND");
    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_flock_pid(child.process_id());
    wait_for_socket(&socket, Duration::from_secs(5));
    Server {
        _master: pair.master,
        child,
        base,
        socket,
    }
}

fn request(server: &Server, json: &str) -> serde_json::Value {
    let mut stream = UnixStream::connect(&server.socket).unwrap();
    stream.write_all(json.as_bytes()).unwrap();
    stream.write_all(b"\n").unwrap();
    stream.flush().unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn cli(server: &Server, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_flk"))
        .args(args)
        .env("FLOCK_SOCKET_PATH", &server.socket)
        .output()
        .unwrap()
}

/// A wait running in the background, so the test can change the agent's
/// status while it blocks.
fn cli_spawn(server: &Server, args: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_flk"))
        .args(args)
        .env("FLOCK_SOCKET_PATH", &server.socket)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// Poll a background wait until it exits or `within` passes.
fn exited_within(child: &mut Child, within: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        thread::sleep(Duration::from_millis(20));
    }
    None
}

/// Collect a background wait's output; a waiter still running is killed
/// first, so a failing test never hangs on it.
fn finish(mut child: Child) -> Output {
    if child.try_wait().unwrap().is_none() {
        let _ = child.kill();
    }
    child.wait_with_output().unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn all_text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn stdout_json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|err| {
        panic!(
            "stdout is not one JSON value ({err}): {:?} / stderr {:?}",
            String::from_utf8_lossy(&out.stdout),
            stderr(out)
        )
    })
}

/// A focused workspace whose root pane is named `worker` and reported as an
/// idle `pi` agent. Returns the pane id.
fn worker_pane(server: &Server) -> String {
    let created = request(
        server,
        &format!(
            r#"{{"id":"ws","method":"workspace.create","params":{{"cwd":"{}","focus":true}}}}"#,
            server.base.display()
        ),
    );
    let workspace_id = created["result"]["workspace"]["workspace_id"]
        .as_str()
        .unwrap_or_else(|| panic!("workspace.create: {created}"))
        .to_string();
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| format!("{workspace_id}-1"));
    report(server, &pane_id, "idle");
    let named = cli(server, &["agent", "rename", &pane_id, "worker"]);
    assert!(named.status.success(), "rename: {}", stderr(&named));
    pane_id
}

fn report(server: &Server, pane_id: &str, state: &str) {
    let reported = request(
        server,
        &format!(
            r#"{{"id":"rep","method":"pane.report_agent","params":{{"pane_id":"{pane_id}","source":"flock:pi","agent":"pi","state":"{state}"}}}}"#
        ),
    );
    assert_eq!(
        reported["result"]["type"], "ok",
        "report {state}: {reported}"
    );
}

fn agent_get(server: &Server) -> serde_json::Value {
    let out = cli(server, &["agent", "get", "worker"]);
    assert!(out.status.success(), "agent get: {}", stderr(&out));
    stdout_json(&out)["result"]["agent"].clone()
}

/// The opaque turn cursor from `flk agent get` (E1).
fn cursor(server: &Server) -> String {
    let agent = agent_get(server);
    agent["turn_cursor"]
        .as_str()
        .unwrap_or_else(|| panic!("agent get has no turn_cursor string: {agent}"))
        .to_string()
}

fn settled_wait(server: &Server, after: Option<&str>, timeout: &str) -> Child {
    let mut args = vec!["agent", "wait", "worker", "--status", "settled"];
    if let Some(after) = after {
        args.extend(["--after", after]);
    }
    args.extend(["--settle", SETTLE, "--timeout", timeout]);
    cli_spawn(server, &args)
}

/// Calibration: a reported `working` holds for longer than any settle window
/// used below, so a test that waits through it is not racing the server.
/// Passes on main.
#[test]
fn a0_reported_working_holds_on_a_plain_pane() {
    let server = start_server();
    let pane = worker_pane(&server);
    report(&server, &pane, "working");
    thread::sleep(Duration::from_millis(4 * SETTLE_MS));
    assert_eq!(agent_get(&server)["agent_status"], "working");
}

/// E1, E2: no cursor, an agent already idle past the settle window is
/// settled; the result line names the status, the pane and the cursor.
#[test]
fn a1_settled_without_cursor_returns_for_an_idle_agent() {
    let server = start_server();
    let pane = worker_pane(&server);
    let before = cursor(&server);
    let started = Instant::now();
    let out = cli(
        &server,
        &[
            "agent",
            "wait",
            "worker",
            "--status",
            "settled",
            "--settle",
            SETTLE,
            "--timeout",
            "5000",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(started.elapsed() < Duration::from_millis(4000));
    let line = stdout_json(&out);
    assert_eq!(line["status"], "settled", "{line}");
    assert_eq!(line["pane_id"], pane.as_str(), "{line}");
    assert!(
        matches!(line["agent_status"].as_str(), Some("idle" | "done")),
        "{line}"
    );
    assert_eq!(line["turn_cursor"], before.as_str(), "{line}");
    assert!(
        line["held_ms"].as_u64().is_some_and(|ms| ms >= SETTLE_MS),
        "{line}"
    );
}

/// E3: with `--after` captured while idle, an agent still idle from the
/// previous turn is NOT settled; the wait returns only after it enters
/// working and goes idle again.
#[test]
fn a2_after_cursor_requires_a_new_working_entry() {
    let server = start_server();
    let pane = worker_pane(&server);
    let before = cursor(&server);
    let mut wait = settled_wait(&server, Some(&before), "10000");
    assert!(
        exited_within(&mut wait, Duration::from_millis(3 * SETTLE_MS)).is_none(),
        "returned before the agent started working: {:?}",
        finish(wait)
    );
    report(&server, &pane, "working");
    thread::sleep(Duration::from_millis(300));
    report(&server, &pane, "idle");
    let status = exited_within(&mut wait, Duration::from_secs(5));
    let out = finish(wait);
    assert_eq!(
        status.and_then(|s| s.code()),
        Some(0),
        "stderr: {}",
        stderr(&out)
    );
    let line = stdout_json(&out);
    assert_eq!(line["status"], "settled", "{line}");
    assert_ne!(
        line["turn_cursor"],
        before.as_str(),
        "cursor did not advance: {line}"
    );
    assert_eq!(line["turn_cursor"], cursor(&server).as_str());
}

/// E4: an idle blip shorter than the settle window inside a turn is not
/// settled; the wait returns only a full settle window after the LAST idle.
#[test]
fn a3_mid_turn_idle_blip_does_not_settle() {
    let server = start_server();
    let pane = worker_pane(&server);
    let before = cursor(&server);
    let mut wait = settled_wait(&server, Some(&before), "10000");
    thread::sleep(WAITER_START);
    report(&server, &pane, "working");
    thread::sleep(Duration::from_millis(200));
    report(&server, &pane, "idle");
    thread::sleep(Duration::from_millis(SETTLE_MS / 3));
    report(&server, &pane, "working");
    assert!(
        exited_within(&mut wait, Duration::from_millis(2 * SETTLE_MS)).is_none(),
        "settled on a mid-turn blip: {:?}",
        finish(wait)
    );
    let before_last_idle = Instant::now();
    report(&server, &pane, "idle");
    let status = exited_within(&mut wait, Duration::from_secs(5));
    let returned_after = before_last_idle.elapsed();
    let out = finish(wait);
    assert_eq!(
        status.and_then(|s| s.code()),
        Some(0),
        "stderr: {}",
        stderr(&out)
    );
    assert!(
        returned_after >= Duration::from_millis(SETTLE_MS),
        "returned {returned_after:?} after the last idle, settle is {SETTLE_MS} ms"
    );
}

/// E5: a whole working → idle sequence that happened before the wait was
/// issued still counts, because the cursor (not a live event) records it.
#[test]
fn a4_turn_completed_before_the_wait_still_counts() {
    let server = start_server();
    let pane = worker_pane(&server);
    let before = cursor(&server);
    report(&server, &pane, "working");
    report(&server, &pane, "idle");
    let out = cli(
        &server,
        &[
            "agent",
            "wait",
            "worker",
            "--status",
            "settled",
            "--after",
            &before,
            "--settle",
            SETTLE,
            "--timeout",
            "5000",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stdout_json(&out)["status"], "settled");
}

/// E6: a blocked agent is not settled; a `blocked` held for the settle
/// window ends the wait with exit 3 and status "blocked", even before a new
/// working entry.
#[test]
fn a5_held_blocked_exits_3() {
    let server = start_server();
    let pane = worker_pane(&server);
    let before = cursor(&server);
    report(&server, &pane, "blocked");
    let out = cli(
        &server,
        &[
            "agent",
            "wait",
            "worker",
            "--status",
            "settled",
            "--after",
            &before,
            "--settle",
            SETTLE,
            "--timeout",
            "5000",
        ],
    );
    assert_eq!(out.status.code(), Some(3), "stderr: {}", stderr(&out));
    let line = stdout_json(&out);
    assert_eq!(line["status"], "blocked", "{line}");
    assert_eq!(line["agent_status"], "blocked", "{line}");
}

/// E7: no working entry within the timeout exits 124, distinct from errors.
#[test]
fn a6_timeout_exits_124() {
    let server = start_server();
    let _pane = worker_pane(&server);
    let before = cursor(&server);
    let started = Instant::now();
    let out = cli(
        &server,
        &[
            "agent",
            "wait",
            "worker",
            "--status",
            "settled",
            "--after",
            &before,
            "--settle",
            SETTLE,
            "--timeout",
            "1500",
        ],
    );
    assert_eq!(out.status.code(), Some(124), "stderr: {}", stderr(&out));
    assert!(started.elapsed() >= Duration::from_millis(1400));
    assert!(started.elapsed() < Duration::from_millis(5000));
}

/// E8: a cursor from another terminal, a malformed one, or one ahead of the
/// server's counters is refused with exit 2, never read as "it worked".
#[test]
fn a7_foreign_or_malformed_cursor_is_refused() {
    let server = start_server();
    let _pane = worker_pane(&server);
    let own = cursor(&server);
    let fields: Vec<&str> = own.split(':').collect();
    assert_eq!(
        fields.len(),
        5,
        "cursor is <terminal>:<epoch>:<entries>:<seq>:<w|i>: {own}"
    );
    let foreign = format!("term_bogus:{}", fields[1..].join(":"));
    let ahead = format!(
        "{}:{}:{}:{}:{}",
        fields[0],
        fields[1],
        fields[2].parse::<u64>().unwrap() + 5,
        fields[3].parse::<u64>().unwrap() + 5,
        fields[4]
    );
    for bad in [foreign.as_str(), "not-a-cursor", ahead.as_str()] {
        let out = cli(
            &server,
            &[
                "agent",
                "wait",
                "worker",
                "--status",
                "settled",
                "--after",
                bad,
                "--settle",
                SETTLE,
                "--timeout",
                "3000",
            ],
        );
        assert_eq!(out.status.code(), Some(2), "{bad}: stderr {}", stderr(&out));
        assert!(stderr(&out).contains("cursor"), "{bad}: {}", stderr(&out));
    }
}

/// E9: `flk wait agent-status <pane> --status settled` is the same signal,
/// with the same result line and exit codes.
#[test]
fn a8_wait_agent_status_settled_matches_agent_wait() {
    let server = start_server();
    let pane = worker_pane(&server);
    let before = cursor(&server);
    report(&server, &pane, "working");
    report(&server, &pane, "idle");
    let out = cli(
        &server,
        &[
            "wait",
            "agent-status",
            &pane,
            "--status",
            "settled",
            "--after",
            &before,
            "--settle",
            SETTLE,
            "--timeout",
            "5000",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let line = stdout_json(&out);
    assert_eq!(line["status"], "settled", "{line}");
    assert_eq!(line["pane_id"], pane.as_str(), "{line}");

    let now = line["turn_cursor"].as_str().unwrap().to_string();
    let out = cli(
        &server,
        &[
            "wait",
            "agent-status",
            &pane,
            "--status",
            "settled",
            "--after",
            &now,
            "--settle",
            SETTLE,
            "--timeout",
            "1200",
        ],
    );
    assert_eq!(out.status.code(), Some(124), "stderr: {}", stderr(&out));
}

/// E10: one vocabulary. `--status idle` means "ready for input" in BOTH
/// commands, so it matches an unseen `done` pane in `wait agent-status` too
/// (the #553 hang), and `agent wait` no longer refuses `done`.
#[test]
fn a9_idle_matches_done_in_both_commands() {
    let server = start_server();
    let pane = worker_pane(&server);
    let workspace_id = agent_get(&server)["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();
    report(&server, &pane, "working");
    let tab = request(
        &server,
        &format!(
            r#"{{"id":"tab","method":"tab.create","params":{{"workspace_id":"{workspace_id}","focus":true}}}}"#
        ),
    );
    assert_eq!(tab["result"]["type"], "tab_created", "{tab}");
    report(&server, &pane, "idle");
    assert_eq!(
        agent_get(&server)["agent_status"],
        "done",
        "precondition: an idle pane in a background tab reports done"
    );

    let out = cli(
        &server,
        &[
            "wait",
            "agent-status",
            &pane,
            "--status",
            "idle",
            "--timeout",
            "2000",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));

    let out = cli(
        &server,
        &[
            "agent",
            "wait",
            "worker",
            "--status",
            "done",
            "--timeout",
            "2000",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
}

/// E10: every help surface a user reaches documents the same vocabulary,
/// and the verb help explains `settled`, its flags, the cursor, what `done`
/// means, and that settled is observed quiescence (not a turn result).
#[test]
fn a10_help_documents_one_vocabulary() {
    let server = start_server();
    let vocab = "idle|working|blocked|done|unknown|hibernated|settled";
    let surfaces: [&[&str]; 4] = [
        &["agent", "wait", "--help"],
        &["wait", "agent-status", "--help"],
        &["agent", "--help"],
        &["wait", "--help"],
    ];
    for args in surfaces {
        let out = cli(&server, args);
        assert_eq!(out.status.code(), Some(0), "{args:?}: {}", all_text(&out));
        let text = all_text(&out);
        assert!(
            text.contains(vocab),
            "{args:?} help lacks `{vocab}`:\n{text}"
        );
    }
    let verb_help = all_text(&cli(&server, &["agent", "wait", "--help"]));
    for needle in [
        "--after",
        "--settle",
        "turn_cursor",
        "unseen",
        "observed",
        "agent result",
    ] {
        assert!(
            verb_help.contains(needle),
            "agent wait --help lacks {needle}:\n{verb_help}"
        );
    }
    let wait_help = all_text(&cli(&server, &["wait", "agent-status", "--help"]));
    for needle in ["--after", "--settle"] {
        assert!(
            wait_help.contains(needle),
            "wait agent-status --help lacks {needle}:\n{wait_help}"
        );
    }
}

/// E12: the pane going away ends the wait with exit 4 and a "gone" result
/// line, not a hang until the timeout.
#[test]
fn a11_pane_closed_while_waiting_exits_4() {
    let server = start_server();
    let pane = worker_pane(&server);
    let before = cursor(&server);
    let mut wait = settled_wait(&server, Some(&before), "15000");
    thread::sleep(WAITER_START);
    let closed = cli(&server, &["pane", "close", &pane]);
    assert!(closed.status.success(), "pane close: {}", stderr(&closed));
    let status = exited_within(&mut wait, Duration::from_secs(5));
    let out = finish(wait);
    assert_eq!(
        status.and_then(|s| s.code()),
        Some(4),
        "stderr: {}",
        stderr(&out)
    );
    let line = stdout_json(&out);
    assert_eq!(line["status"], "gone", "{line}");
    assert_eq!(line["pane_id"], pane.as_str(), "{line}");
}

/// E11: the default settle window is 5000 ms: without `--settle`, a fresh
/// idle is not settled for at least 5 s.
#[test]
fn a12_default_settle_is_five_seconds() {
    let server = start_server();
    let pane = worker_pane(&server);
    let before = cursor(&server);
    report(&server, &pane, "working");
    let before_idle = Instant::now();
    report(&server, &pane, "idle");
    let out = cli(
        &server,
        &[
            "agent",
            "wait",
            "worker",
            "--status",
            "settled",
            "--after",
            &before,
            "--timeout",
            "12000",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(
        before_idle.elapsed() >= Duration::from_millis(5000),
        "{:?}",
        before_idle.elapsed()
    );
    assert!(stdout_json(&out)["held_ms"]
        .as_u64()
        .is_some_and(|ms| ms >= 5000));
}

/// E13: the cursor is on the pane record and on `agent list` entries too,
/// identical to the agent's.
#[test]
fn a13_pane_get_and_agent_list_carry_the_same_cursor() {
    let server = start_server();
    let pane = worker_pane(&server);
    let own = cursor(&server);
    let got = request(
        &server,
        &format!(r#"{{"id":"pg","method":"pane.get","params":{{"pane_id":"{pane}"}}}}"#),
    );
    assert_eq!(
        got["result"]["pane"]["turn_cursor"],
        own.as_str(),
        "pane.get: {got}"
    );

    let listed = cli(&server, &["agent", "list"]);
    assert!(listed.status.success(), "agent list: {}", stderr(&listed));
    let listed = stdout_json(&listed);
    let agents = listed["result"]["agents"]
        .as_array()
        .unwrap_or_else(|| panic!("agent list: {listed}"));
    let worker = agents
        .iter()
        .find(|agent| agent["name"] == "worker")
        .unwrap_or_else(|| panic!("worker not listed: {listed}"));
    assert_eq!(worker["turn_cursor"], own.as_str(), "{worker}");
}

/// E14: any state excursion resets the dwell, not only a new working entry:
/// idle → blocked → idle inside the settle window (no working at all) must
/// not settle until a full window after the last idle.
#[test]
fn a14_non_working_excursion_resets_the_settle_window() {
    let server = start_server();
    let pane = worker_pane(&server);
    let before = cursor(&server);
    report(&server, &pane, "working");
    report(&server, &pane, "idle");
    let mut wait = settled_wait(&server, Some(&before), "10000");
    thread::sleep(Duration::from_millis(SETTLE_MS / 2));
    report(&server, &pane, "blocked");
    let before_last_idle = Instant::now();
    report(&server, &pane, "idle");
    let status = exited_within(&mut wait, Duration::from_secs(5));
    let returned_after = before_last_idle.elapsed();
    let out = finish(wait);
    assert_eq!(
        status.and_then(|s| s.code()),
        Some(0),
        "stderr: {}",
        stderr(&out)
    );
    assert!(
        returned_after >= Duration::from_millis(SETTLE_MS),
        "returned {returned_after:?} after the last idle, settle is {SETTLE_MS} ms"
    );
}

/// E15: a cursor captured while the agent is already working (a follow-up
/// queued into a running turn) settles at the next quiescence, with no new
/// working entry needed.
#[test]
fn a15_cursor_captured_while_working_settles_without_a_new_entry() {
    let server = start_server();
    let pane = worker_pane(&server);
    report(&server, &pane, "working");
    let during = cursor(&server);
    let mut wait = settled_wait(&server, Some(&during), "10000");
    assert!(
        exited_within(&mut wait, Duration::from_millis(2 * SETTLE_MS)).is_none(),
        "settled while still working: {:?}",
        finish(wait)
    );
    report(&server, &pane, "idle");
    let status = exited_within(&mut wait, Duration::from_secs(5));
    let out = finish(wait);
    assert_eq!(
        status.and_then(|s| s.code()),
        Some(0),
        "stderr: {}",
        stderr(&out)
    );
    assert_eq!(stdout_json(&out)["status"], "settled");
}

/// E16: flag misuse is a usage error (exit 2), checked before any waiting.
#[test]
fn a16_flag_misuse_exits_2() {
    let server = start_server();
    let _pane = worker_pane(&server);
    let own = cursor(&server);
    let cases: [&[&str]; 5] = [
        &[
            "agent", "wait", "worker", "--status", "idle", "--after", &own,
        ],
        &[
            "agent", "wait", "worker", "--status", "idle", "--settle", "100",
        ],
        &[
            "agent", "wait", "worker", "--status", "settled", "--settle", "abc",
        ],
        &["agent", "wait", "worker", "--ready", "--settle", "100"],
        &[
            "wait",
            "agent-status",
            "w1:p1",
            "--status",
            "working",
            "--after",
            &own,
        ],
    ];
    for args in cases {
        let started = Instant::now();
        let out = cli(&server, args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", all_text(&out));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{args:?} waited"
        );
    }
}
