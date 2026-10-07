//! Safety tests for `flk delegate` (#578), round 1 of the fix review.
//!
//! `tests/delegate.rs` is the conductor's frozen acceptance suite and stays that
//! way. These are the tests for the faults that suite did not reach, and they are
//! in a new file so the frozen one is untouched:
//!
//! 1. a rollback must not close a workspace the operator opened while the
//!    delegate was starting (R1);
//! 2. a reap must not destroy a workspace whose id a restarted server has
//!    reassigned (R2);
//! 3. a reap that the server refuses must report it, map the exit code through
//!    `worktree kill`'s table, and keep the entry (R4);
//! 4. a reply from the turn BEFORE the brief must not count as this round's
//!    (R8);
//! 5. `agent wait` and `wait agent-status` refusals must be byte-for-byte what
//!    they were before `flk delegate` existed (R5).
//!
//! The fixture is the same shape as `tests/delegate.rs`: a real `flk server` in a
//! PTY, a fake `opencode` on its PATH that draws whatever screen the test asks
//! for, and an opencode database the test writes replies into.

// TracedCommand (logging redesign PR-3) polices flock's shipped code; this
// harness drives the compiled flock binary through raw Command.
#![allow(clippy::disallowed_methods)]

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

/// Comfortably longer than the delegate's 10 s result grace, so a test that
/// waits it out is waiting out the grace and not merely a few polls.
const RESULT_GRACE_OUTSIDE: Duration = Duration::from_millis(12_000);

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
         while IFS= read -r line; do printf '%s\\n' \"$line\" >> '{base}/typed.log'; done\n",
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
        "idle" => "",
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

/// Where the registry keeps this server's delegates.
///
/// The test needs it for one thing only: rewriting an entry so its
/// `workspace_id`, `pane_id`, and `root_pane` name a workspace that is not the
/// delegate's, which is what a restarted server can leave behind. The key is
/// FNV-1a 64 over the socket path string, exactly as `cli::delegate::server_key`
/// computes it.
fn registry_entry_path(server: &Server, name: &str) -> PathBuf {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in server.socket.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    server
        .base
        .join("state")
        .join(app_dir_name())
        .join("delegates")
        .join(format!("{hash:016x}"))
        .join(format!("{name}.json"))
}

/// Rewrite the entry's `workspace_id`, `pane_id`, and `root_pane` to point at the
/// given workspace, using its actual pane ids. The `terminal_id` is left alone
/// to simulate a server restart where ids are reused but terminal ids are not.
fn rewrite_entry_workspace(server: &Server, name: &str, workspace_id: &str) {
    let path = registry_entry_path(server, name);
    let body = fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {path:?}: {err}"));
    let mut entry: serde_json::Value = serde_json::from_str(&body).expect("the entry is JSON");
    entry["workspace_id"] = serde_json::Value::String(workspace_id.to_string());
    // Get the target workspace's actual pane ids to simulate a restart.
    let listed = request(
        server,
        &format!(
            r#"{{"id":"pl","method":"pane.list","params":{{"workspace_id":"{workspace_id}"}}}}"#
        ),
    );
    if let Some(panes) = listed.pointer("/result/panes").and_then(|p| p.as_array()) {
        if let Some(first_pane) = panes.first() {
            if let Some(pane_id) = first_pane.get("pane_id").and_then(|v| v.as_str()) {
                entry["pane_id"] = serde_json::Value::String(pane_id.to_string());
                entry["root_pane"] = serde_json::Value::String(pane_id.to_string());
            }
        }
    }
    fs::write(&path, serde_json::to_string(&entry).unwrap()).unwrap();
}

fn workspace_pane_count(server: &Server, workspace_id: &str) -> u64 {
    let listed = request(
        server,
        &format!(
            r#"{{"id":"pl","method":"pane.list","params":{{"workspace_id":"{workspace_id}"}}}}"#
        ),
    );
    listed["result"]["panes"]
        .as_array()
        .map(|panes| panes.len() as u64)
        .unwrap_or(0)
}

