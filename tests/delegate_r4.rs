//! Acceptance tests for package P578 r4 (#578): the redesign after the
//! design-work tripwire (see `.conductor/P578.r4-delta.md`).
//!
//! Written by the conductor before the build and frozen: the builder must not
//! edit this file. The fixture below is copied verbatim from the frozen
//! `tests/delegate.rs` (a fake `opencode` whose screen the test draws, an
//! opencode database under an isolated `XDG_DATA_HOME`, HOME/worktree/git
//! isolation). a20, a21 and a23 fail on the round-2 head 6ddf746; a22 is a
//! regression guard for the settled phase and passes there.

// TracedCommand (logging redesign PR-3) polices flock's shipped code; this
// harness drives the compiled flock binary through raw Command.
#![allow(clippy::disallowed_methods)]
// The shared fixture is copied whole; not every helper is used here.
#![allow(dead_code)]

mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use support::{
    cleanup_test_base, register_runtime_dir, register_spawned_flock_pid,
    unregister_spawned_flock_pid, wait_for_socket,
};

/// Short settle window so the suite stays fast.
const SETTLE: &str = "300";
const SETTLE_MS: u64 = 300;
/// Generous bound for anything the test waits on.
const WITHIN: Duration = Duration::from_secs(20);
/// The session id the test reports for every delegate.
const SES: &str = "ses_p578";

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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn bin_dir(base: &Path) -> PathBuf {
    base.join("bin")
}