/// Create a plain workspace the operator owns, and return its id.
fn plain_workspace(server: &Server, dir: &Path) -> String {
    fs::create_dir_all(dir).unwrap();
    let created = request(
        server,
        &format!(
            r#"{{"id":"ws","method":"workspace.create","params":{{"cwd":"{}","focus":false}}}}"#,
            dir.display()
        ),
    );
    assert!(created.get("error").is_none(), "{created}");
    created["result"]["workspace"]["workspace_id"]
        .as_str()
        .expect("workspace_id")
        .to_string()
}

/// R1: a rollback closes only what the start created.
///
/// `worktree.create` opens a repository-root workspace as the new checkout's
/// parent, and the round-0 code closed that parent by diffing the workspace list
/// across the placement call — so any workspace the OPERATOR opened while the
/// delegate was starting would be closed by the rollback with it.
///
/// The harness draws a permission prompt and nothing else, so the start places
/// its worktree, finds the agent ready, and then waits out the whole
/// `--ready-timeout` at the prompt. That is the window the test needs: it opens a
/// workspace of its own while the start is provably still running, and then lets
/// the start fail.
#[test]
fn s1_rollback_leaves_a_foreign_workspace_alone() {
    let server = start_server();
    let operator = operator_workspace(&server);
    let repo = committed_repo(&server);
    let repo_s = repo.to_string_lossy().into_owned();
    let before_the_start = plain_workspace(&server, &server.base.join("op-before"));
    // A permission prompt: ready (non-`unknown`) but not at its prompt, so the
    // start waits instead of typing.
    fs::write(server.base.join("screen"), screen_for("blocked")).unwrap();
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
            "feat/p578-r1",
            "--ready-timeout",
            "30000",
        ],
    );
    // Wait for the delegate's own linked worktree to appear: proof the start is
    // past placement and into its readiness gate.
    let deadline = Instant::now() + WITHIN;
    loop {
        let linked = workspaces(&server)
            .iter()
            .any(|ws| ws["worktree"]["is_linked_worktree"] == true);
        if linked {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the start never placed a worktree"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let during_the_start = plain_workspace(&server, &server.base.join("op-during"));
    assert!(
        child.try_wait().unwrap().is_none(),
        "the start is still running, so the window is real"
    );

    let status = exited_within(&mut child, WITHIN + Duration::from_secs(35))
        .expect("the readiness gate gives up");
    let out = finish(child);
    assert_eq!(status.code(), Some(1), "stderr {}", stderr(&out));
    assert!(
        stderr(&out).contains("is blocked"),
        "the status is named: {}",
        stderr(&out)
    );

    let open: Vec<String> = workspaces(&server)
        .iter()
        .filter_map(|ws| ws["workspace_id"].as_str().map(str::to_string))
        .collect();
    for foreign in [
        operator.as_str(),
        before_the_start.as_str(),
        during_the_start.as_str(),
    ] {
        assert!(
            open.iter().any(|id| id == foreign),
            "the rollback closed the operator's workspace {foreign}: {open:?}"
        );
    }
    assert_eq!(
        open.len(),
        3,
        "and the delegate's own workspaces are gone again: {open:?}"
    );
}

/// R2: a reap must not destroy a workspace whose id now names something else.
///
/// Workspace ids are reused only across a server restart, so the fault is
/// injected: the entry is rewritten to name the operator's workspace, which is
/// exactly what a reassigned id looks like from here. The operator's workspace
/// and its pane must survive; in worktree mode the checkout is still removed,
/// because that is the recorded path and nothing else.
#[test]
fn s2_reap_leaves_a_reused_id_alone() {
    let server = start_server();
    let operator = operator_workspace(&server);
    let operator_panes = workspace_pane_count(&server, &operator);
    let b = brief(&server, "task.md", "x\n");

    let mut child = start_cwd(&server, "d1", &b, &["--json"]);
    make_ready(&server, "d1");
    assert_eq!(
        exited_within(&mut child, WITHIN).and_then(|s| s.code()),
        Some(0)
    );
    let started = stdout_json(&finish(child));
    let delegate_ws = started["workspace_id"].as_str().unwrap().to_string();
    assert_ne!(
        delegate_ws.as_str(),
        operator.as_str(),
        "the delegate got its own workspace"
    );

    // The delegate's own workspace goes away by hand, and the entry is pointed at
    // the operator's: a stale id, exactly as a restart would leave it.
    let closed = request(
        &server,
        &format!(
            r#"{{"id":"wc","method":"workspace.close","params":{{"workspace_id":"{delegate_ws}"}}}}"#
        ),
    );
    assert!(
        closed.get("error").is_none(),
        "workspace.close failed: {closed}"
    );
    rewrite_entry_workspace(&server, "d1", &operator);

    let reaped = cli(&server, &["delegate", "reap", "d1", "--json"]);
    assert_eq!(reaped.status.code(), Some(0), "reap: {}", stderr(&reaped));
    assert!(
        workspaces(&server)
            .iter()
            .any(|ws| ws["workspace_id"] == operator.as_str()),
        "the operator's workspace survived"
    );
    assert_eq!(
        workspace_pane_count(&server, &operator),
        operator_panes,
        "and so did its pane"
    );
    assert!(
        !registry_entry_path(&server, "d1").exists(),
        "the entry is still removed"
    );
}

/// R2, worktree mode: the mismatched workspace survives, and the checkout is
/// removed by path.
#[test]
fn s2_reap_removes_the_checkout_but_not_a_reused_workspace() {
    let server = start_server();
    let operator = operator_workspace(&server);
    let operator_panes = workspace_pane_count(&server, &operator);
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
            "feat/p578-r2",
            "--json",
        ],
    );
    make_ready(&server, "w1");
    assert_eq!(
        exited_within(&mut child, WITHIN).and_then(|s| s.code()),
        Some(0)
    );
    let json = stdout_json(&finish(child));
    let delegate_ws = json["workspace_id"].as_str().unwrap().to_string();
    let worktree = PathBuf::from(json["worktree"].as_str().expect("worktree path"));
    assert!(worktree.is_dir(), "{json}");

    // Point the entry at the operator's workspace; the delegate's own workspace
    // stays open, holding the checkout.
    rewrite_entry_workspace(&server, "w1", &operator);
    let reaped = cli(&server, &["delegate", "reap", "w1", "--json"]);
    assert_eq!(reaped.status.code(), Some(0), "reap: {}", stderr(&reaped));
    assert!(
        workspaces(&server)
            .iter()
            .any(|ws| ws["workspace_id"] == operator.as_str()),
        "the mismatched workspace survived"
    );
    assert_eq!(
        workspace_pane_count(&server, &operator),
        operator_panes,
        "and so did its pane"
    );
    // The delegate's own recorded workspace must be gone.
    assert!(
        workspaces(&server)
            .iter()
            .all(|ws| ws["workspace_id"] != delegate_ws.as_str()),
        "the delegate's own workspace was removed"
    );
    assert!(
        !worktree.exists(),
        "the recorded checkout is still removed: a path is not a workspace id"
    );
    assert!(
        !registry_entry_path(&server, "w1").exists(),
        "the entry is removed"
    );
}

/// R4: a reap the server refuses reports the refusal, maps it through
/// `worktree kill`'s own table, and keeps the entry so it can be retried.
#[test]
fn s3_reap_reports_a_refusal_and_keeps_the_entry() {
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
            "feat/p578-r4",
            "--json",
        ],
    );
    make_ready(&server, "w1");
    assert_eq!(
        exited_within(&mut child, WITHIN).and_then(|s| s.code()),
        Some(0)
    );
    let json = stdout_json(&finish(child));
    let worktree = PathBuf::from(json["worktree"].as_str().expect("worktree path"));

    // An untracked file makes the checkout dirty, which `worktree.kill` refuses
    // without --force — the retryable class, exit 4.
    fs::write(worktree.join("scratch.txt"), "untracked\n").unwrap();
    let refused = cli(&server, &["delegate", "reap", "w1"]);
    assert_eq!(
        refused.status.code(),
        Some(4),
        "stderr {}",
        stderr(&refused)
    );
    let reason = stderr(&refused);
    assert!(
        reason.contains("dirty_worktree_requires_force"),
        "the server's own refusal is reported: {reason:?}"
    );
    assert!(
        stdout(&refused).is_empty(),
        "a refused reap prints no outcome object on stdout: {:?}",
        stdout(&refused)
    );
    assert!(
        registry_entry_path(&server, "w1").exists(),
        "the entry is KEPT, so the reap can be retried"
    );
    assert!(worktree.is_dir(), "and the checkout is still there");

    let forced = cli(&server, &["delegate", "reap", "w1", "--force", "--json"]);
    assert_eq!(forced.status.code(), Some(0), "stderr {}", stderr(&forced));
    assert!(!worktree.exists(), "--force removes it");
    assert!(!registry_entry_path(&server, "w1").exists());
}