fn search_path(base: &Path) -> String {
    format!(
        "{}:{}",
        bin_dir(base).display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// The fake harness: records its argv and cwd, dies at once when
/// `<base>/die` exists, redraws its screen whenever `<base>/screen` changes,
/// and appends every line typed into its pane to `<base>/typed.log`.
fn write_fake_opencode(base: &Path) {
    let bin = bin_dir(base);
    fs::create_dir_all(&bin).unwrap();
    let script = format!(
        "#!/bin/sh\n\
         printf '%s\\n' \"$*\" >> '{base}/argv.log'\n\
         printf '%s\\n' \"$PWD\" >> '{base}/cwd.log'\n\
         if [ -e '{base}/die' ]; then exit 1; fi\n\
         ( last=; while :; do now=$(cat '{base}/screen' 2>/dev/null); \
         if [ \"$now\" != \"$last\" ]; then printf '\\033[2J\\033[H%s\\n' \"$now\"; last=$now; fi; \
         sleep 0.05; done ) &\n\
         first=1; while IFS= read -r line; do printf '%s\\n' \"$line\" >> '{base}/typed.log'; \
         if [ ! -e '{base}/manual-submit' ]; then first=; printf '\\033[2J\\033[H■■■■⬝⬝  esc interrupt  opencode\\n'; sleep 0.4; \
         printf '\\033[2J\\033[H%s\\n' \"$(cat '{base}/screen')\"; fi; done\n",
        base = base.display()
    );
    let path = bin.join("opencode");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn db_path(base: &Path) -> PathBuf {
    base.join("data")
        .join("opencode")
        .join("opencode-stable.db")
}

fn create_db(base: &Path) {
    fs::create_dir_all(db_path(base).parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(db_path(base)).unwrap();
    conn.execute_batch(
        "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, \
         time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
         CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, \
         session_id TEXT NOT NULL, time_created INTEGER NOT NULL, \
         time_updated INTEGER NOT NULL, data TEXT NOT NULL);",
    )
    .unwrap();
}

fn start_server() -> Server {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let base = PathBuf::from(format!("/tmp/hdel-{}-{nanos}", std::process::id()));
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket = runtime_dir.join("flock.sock");
    fs::create_dir_all(config_home.join(app_dir_name())).unwrap();
    fs::create_dir_all(&runtime_dir).unwrap();
    fs::create_dir_all(base.join("work")).unwrap();
    fs::create_dir_all(base.join("briefs")).unwrap();
    fs::create_dir_all(base.join("state")).unwrap();
    register_runtime_dir(&runtime_dir);
    fs::create_dir_all(base.join("home")).unwrap();
    fs::write(
        config_home.join(app_dir_name()).join("config.toml"),
        format!(
            "onboarding = false\n\n[worktrees]\ndirectory = \"{}\"\n",
            base.join("wt").display()
        ),
    )
    .unwrap();
    write_fake_opencode(&base);
    create_db(&base);

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
    cmd.cwd(&base);
    cmd.env("XDG_CONFIG_HOME", &config_home);
    cmd.env("XDG_RUNTIME_DIR", &runtime_dir);
    cmd.env("XDG_DATA_HOME", base.join("data"));
    cmd.env("XDG_STATE_HOME", base.join("state"));
    cmd.env("HOME", base.join("home"));
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("PATH", search_path(&base));
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

fn command(server: &Server, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_flk"));
    cmd.args(args)
        .current_dir(&server.base)
        .env("FLOCK_SOCKET_PATH", &server.socket)
        .env_remove("FLOCK_CLIENT_SOCKET_PATH")
        .env("XDG_STATE_HOME", server.base.join("state"))
        .env("XDG_DATA_HOME", server.base.join("data"))
        .env("XDG_CONFIG_HOME", server.base.join("config"))
        .env("HOME", server.base.join("home"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("PATH", search_path(&server.base));
    cmd
}

fn cli(server: &Server, args: &[&str]) -> Output {
    command(server, args).output().unwrap()
}

/// A delegate verb running in the background, so the test can play the
/// harness while it blocks.
fn cli_spawn(server: &Server, args: &[&str]) -> Child {
    command(server, args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

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

/// Collect a background verb's output; one still running is killed first, so
/// a failing test never hangs on it.
fn finish(mut child: Child) -> Output {
    if child.try_wait().unwrap().is_none() {
        let _ = child.kill();
    }
    child.wait_with_output().unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stdout_json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|err| {
        panic!(
            "stdout is not one JSON value ({err}): {:?} / stderr {:?}",
            stdout(out),
            stderr(out)
        )
    })
}

fn brief(server: &Server, name: &str, body: &str) -> String {
    let path = server.base.join("briefs").join(name);
    fs::write(&path, body).unwrap();
    path.to_string_lossy().into_owned()
}

fn expected_line(brief_path: &str) -> String {
    format!("Read {brief_path} and execute it exactly.")
}

fn read_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn typed(server: &Server) -> Vec<String> {
    read_lines(&server.base.join("typed.log"))
}

/// Block until the fake harness has received `n` submitted lines.
fn wait_typed(server: &Server, n: usize) -> Vec<String> {
    let deadline = Instant::now() + WITHIN;
    loop {
        let lines = typed(server);
        if lines.len() >= n {
            return lines;
        }
        assert!(
            Instant::now() < deadline,
            "the harness received {} line(s), expected {n}: {lines:?}",
            lines.len()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn agent_get(server: &Server, name: &str) -> Option<serde_json::Value> {
    let out = cli(server, &["agent", "get", name]);
    out.status
        .success()
        .then(|| stdout_json(&out)["result"]["agent"].clone())
}

/// Wait for the delegate's agent to exist and return its pane id.
fn delegate_pane(server: &Server, name: &str) -> String {
    let deadline = Instant::now() + WITHIN;
    loop {
        if let Some(agent) = agent_get(server, name) {
            if let Some(pane) = agent["pane_id"].as_str() {
                return pane.to_string();
            }
        }
        assert!(Instant::now() < deadline, "agent {name} never appeared");
        thread::sleep(Duration::from_millis(30));
    }
}

/// What the fake harness draws for each status, as opencode would.
fn screen_for(state: &str) -> &'static str {
    match state {
        "working" => "\u{25a0}\u{25a0}\u{25a0}\u{25a0}\u{2b1d}\u{2b1d}  esc interrupt  opencode",
        "blocked" => "\u{25b3} Permission required",
        "idle" => "┃\n┃  Ask anything…\n┃\n┃  Build test-model\n╹\ntab agents ctrl+p commands\n",
        other => panic!("no screen for {other}"),
    }
}

/// Make the delegate's pane show `state`, and wait until flock reports it
/// (`idle` may read as `done` on a pane nobody looks at).
fn report(server: &Server, pane_id: &str, state: &str) {
    fs::write(server.base.join("screen"), screen_for(state)).unwrap();
    let deadline = Instant::now() + WITHIN;
    loop {
        let got = request(
            server,
            &format!(r#"{{"id":"ag","method":"agent.get","params":{{"target":"{pane_id}"}}}}"#),
        );
        let status = got["result"]["agent"]["agent_status"]
            .as_str()
            .unwrap_or("");
        if status == state || (state == "idle" && status == "done") {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "pane {pane_id} never showed {state}: {got}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn report_session(server: &Server, pane_id: &str) {
    let reported = request(
        server,
        &format!(
            r#"{{"id":"ses","method":"pane.report_agent_session","params":{{"pane_id":"{pane_id}","source":"flock:opencode","agent":"opencode","agent_session_id":"{SES}"}}}}"#
        ),
    );
    assert!(reported.get("error").is_none(), "{reported}");
}

/// Write one user message and one finished assistant reply, timed now.
fn write_reply(server: &Server, turn: &str, text: &str) {
    let mut conn = rusqlite::Connection::open(db_path(&server.base)).unwrap();
    let tx = conn.transaction().unwrap();
    let at = now_ms() as i64;
    let user = format!("{turn}-u");
    let assistant = format!("{turn}-a");
    let user_data = serde_json::json!({"role": "user"}).to_string();
    let user_part = serde_json::json!({"type": "text", "text": "brief"}).to_string();
    let reply_data =
        serde_json::json!({"role": "assistant", "time": {"completed": at + 1}}).to_string();
    let reply_part = serde_json::json!({"type": "text", "text": text}).to_string();
    tx.execute(
        "INSERT INTO message VALUES (?1, ?2, ?3, ?3, ?4)",
        rusqlite::params![user, SES, at, user_data],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
        rusqlite::params![format!("{user}-p"), user, SES, at, user_part],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO message VALUES (?1, ?2, ?3, ?3, ?4)",
        rusqlite::params![assistant, SES, at + 1, reply_data],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
        rusqlite::params![format!("{assistant}-p"), assistant, SES, at + 1, reply_part],
    )
    .unwrap();
    tx.commit().unwrap();
}

/// Play the plugin through readiness: the agent exists, names its session,
/// and shows idle.
fn make_ready(server: &Server, name: &str) -> String {
    let pane = delegate_pane(server, name);
    report_session(server, &pane);
    report(server, &pane, "idle");
    pane
}

/// Play one whole turn: working, the reply lands, idle.
fn play_turn(server: &Server, pane: &str, turn: &str, text: &str) {
    report(server, pane, "working");
    thread::sleep(Duration::from_millis(100));
    write_reply(server, turn, text);
    report(server, pane, "idle");
}

fn workspaces(server: &Server) -> Vec<serde_json::Value> {
    let listed = request(
        server,
        r#"{"id":"wl","method":"workspace.list","params":{}}"#,
    );
    listed["result"]["workspaces"]
        .as_array()
        .unwrap_or_else(|| panic!("workspace.list: {listed}"))
        .clone()
}

fn focused_workspace(server: &Server) -> Option<String> {
    workspaces(server)
        .iter()
        .find(|ws| ws["focused"] == true)
        .and_then(|ws| ws["workspace_id"].as_str().map(str::to_string))
}

/// A focused workspace the operator is "working in", so a delegate that
/// steals focus or lands in it is caught.
fn operator_workspace(server: &Server) -> String {
    let created = request(
        server,
        &format!(
            r#"{{"id":"ws","method":"workspace.create","params":{{"cwd":"{}","focus":true}}}}"#,
            server.base.display()
        ),
    );
    created["result"]["workspace"]["workspace_id"]
        .as_str()
        .unwrap_or_else(|| panic!("workspace.create: {created}"))
        .to_string()
}

fn work_dir(server: &Server) -> String {
    server.base.join("work").to_string_lossy().into_owned()
}

fn start_cwd(server: &Server, name: &str, brief_path: &str, extra: &[&str]) -> Child {
    let work = work_dir(server);
    let mut args = vec![
        "delegate", "start", name, "--brief", brief_path, "--cwd", &work, "--settle", SETTLE,
    ];
    args.extend_from_slice(extra);
    cli_spawn(server, &args)
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .env("HOME", dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", stderr(&out));
}

fn committed_repo(server: &Server) -> PathBuf {
    let repo = server.base.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("README.md"), "repo\n").unwrap();
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    repo
}

/// E20: a repository-root workspace the operator had open BEFORE the start is
/// not the delegate's, so neither the start nor the reap may close it.
#[test]
fn a20_an_operator_repo_root_workspace_survives_start_and_reap() {
    let server = start_server();
    operator_workspace(&server);
    let repo = committed_repo(&server);
    let repo_s = repo.to_string_lossy().into_owned();
    let opened = request(
        &server,
        &format!(
            r#"{{"id":"rw","method":"workspace.create","params":{{"cwd":"{repo_s}","focus":false}}}}"#
        ),
    );
    let repo_ws = opened["result"]["workspace"]["workspace_id"]
        .as_str()
        .unwrap_or_else(|| panic!("workspace.create: {opened}"))
        .to_string();
    let b = brief(&server, "task.md", "x\n");
    let mut child = cli_spawn(
        &server,
        &[
            "delegate",
            "start",
            "w1",
            "--brief",
            &b,
            "--worktree",
            "--repo",
            &repo_s,
            "--branch",
            "feat/a20",
            "--json",
        ],
    );
    make_ready(&server, "w1");
    let status = exited_within(&mut child, WITHIN).expect("start returns");
    let out = finish(child);
    assert_eq!(status.code(), Some(0), "stderr {}", stderr(&out));
    let reaped = cli(&server, &["delegate", "reap", "w1", "--json"]);
    assert_eq!(reaped.status.code(), Some(0), "reap: {}", stderr(&reaped));
    assert!(
        workspaces(&server)
            .iter()
            .any(|ws| ws["workspace_id"] == repo_ws.as_str()),
        "the operator's repo-root workspace {repo_ws} survived"
    );
}

/// E21: `--worktree` needs `--branch` (so a failed create can be found and
/// removed deterministically).
#[test]
fn a21_worktree_without_branch_is_a_usage_error() {
    let server = start_server();
    operator_workspace(&server);
    let before = workspaces(&server).len();
    let repo = committed_repo(&server);
    let repo_s = repo.to_string_lossy().into_owned();
    let b = brief(&server, "task.md", "x\n");
    let out = cli(
        &server,
        &[
            "delegate",
            "start",
            "w1",
            "--brief",
            &b,
            "--worktree",
            "--repo",
            &repo_s,
        ],
    );
    assert_eq!(out.status.code(), Some(2), "stderr {}", stderr(&out));
    assert!(stdout(&out).is_empty());
    assert_eq!(workspaces(&server).len(), before, "nothing was created");
    assert!(walk(&server.base.join("wt")).is_empty(), "no checkout");
}

/// E22 (regression guard; passes on 6ddf746 too): with the server frozen
/// (SIGSTOP) while `delegate wait` is still in the settled phase, the wait ends
/// at its timeout: exit 124 with the timeout outcome object.
#[test]
fn a22_a_frozen_server_cannot_hold_a_wait_past_its_deadline() {
    let server = start_server();
    operator_workspace(&server);
    let b = brief(&server, "task.md", "x\n");
    let mut child = start_cwd(&server, "d1", &b, &["--json"]);
    make_ready(&server, "d1");
    assert_eq!(
        exited_within(&mut child, WITHIN).and_then(|s| s.code()),
        Some(0)
    );
    finish(child);
    wait_typed(&server, 1);

    let pid = server.child.process_id().expect("server pid") as libc::pid_t;
    let mut waiter = cli_spawn(
        &server,
        &[
            "delegate",
            "wait",
            "d1",
            "--settle",
            SETTLE,
            "--timeout",
            "2000",
            "--json",
        ],
    );
    thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    let status = exited_within(&mut waiter, Duration::from_secs(8));
    unsafe { libc::kill(pid, libc::SIGCONT) };
    let status = status.expect("the wait must end while the server is frozen");
    let elapsed = started.elapsed();
    let out = finish(waiter);
    assert_eq!(status.code(), Some(124), "stderr {}", stderr(&out));
    assert!(
        elapsed < Duration::from_secs(5),
        "ended at its deadline, not later: {elapsed:?}"
    );
    assert_eq!(stdout_json(&out)["outcome"], "timeout");
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}

/// E23: the deadline also bounds the result grace. The agent went working →
/// idle but no reply is recorded yet, so the wait is polling `agent.result`
/// when the server is frozen. It must still end at its timeout with exit 124
/// (a request timeout there is the deadline, not a failure).
#[test]
fn a23_a_frozen_server_cannot_hold_the_result_grace_past_the_deadline() {
    let server = start_server();
    operator_workspace(&server);
    let b = brief(&server, "task.md", "x\n");
    let mut child = start_cwd(&server, "d1", &b, &["--json"]);
    let pane = make_ready(&server, "d1");
    assert_eq!(
        exited_within(&mut child, WITHIN).and_then(|s| s.code()),
        Some(0)
    );
    finish(child);
    wait_typed(&server, 1);
    report(&server, &pane, "working");
    thread::sleep(Duration::from_millis(100));
    report(&server, &pane, "idle");

    let pid = server.child.process_id().expect("server pid") as libc::pid_t;
    let mut waiter = cli_spawn(
        &server,
        &[
            "delegate",
            "wait",
            "d1",
            "--settle",
            SETTLE,
            "--timeout",
            "4000",
            "--json",
        ],
    );
    // Settled after ~SETTLE ms; then the grace polls agent.result.
    thread::sleep(Duration::from_millis(3 * SETTLE_MS + 400));
    assert!(
        waiter.try_wait().unwrap().is_none(),
        "no reply yet: the wait is in the result grace"
    );
    let started = Instant::now();
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    let status = exited_within(&mut waiter, Duration::from_secs(10));
    unsafe { libc::kill(pid, libc::SIGCONT) };
    let status = status.expect("the wait must end while the server is frozen");
    let elapsed = started.elapsed();
    let out = finish(waiter);
    assert_eq!(status.code(), Some(124), "stderr {}", stderr(&out));
    assert!(
        elapsed < Duration::from_secs(6),
        "ended at its deadline, not later: {elapsed:?}"
    );
    assert_eq!(stdout_json(&out)["outcome"], "timeout");
}

/// a24 (P578 r4-2): reaping a LIVE cwd delegate closes its own workspace.
///
/// The delegate's workspace is still open and its agent still runs, so the
/// identity check has positive evidence (the recorded terminal ids) and the
/// reap must act on it. The operator's workspace is untouched.
#[test]
fn a24_reap_closes_a_live_cwd_delegates_own_workspace() {
    let server = start_server();
    let operator = operator_workspace(&server);
    let b = brief(&server, "task.md", "x\n");
    let mut child = start_cwd(&server, "d1", &b, &["--json"]);
    make_ready(&server, "d1");
    let status = exited_within(&mut child, WITHIN).expect("start returns");
    let out = finish(child);
    assert_eq!(status.code(), Some(0), "stderr {}", stderr(&out));
    let started = stdout_json(&out);
    let delegate_ws = started["workspace_id"]
        .as_str()
        .unwrap_or_else(|| panic!("start --json names its workspace: {started}"))
        .to_string();
    assert!(
        workspaces(&server)
            .iter()
            .any(|ws| ws["workspace_id"] == delegate_ws.as_str()),
        "precondition: the delegate's workspace {delegate_ws} is open before the reap"
    );

    let reaped = cli(&server, &["delegate", "reap", "d1", "--json"]);
    assert_eq!(reaped.status.code(), Some(0), "reap: {}", stderr(&reaped));
    let after = workspaces(&server);
    assert!(
        after
            .iter()
            .all(|ws| ws["workspace_id"] != delegate_ws.as_str()),
        "the reap closed the delegate's own workspace {delegate_ws}: {after:?}"
    );
    assert!(
        after
            .iter()
            .any(|ws| ws["workspace_id"] == operator.as_str()),
        "the operator's workspace {operator} survived"
    );
}

/// The registry entry file for `name`, wherever the state dir keys it.
fn entry_file(server: &Server, name: &str) -> Option<PathBuf> {
    let want = format!("{name}.json");
    walk(&server.base.join("state"))
        .into_iter()
        .find(|path| path.file_name().and_then(|n| n.to_str()) == Some(want.as_str()))
}

/// The repo-root (non-linked) workspace whose checkout is `repo`, if one is open.
fn repo_root_workspace(server: &Server, repo: &Path) -> Option<String> {
    let want = fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    workspaces(server).iter().find_map(|ws| {
        let wt = &ws["worktree"];
        let path = wt["checkout_path"].as_str()?;
        let same = fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path)) == want;
        (same && wt["is_linked_worktree"] == false)
            .then(|| ws["workspace_id"].as_str().map(str::to_string))
            .flatten()
    })
}

/// a25 (P578 r4-3, K1): `reap` NEVER closes the parent workspace, even the one
/// `worktree.create` opened for this delegate. No repo-root workspace is open
/// before the start, so the server opens (and reports) a parent.
#[test]
fn a25_reap_never_closes_the_parent_the_create_opened() {
    let server = start_server();
    operator_workspace(&server);
    let repo = committed_repo(&server);
    let repo_s = repo.to_string_lossy().into_owned();
    assert!(
        repo_root_workspace(&server, &repo).is_none(),
        "precondition: no repo-root workspace before the start"
    );
    let b = brief(&server, "task.md", "x\n");
    let mut child = cli_spawn(
        &server,
        &[
            "delegate",
            "start",
            "w1",
            "--brief",
            &b,
            "--worktree",
            "--repo",
            &repo_s,
            "--branch",
            "feat/a25",
            "--json",
        ],
    );
    make_ready(&server, "w1");
    let status = exited_within(&mut child, WITHIN).expect("start returns");
    let out = finish(child);
    assert_eq!(status.code(), Some(0), "stderr {}", stderr(&out));
    let parent = repo_root_workspace(&server, &repo)
        .expect("precondition: worktree.create opened a repo-root parent workspace");

    let reaped = cli(&server, &["delegate", "reap", "w1", "--json"]);
    assert_eq!(reaped.status.code(), Some(0), "reap: {}", stderr(&reaped));
    assert!(
        workspaces(&server)
            .iter()
            .any(|ws| ws["workspace_id"] == parent.as_str()),
        "reap left the parent workspace {parent} open"
    );
}

/// a26 (P578 r4-3): an UNREACHABLE server is never "not a delegate" and never
/// permission. With the server dead, `reap` of a cwd delegate exits 1 and KEEPS
/// the entry (nothing could be shown to be ours, so nothing is forgotten), and
/// `status` / `wait` exit 1 with the transport error, not 2.
#[test]
fn a26_a_dead_server_is_a_failure_not_a_verdict() {
    let server = start_server();
    operator_workspace(&server);
    let b = brief(&server, "task.md", "x\n");
    let mut child = start_cwd(&server, "d1", &b, &["--json"]);
    make_ready(&server, "d1");
    assert_eq!(
        exited_within(&mut child, WITHIN).and_then(|s| s.code()),
        Some(0)
    );
    finish(child);
    assert!(
        entry_file(&server, "d1").is_some(),
        "precondition: entry written"
    );

    let pid = server.child.process_id().expect("server pid") as libc::pid_t;
    unsafe { libc::kill(pid, libc::SIGKILL) };
    thread::sleep(Duration::from_millis(300));

    let reaped = cli(&server, &["delegate", "reap", "d1", "--json"]);
    assert_eq!(reaped.status.code(), Some(1), "reap: {}", stderr(&reaped));
    assert_eq!(
        stdout(&reaped),
        "",
        "an exit-1 reap prints nothing on stdout"
    );
    assert!(
        entry_file(&server, "d1").is_some(),
        "the entry is KEPT, so the reap can be retried"
    );
    for args in [
        vec!["delegate", "status", "d1"],
        vec!["delegate", "wait", "d1", "--timeout", "3000", "--json"],
    ] {
        let out = cli(&server, &args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", stderr(&out));
        assert!(
            !stderr(&out).contains("not a delegate"),
            "{args:?} names the transport failure, not 'not a delegate': {}",
            stderr(&out)
        );
        assert_eq!(stdout(&out), "", "{args:?} prints nothing on stdout");
    }
}

/// a27 (P578 r4-3): a reap against a FROZEN server ends on its own, refuses
/// (exit 1), keeps the entry, and succeeds once the server answers again.
#[test]
fn a27_a_frozen_server_reap_refuses_then_retries() {
    let server = start_server();
    operator_workspace(&server);
    let b = brief(&server, "task.md", "x\n");
    let mut child = start_cwd(&server, "d1", &b, &["--json"]);
    make_ready(&server, "d1");
    let status = exited_within(&mut child, WITHIN).expect("start returns");
    let started = stdout_json(&finish(child));
    assert_eq!(status.code(), Some(0));
    let delegate_ws = started["workspace_id"].as_str().unwrap().to_string();

    let pid = server.child.process_id().expect("server pid") as libc::pid_t;
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    let mut reap = cli_spawn(&server, &["delegate", "reap", "d1", "--json"]);
    let status = exited_within(&mut reap, Duration::from_secs(15));
    unsafe { libc::kill(pid, libc::SIGCONT) };
    let status = status.expect("a reap against a frozen server must end on its own");
    let out = finish(reap);
    assert_eq!(status.code(), Some(1), "stderr {}", stderr(&out));
    assert!(entry_file(&server, "d1").is_some(), "the entry is kept");

    let retried = cli(&server, &["delegate", "reap", "d1", "--json"]);
    assert_eq!(
        retried.status.code(),
        Some(0),
        "retry: {}",
        stderr(&retried)
    );
    assert!(
        workspaces(&server)
            .iter()
            .all(|ws| ws["workspace_id"] != delegate_ws.as_str()),
        "the retried reap closed the delegate's workspace"
    );
    assert!(entry_file(&server, "d1").is_none(), "and removed the entry");
}

/// a28 (P578 r4-4): a server that freezes during READINESS cannot hold `start`.
/// The agent has launched but never reported ready; with the server stopped,
/// `start` must end on its own (`--ready-timeout` plus one 2 s cap plus slack)
/// with exit 1, and write no registry entry.
#[test]
fn a28_a_frozen_server_cannot_hold_start_in_readiness() {
    let server = start_server();
    operator_workspace(&server);
    let b = brief(&server, "task.md", "x\n");
    let mut child = start_cwd(&server, "d1", &b, &["--ready-timeout", "3000"]);
    let appeared = Instant::now() + WITHIN;
    while agent_get(&server, "d1").is_none() {
        assert!(
            Instant::now() < appeared,
            "the delegate's agent never appeared"
        );
        thread::sleep(Duration::from_millis(50));
    }
    thread::sleep(Duration::from_millis(300));
    let pid = server.child.process_id().expect("server pid") as libc::pid_t;
    let frozen = Instant::now();
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    let status = exited_within(&mut child, Duration::from_secs(20));
    let elapsed = frozen.elapsed();
    unsafe { libc::kill(pid, libc::SIGCONT) };
    let status = status.expect("start must end on its own while the server is frozen");
    let out = finish(child);
    assert_eq!(status.code(), Some(1), "stderr {}", stderr(&out));
    assert!(
        elapsed < Duration::from_secs(10),
        "start ended within ready-timeout + one cap + slack: {elapsed:?}"
    );
    assert!(
        entry_file(&server, "d1").is_none(),
        "no registry entry was written"
    );
}

/// a29 (P578 r4-4): a worktree delegate whose workspace AND checkout are already
/// gone (killed by hand) reaps cleanly: "already gone" is the goal, not an error.
#[test]
fn a29_reap_after_the_checkout_is_already_gone_succeeds() {
    let server = start_server();
    operator_workspace(&server);
    let repo = committed_repo(&server);
    let repo_s = repo.to_string_lossy().into_owned();
    let b = brief(&server, "task.md", "x\n");
    let mut child = cli_spawn(
        &server,
        &[
            "delegate",
            "start",
            "w1",
            "--brief",
            &b,
            "--worktree",
            "--repo",
            &repo_s,
            "--branch",
            "feat/a29",
            "--json",
        ],
    );
    make_ready(&server, "w1");
    let status = exited_within(&mut child, WITHIN).expect("start returns");
    let out = finish(child);
    assert_eq!(status.code(), Some(0), "stderr {}", stderr(&out));
    let started = stdout_json(&out);
    let ws = started["workspace_id"].as_str().unwrap().to_string();
    let checkout = started["worktree"]
        .as_str()
        .unwrap_or_else(|| panic!("start --json names its checkout: {started}"))
        .to_string();

    let killed = request(
        &server,
        &format!(
            r#"{{"id":"k","method":"worktree.kill","params":{{"workspace_id":"{ws}","force":true}}}}"#
        ),
    );
    assert!(killed.get("error").is_none(), "worktree.kill: {killed}");
    assert!(
        !Path::new(&checkout).exists(),
        "precondition: the checkout is gone"
    );

    let reaped = cli(&server, &["delegate", "reap", "w1", "--json"]);
    assert_eq!(reaped.status.code(), Some(0), "reap: {}", stderr(&reaped));
    assert!(entry_file(&server, "w1").is_none(), "the entry is removed");
}

/// a30 (P578 r4-4): forget nothing. A start whose registry write FAILS must not
/// delete an entry it did not write. The old entry is stale (its workspace was
/// closed by hand), so the start proceeds; the write is made to fail by
/// occupying its temp path with a directory.
#[test]
fn a30_a_failed_registry_write_keeps_the_existing_entry() {
    let server = start_server();
    operator_workspace(&server);
    let b = brief(&server, "task.md", "x\n");
    let mut child = start_cwd(&server, "d1", &b, &["--json"]);
    make_ready(&server, "d1");
    let status = exited_within(&mut child, WITHIN).expect("start returns");
    let started = stdout_json(&finish(child));
    assert_eq!(status.code(), Some(0));
    let ws = started["workspace_id"].as_str().unwrap().to_string();
    let closed = request(
        &server,
        &format!(r#"{{"id":"wc","method":"workspace.close","params":{{"workspace_id":"{ws}"}}}}"#),
    );
    assert!(closed.get("error").is_none(), "workspace.close: {closed}");
    // The old agent must be gone before the second start, or a slow runner
    // marks the dying pane ready instead of the new one.
    let gone_by = Instant::now() + WITHIN;
    while agent_get(&server, "d1").is_some() {
        assert!(Instant::now() < gone_by, "the old agent never went away");
        thread::sleep(Duration::from_millis(50));
    }

    let entry = entry_file(&server, "d1").expect("precondition: the old entry exists");
    let before = fs::read(&entry).unwrap();
    fs::create_dir_all(entry.parent().unwrap().join(".d1.json.tmp")).unwrap();

    let mut again = start_cwd(&server, "d1", &b, &["--json"]);
    let ready_by = Instant::now() + WITHIN;
    while agent_get(&server, "d1").is_none() && Instant::now() < ready_by {
        thread::sleep(Duration::from_millis(50));
    }
    if agent_get(&server, "d1").is_some() {
        make_ready(&server, "d1");
    }
    let status = exited_within(&mut again, WITHIN).expect("the second start returns");
    let out = finish(again);
    assert_eq!(status.code(), Some(1), "stderr {}", stderr(&out));
    assert_eq!(
        fs::read(&entry).ok(),
        Some(before),
        "the entry this start did not write is still there, unchanged"
    );
}