/// R8: the cursor is captured from the LAST record of the idle gate, so the quiet
/// that PRECEDED the brief cannot be settled into a result.
///
/// The sequence is the one that breaks a cursor captured too early. The agent is
/// working before `send` runs, so the send sits in the idle gate; it then goes
/// idle with round one's reply still the newest and never works again. A cursor
/// captured before the gate would already count this idle as "a turn started
/// after it", settle on it, and then poll a grace for a reply that belongs to
/// round one — reporting `no_result` when the grace ran out, while the real turn
/// was still to come. The cursor captured last does not settle at all, because no
/// working entry follows it.
///
/// The second half is what makes the test bite rather than merely pass: the wait
/// has to OUTLIVE the whole result grace, because a grace that expired is exactly
/// where the early cursor showed its answer.
#[test]
fn s4_the_earlier_turn_does_not_count() {
    let server = start_server();
    operator_workspace(&server);
    let b = brief(&server, "task.md", "x\n");
    let mut child = start_cwd(&server, "d1", &b, &["--json"]);
    let pane = make_ready(&server, "d1");
    wait_typed(&server, 1);
    play_turn(&server, &pane, "t1", "DONE: round one");
    assert_eq!(
        exited_within(&mut child, WITHIN).and_then(|s| s.code()),
        Some(0)
    );
    finish(child);

    // The agent is working BEFORE the send runs, so the send sits in the idle
    // gate rather than submitting straight away. (Reported first, then spawned:
    // the reverse order races, and a send that finds the agent already idle has
    // no gate to sit in.)
    report(&server, &pane, "working");
    let r1 = brief(&server, "r1.md", "y\n");
    let mut sender = cli_spawn(
        &server,
        &[
            "delegate",
            "send",
            "d1",
            "--brief",
            &r1,
            "--await",
            "--settle",
            SETTLE,
            "--timeout",
            "60000",
            "--json",
        ],
    );
    thread::sleep(Duration::from_millis(4 * SETTLE_MS));
    assert_eq!(
        typed(&server),
        vec![expected_line(&b)],
        "the send waits for the prompt, and the typed text is still only the brief sentence"
    );

    // Idle, with round one's reply still the newest, and no working entry after
    // it. Nothing may be reported — not before the grace, and not after it.
    report(&server, &pane, "idle");
    thread::sleep(Duration::from_millis(4 * SETTLE_MS));
    assert!(
        sender.try_wait().unwrap().is_none(),
        "round one's reply must not be reported as round two's"
    );
    thread::sleep(RESULT_GRACE_OUTSIDE);
    assert!(
        sender.try_wait().unwrap().is_none(),
        "and the await must outlive the whole result grace, rather than settling on the quiet \
         that preceded the brief and reporting no_result"
    );

    // Now a real turn, and the await reports THAT.
    report(&server, &pane, "working");
    thread::sleep(Duration::from_millis(100));
    write_reply(&server, "t2", "DONE: round two");
    report(&server, &pane, "idle");
    let status = exited_within(&mut sender, WITHIN).expect("the await returns");
    let out = finish(sender);
    assert_eq!(status.code(), Some(0), "stderr {}", stderr(&out));
    let json = stdout_json(&out);
    assert_eq!(json["status_text"], "round two", "{json}");
    assert_eq!(json["round"], 2, "{json}");
}

/// R5: the two settled wait verbs' refusals are byte-for-byte what they were.
///
/// At the base, a refusal to the INITIAL resolve printed the server's whole
/// response with no verb prefix in front of it. The round-0 split folded that
/// into the same arm as every other failure and put `{verb}: ` in front of it.
#[test]
fn s5_settled_wait_refusals_are_byte_for_byte() {
    let server = start_server();
    operator_workspace(&server);
    for verb in [
        vec![
            "agent",
            "wait",
            "no-such-agent",
            "--status",
            "settled",
            "--timeout",
            "3000",
        ],
        vec![
            "wait",
            "agent-status",
            "no-such-pane",
            "--status",
            "settled",
            "--timeout",
            "3000",
        ],
    ] {
        let out = cli(&server, &verb);
        assert_eq!(out.status.code(), Some(1), "{verb:?}: {}", stderr(&out));
        let reason = stderr(&out);
        let trimmed = reason.trim_end();
        assert!(
            trimmed.starts_with('{'),
            "{verb:?}: the server's response is printed verbatim, JSON and all: {trimmed:?}"
        );
        assert!(
            !trimmed.contains("agent wait:") && !trimmed.contains("wait agent-status:"),
            "{verb:?}: no verb prefix may be added: {trimmed:?}"
        );
        let parsed: serde_json::Value =
            serde_json::from_str(trimmed).unwrap_or_else(|err| panic!("{trimmed:?}: {err}"));
        assert!(
            parsed["error"]["code"].is_string(),
            "and it parses as the server's own error: {parsed}"
        );
        assert!(
            stdout(&out).is_empty(),
            "{verb:?}: a refusal prints nothing on stdout"
        );
    }
}

/// W2 (P578 r4-3): every request `delegate send` makes is bounded. A `send`
/// against a frozen server ends well within `cap + 5 s` (not forever),
/// exits 1 naming the delegate, and leaves the registry entry's `round`
/// unchanged — a submit failing must look to a retry like the round that
/// failed had never been written.
///
/// This is the call-site mirror of the brief's W2: the frozen-server read
/// (`require_delegate` → `agent.get`) and the brief submit itself (now
/// bounded through `delegate.rs`, not through the untimed `pane` helpers)
/// both honour the cap. Either path is proof that no request is unbounded;
/// what the test rules out is the F2 behaviour of hanging forever.
#[test]
fn s6_send_against_a_frozen_server_ends_within_cap() {
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
    // Capture round BEFORE the frozen send so a retry would see the same
    // round number — a failed submit must not mutate the entry.
    let round_before: u64 = {
        let body = fs::read_to_string(registry_entry_path(&server, "d1")).unwrap();
        let entry: serde_json::Value = serde_json::from_str(&body).unwrap();
        entry["round"].as_u64().unwrap()
    };

    let pid = server.child.process_id().expect("server pid") as libc::pid_t;
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    let b2 = brief(&server, "task2.md", "y\n");
    let started = Instant::now();
    let mut send = cli_spawn(&server, &["delegate", "send", "d1", "--brief", &b2]);
    // 10 s PaneSendInput cap + 5 s slack: that is the brief's budget.
    let status = exited_within(&mut send, Duration::from_secs(15));
    unsafe { libc::kill(pid, libc::SIGCONT) };
    let status = status.expect("the send must end on its own, not stall against a frozen server");
    let elapsed = started.elapsed();
    let out = finish(send);
    assert_eq!(status.code(), Some(1), "stderr: {}", stderr(&out));
    assert!(
        elapsed < Duration::from_secs(15),
        "ended within cap + 5 s: {elapsed:?}"
    );
    assert!(
        stderr(&out).contains("delegate d1"),
        "stderr names the delegate: {}",
        stderr(&out)
    );
    assert!(
        !stderr(&out).contains("not a delegate"),
        "an unreachable server is never 'not a delegate': {}",
        stderr(&out)
    );

    let round_after: u64 = {
        let body = fs::read_to_string(registry_entry_path(&server, "d1")).unwrap();
        let entry: serde_json::Value = serde_json::from_str(&body).unwrap();
        entry["round"].as_u64().unwrap()
    };
    assert_eq!(
        round_before, round_after,
        "a failed submit leaves the entry unchanged"
    );
}

/// R18 (gerchowl/flock#556): `flk --help` must mention `delegate` so a supervising
/// agent can discover the verb.
#[test]
fn r18_flk_help_lists_delegate() {
    let server = start_server();
    let out = cli(&server, &["--help"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let stdout = stdout(&out);
    assert!(
        stdout.contains("flk delegate <subcommand> ..."),
        "the Usage block must list the delegate command: {stdout}"
    );
    assert!(
        stdout.contains("flk delegate <subcommand>        "),
        "the command list must list the delegate command: {stdout}"
    );
    // The short description should appear on the same line or adjacent.
    assert!(
        stdout.contains("Hand a task to an agent"),
        "flk --help must describe what delegate does: {stdout}"
    );
}
