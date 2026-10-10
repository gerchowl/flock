//! Acceptance tests for package P578 r2 (#578): `flk delegate`, the plain
//! wrapper over worktree/workspace + agent start + brief submit + settled
//! wait + agent result + reap.
//!
//! Written by the conductor before the build and frozen: the builder must not
//! edit this file. Each test names the edge-table row (E*) it covers.
//!
//! The harness is a fake `opencode` on the server's PATH. It records its argv
//! and every line typed into it, so the test can see exactly what the
//! delegate submitted. opencode's status is NATIVE: flock reads it off the
//! screen (`src/detect/agents/opencode.rs`; a `flock:opencode` state report
//! is reserved and only carries the session). So the fake draws the screen
//! the test asks for through `<base>/screen`: a progress run for working,
//! a prompt for idle, the permission banner for blocked. The test also plays
//! the opencode plugin's session report (`pane.report_agent_session`) and
//! writes the session's replies into an opencode database under an isolated
//! `XDG_DATA_HOME`, which is what `agent.result` reads.
//! `a0_*` calibrates that harness and passes on the base as well; every other
//! test fails on the base.

// TracedCommand (logging redesign PR-3) polices flock's shipped code; this
// harness drives the compiled flock binary through raw Command.
#![allow(clippy::disallowed_methods)]

mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use support::environment::Command;

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
/// `<base>/die` exists, dies on its first input line when `<base>/die-on-input`
/// exists, redraws its screen whenever `<base>/screen` changes,
/// and appends every line typed into its pane to `<base>/typed.log`.
fn write_fake_opencode(base: &Path) {
    let bin = bin_dir(base);
    fs::create_dir_all(&bin).unwrap();
    let script = format!(
        r#"#!/bin/sh
if [ -e '{base}/starting' ]; then
  : > '{base}/startup-entered'
  while [ -e '{base}/starting' ]; do sleep 0.05; done
fi
printf '%s\n' "$*" >> '{base}/argv.log'
printf '%s\n' "$PWD" >> '{base}/cwd.log'
if [ -e '{base}/die' ]; then exit 1; fi
( last=; while :; do
  now=$(cat '{base}/screen' 2>/dev/null)
  if [ "$now" != "$last" ]; then
    printf '\033[2J\033[H%s\n' "$now"
    last=$now
    : > '{base}/first-draw'
  fi
  sleep 0.05
done ) &
if [ -e '{base}/ignore-input-ms' ]; then
  ( while [ ! -e '{base}/first-draw' ]; do sleep 0.01; done
    sleep "$(awk '{{print $1 / 1000}}' '{base}/ignore-input-ms')"
    : > '{base}/accept-input' ) &
fi
pending=
first=1
while IFS= read -r line; do
  printf '%s\n' "$line" >> '{base}/input.log'
  if [ -e '{base}/die-on-input' ]; then exit 1; fi
  if [ -e '{base}/ignore-input-ms' ] && [ ! -e '{base}/accept-input' ]; then continue; fi
  if [ -e '{base}/hold-first-enter' ] && [ -z "$pending" ]; then
    pending=$line
    ( printf '┃\n'
      printf '%s\n' "$pending" | fold -s -w 40 | sed 's/^/┃  /'
      printf '┃\n┃  Build test-model\n╹\ntab agents ctrl+p commands\n' ) > '{base}/screen'
    continue
  fi
  if [ -n "$pending" ]; then line=$pending; pending=; rm '{base}/hold-first-enter'; fi
  if [ -e '{base}/late-submit-ms' ]; then
    sleep "$(awk '{{print $1 / 1000}}' '{base}/late-submit-ms')"
  fi
  printf '%s\n' "$line" >> '{base}/typed.log'
  if [ ! -e '{base}/manual-submit' ]; then
    first=
    printf '\033[2J\033[H■■■■⬝⬝  esc interrupt  opencode\n'
    sleep 0.4
    printf '\033[2J\033[H%s\n' "$(cat '{base}/screen')"
  fi
done
"#,
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
    start_server_with_shell_startup(false)
}

fn start_server_with_shell_startup(slow_startup: bool) -> Server {
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
    let terminal_config = if slow_startup {
        let shell = base.join("slow-shell");
        fs::write(&shell, "#!/bin/sh\n. \"$0.rc\"\nexec /bin/sh\n").unwrap();
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(base.join("slow-shell.rc"), "sleep 30\n").unwrap();
        format!(
            "[terminal]\ndefault_shell = {:?}\nshell_mode = \"non_login\"\n",
            shell
        )
    } else {
        String::new()
    };
    fs::write(
        config_home.join(app_dir_name()).join("config.toml"),
        format!(
            "onboarding = false\n\n{terminal_config}\n[worktrees]\ndirectory = \"{}\"\n",
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
    for (key, value) in support::environment::isolated_env(&config_home, &runtime_dir) {
        cmd.env(key, value);
    }
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

/// Calibration: a fake `opencode` started through `agent start` runs with the
/// test's PATH, is detected as opencode, receives typed lines, shows the
/// status its screen draws (and holds it), and its session's reply is read
/// back by `agent result`. Passes on the base.
#[test]
fn a0_fake_harness_runs_and_reported_status_holds() {
    let server = start_server();
    operator_workspace(&server);
    let work = work_dir(&server);
    let started = cli(
        &server,
        &[
            "agent",
            "start",
            "cal",
            "--cwd",
            &work,
            "--no-focus",
            "--",
            "opencode",
            "--model",
            "m1",
        ],
    );
    assert!(
        started.status.success(),
        "agent start: {}",
        stderr(&started)
    );
    let pane = delegate_pane(&server, "cal");
    report(&server, &pane, "idle");
    let deadline = Instant::now() + WITHIN;
    while read_lines(&server.base.join("argv.log")).is_empty() {
        assert!(Instant::now() < deadline, "the fake harness never ran");
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        read_lines(&server.base.join("argv.log")),
        vec!["--model m1"]
    );
    let ran = cli(&server, &["pane", "run", &pane, "hello there"]);
    assert!(ran.status.success(), "pane run: {}", stderr(&ran));
    assert_eq!(wait_typed(&server, 1), vec!["hello there"]);
    report(&server, &pane, "working");
    thread::sleep(Duration::from_millis(4 * SETTLE_MS));
    let agent = agent_get(&server, "cal").unwrap();
    assert_eq!(agent["agent_status"], "working", "{agent}");
    assert_eq!(agent["agent"], "opencode", "{agent}");
    report(&server, &pane, "blocked");
    thread::sleep(Duration::from_millis(4 * SETTLE_MS));
    assert_eq!(
        agent_get(&server, "cal").unwrap()["agent_status"],
        "blocked"
    );
    report(&server, &pane, "idle");
    report_session(&server, &pane);
    write_reply(&server, "t0", "DONE: calibrated");
    let result = cli(&server, &["agent", "result", "cal"]);
    assert!(result.status.success(), "agent result: {}", stderr(&result));
    let info = &stdout_json(&result)["result"]["result"];
    assert_eq!(info["status_text"], "calibrated", "{info}");
    assert_eq!(info["finished"], true, "{info}");
}

/// E1: every usage error exits 2 before anything is created.
#[test]
fn a1_usage_errors_exit_2_and_create_nothing() {
    let server = start_server();
    operator_workspace(&server);
    let before = workspaces(&server).len();
    let work = work_dir(&server);
    let good = brief(&server, "good.md", "do it\n");
    let bang_dir = server.base.join("briefs").join("bang!");
    fs::create_dir_all(&bang_dir).unwrap();
    let bang = bang_dir.join("b.md");
    fs::write(&bang, "x\n").unwrap();
    let dollar_dir = server.base.join("briefs").join("d$x");
    fs::create_dir_all(&dollar_dir).unwrap();
    let dollar = dollar_dir.join("b.md");
    fs::write(&dollar, "x\n").unwrap();
    let missing = server.base.join("briefs").join("missing.md");
    let bang_s = bang.to_string_lossy().into_owned();
    let dollar_s = dollar.to_string_lossy().into_owned();
    let missing_s = missing.to_string_lossy().into_owned();
    let briefs_dir = server.base.join("briefs").to_string_lossy().into_owned();
    let spaced_dir = server.base.join("briefs").join("two words");
    fs::create_dir_all(&spaced_dir).unwrap();
    let spaced = spaced_dir.join("b.md");
    fs::write(&spaced, "x\n").unwrap();
    let spaced_s = spaced.to_string_lossy().into_owned();

    let cases: Vec<Vec<&str>> = vec![
        vec!["delegate", "start", "u1", "--cwd", &work],
        vec!["delegate", "start", "u1", "--brief", &good],
        vec![
            "delegate",
            "start",
            "u1",
            "--brief",
            &good,
            "--cwd",
            &work,
            "--worktree",
        ],
        vec![
            "delegate",
            "start",
            "u1",
            "--brief",
            &good,
            "--cwd",
            &work,
            "--harness",
            "aider",
        ],
        vec![
            "delegate", "start", "u1", "--brief", &missing_s, "--cwd", &work,
        ],
        vec![
            "delegate",
            "start",
            "u1",
            "--brief",
            &briefs_dir,
            "--cwd",
            &work,
        ],
        vec![
            "delegate", "start", "u1", "--brief", &bang_s, "--cwd", &work,
        ],
        vec![
            "delegate", "start", "u1", "--brief", &dollar_s, "--cwd", &work,
        ],
        vec!["delegate", "start", "--brief", &good, "--cwd", &work],
        vec![
            "delegate", "start", "u1", "--brief", &good, "--cwd", &work, "--bogus",
        ],
        vec!["delegate", "send", "u1"],
        vec!["delegate"],
        vec![
            "delegate", "start", "u1", "--brief", &spaced_s, "--cwd", &work,
        ],
        vec![
            "delegate", "start", "u1", "--brief", &good, "--cwd", &work, "--branch", "b",
        ],
        vec![
            "delegate", "start", "u1", "--brief", &good, "--cwd", &work, "--after", "x",
        ],
    ];
    for args in &cases {
        let out = cli(&server, args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?} must be a usage error: stdout {:?} stderr {:?}",
            stdout(&out),
            stderr(&out)
        );
    }
    // A harness this build does not drive. `claude` was the refusal here while
    // it was the only one (#612); the row moved to a name the table still lacks.
    let harness = cli(
        &server,
        &[
            "delegate",
            "start",
            "u1",
            "--brief",
            &good,
            "--cwd",
            &work,
            "--harness",
            "aider",
        ],
    );
    assert!(
        stderr(&harness).contains("not supported"),
        "harness refusal names the reason: {}",
        stderr(&harness)
    );
    assert!(
        stdout(&harness).is_empty(),
        "a usage error prints nothing on stdout, even with --json"
    );
    for args in [
        vec!["delegate", "--help"],
        vec!["delegate", "start", "--help"],
    ] {
        let out = cli(&server, &args);
        assert_eq!(out.status.code(), Some(0), "{args:?}: {}", stderr(&out));
    }
    assert_eq!(
        workspaces(&server).len(),
        before,
        "no workspace was created"
    );
    assert!(agent_get(&server, "u1").is_none(), "no agent was started");
    assert!(
        read_lines(&server.base.join("argv.log")).is_empty(),
        "the harness was never launched"
    );
}

/// E2: cwd mode with --await, ending on DONE.
#[test]
fn a2_cwd_await_done_in_its_own_workspace() {
    let server = start_server();
    let operator = operator_workspace(&server);
    let before = workspaces(&server).len();
    let b = brief(&server, "task.md", "build it\n");
    let mut child = start_cwd(
        &server,
        "d1",
        &b,
        &[
            "--model",
            "opencode/space-bunny-free",
            "--await",
            "--timeout",
            "30000",
            "--json",
        ],
    );
    let pane = make_ready(&server, "d1");
    let lines = wait_typed(&server, 1);
    assert_eq!(lines, vec![expected_line(&b)], "exactly the brief sentence");
    assert!(
        exited_within(&mut child, Duration::from_millis(3 * SETTLE_MS)).is_none(),
        "the await must not return before the turn ran"
    );
    play_turn(&server, &pane, "t1", "Built it.\nDONE: built and pushed");
    let status = exited_within(&mut child, WITHIN).expect("the await returns");
    let out = finish(child);
    assert_eq!(status.code(), Some(0), "stderr {}", stderr(&out));
    let json = stdout_json(&out);
    assert_eq!(json["name"], "d1", "{json}");
    assert_eq!(json["outcome"], "done", "{json}");
    assert_eq!(json["status"], "done", "{json}");
    assert_eq!(json["status_text"], "built and pushed", "{json}");
    assert!(
        json["text"].as_str().unwrap_or("").contains("Built it."),
        "{json}"
    );
    assert_eq!(json["session_id"], SES, "{json}");
    assert_eq!(json["round"], 1, "{json}");
    assert!(json["worktree"].is_null(), "{json}");
    assert!(json["turn_cursor"].is_string(), "{json}");

    assert_eq!(
        read_lines(&server.base.join("argv.log")),
        vec!["--model opencode/space-bunny-free"],
        "argv carries the model and no prompt"
    );
    let after = workspaces(&server);
    assert_eq!(
        after.len(),
        before + 1,
        "the delegate got its own workspace"
    );
    assert_ne!(json["workspace_id"], operator.as_str(), "{json}");
    let agent = agent_get(&server, "d1").unwrap();
    assert_eq!(agent["workspace_id"], json["workspace_id"], "{agent}");
    assert_eq!(agent["pane_id"], pane.as_str(), "{agent}");
    assert_eq!(
        focused_workspace(&server).as_deref(),
        Some(operator.as_str()),
        "the operator's focus is unchanged"
    );
    let state_files: Vec<_> = walk(&server.base.join("state"));
    assert!(
        state_files.iter().any(|p| p.ends_with("d1.json")),
        "a registry entry was written: {state_files:?}"
    );
    let delegate_ws = after
        .iter()
        .find(|ws| ws["workspace_id"] == json["workspace_id"])
        .expect("the delegate's workspace is listed");
    assert_eq!(
        delegate_ws["pane_count"], 1,
        "the workspace holds the agent alone, no leftover shell: {delegate_ws}"
    );
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

/// E3: a BLOCKED sentinel exits 3; a reply with no sentinel exits 5; without
/// --json stdout is the reply text and stderr one summary line.
#[test]
fn a3_blocked_sentinel_and_no_sentinel_and_plain_output() {
    let server = start_server();
    operator_workspace(&server);
    let b = brief(&server, "task.md", "x\n");
    let mut child = start_cwd(&server, "d1", &b, &["--await", "--timeout", "30000"]);
    let pane = make_ready(&server, "d1");
    wait_typed(&server, 1);
    play_turn(
        &server,
        &pane,
        "t1",
        "Cannot go on.\nBLOCKED: need the API key",
    );
    let status = exited_within(&mut child, WITHIN).expect("the await returns");
    let out = finish(child);
    assert_eq!(status.code(), Some(3), "stderr {}", stderr(&out));
    assert_eq!(
        stdout(&out).trim_end(),
        "Cannot go on.\nBLOCKED: need the API key",
        "plain stdout is the reply text"
    );
    let summary = stderr(&out);
    assert!(
        summary.contains("delegate d1: blocked") && summary.contains("need the API key"),
        "stderr summary: {summary:?}"
    );

    let b2 = brief(&server, "r1.md", "y\n");
    let mut child = cli_spawn(
        &server,
        &[
            "delegate",
            "send",
            "d1",
            "--brief",
            &b2,
            "--await",
            "--settle",
            SETTLE,
            "--timeout",
            "30000",
            "--json",
        ],
    );
    wait_typed(&server, 2);
    play_turn(&server, &pane, "t2", "I looked around and stopped.");
    let status = exited_within(&mut child, WITHIN).expect("the await returns");
    let out = finish(child);
    assert_eq!(status.code(), Some(5), "stderr {}", stderr(&out));
    let json = stdout_json(&out);
    assert_eq!(json["outcome"], "no_sentinel", "{json}");
    assert!(json["status"].is_null(), "{json}");
    assert_eq!(json["round"], 2, "{json}");
}

/// E4: an agent that sits in `blocked` (a permission prompt) is reported as
/// `agent_blocked`, exit 6.
#[test]
fn a4_agent_blocked_exits_6() {
    let server = start_server();
    operator_workspace(&server);
    let b = brief(&server, "task.md", "x\n");
    let mut child = start_cwd(
        &server,
        "d1",
        &b,
        &["--await", "--timeout", "30000", "--json"],
    );
    let pane = make_ready(&server, "d1");
    wait_typed(&server, 1);
    report(&server, &pane, "working");
    thread::sleep(Duration::from_millis(100));
    report(&server, &pane, "blocked");
    let status = exited_within(&mut child, WITHIN).expect("the await returns");
    let out = finish(child);
    assert_eq!(status.code(), Some(6), "stderr {}", stderr(&out));
    let json = stdout_json(&out);
    assert_eq!(json["outcome"], "agent_blocked", "{json}");
    assert_eq!(json["pane_id"], pane.as_str(), "{json}");
}

/// E5: a timeout exits 124 with the outcome object; `delegate wait` with no
/// cursor resumes from the registry and completes.
#[test]
fn a5_timeout_then_wait_resumes() {
    let server = start_server();
    operator_workspace(&server);
    let b = brief(&server, "task.md", "x\n");
    let mut child = start_cwd(
        &server,
        "d1",
        &b,
        &["--await", "--timeout", "4000", "--json"],
    );
    let pane = make_ready(&server, "d1");
    wait_typed(&server, 1);
    report(&server, &pane, "working");
    let status = exited_within(&mut child, WITHIN).expect("the timeout fires");
    let out = finish(child);
    assert_eq!(status.code(), Some(124), "stderr {}", stderr(&out));
    let json = stdout_json(&out);
    assert_eq!(json["outcome"], "timeout", "{json}");
    assert!(json["turn_cursor"].is_string(), "{json}");

    let mut waiter = cli_spawn(
        &server,
        &[
            "delegate",
            "wait",
            "d1",
            "--settle",
            SETTLE,
            "--timeout",
            "30000",
            "--json",
        ],
    );
    thread::sleep(Duration::from_millis(2 * SETTLE_MS));
    assert!(
        waiter.try_wait().unwrap().is_none(),
        "the agent is still working; the wait must block"
    );
    write_reply(&server, "t1", "DONE: finally");
    report(&server, &pane, "idle");
    let status = exited_within(&mut waiter, WITHIN).expect("the wait returns");
    let out = finish(waiter);
    assert_eq!(status.code(), Some(0), "stderr {}", stderr(&out));
    assert_eq!(stdout_json(&out)["status_text"], "finally");
}

/// E6: without --await, start returns right after the submit.
#[test]
fn a6_start_without_await_returns_after_submit() {
    let server = start_server();
    operator_workspace(&server);
    let b = brief(&server, "task.md", "x\n");
    let mut child = start_cwd(&server, "d1", &b, &["--json"]);
    let pane = make_ready(&server, "d1");
    let status = exited_within(&mut child, WITHIN).expect("start returns after submitting");
    let out = finish(child);
    assert_eq!(status.code(), Some(0), "stderr {}", stderr(&out));
    assert_eq!(
        wait_typed(&server, 1),
        vec![expected_line(&b)],
        "submitted before returning"
    );
    let json = stdout_json(&out);
    assert_eq!(json["name"], "d1", "{json}");
    assert_eq!(json["pane_id"], pane.as_str(), "{json}");
    assert_eq!(json["round"], 1, "{json}");
    let cursor = json["turn_cursor"]
        .as_str()
        .expect("turn_cursor")
        .to_string();

    report(&server, &pane, "idle");
    let mut waiter = cli_spawn(
        &server,
        &[
            "delegate",
            "wait",
            "d1",
            "--after",
            &cursor,
            "--settle",
            SETTLE,
            "--timeout",
            "30000",
            "--json",
        ],
    );
    thread::sleep(Duration::from_millis(3 * SETTLE_MS));
    assert!(
        waiter.try_wait().unwrap().is_none(),
        "idle since before the submit is not a finished turn"
    );
    play_turn(&server, &pane, "t1", "VERDICT: approve");
    let status = exited_within(&mut waiter, WITHIN).expect("the wait returns");
    let out = finish(waiter);
    assert_eq!(status.code(), Some(0), "stderr {}", stderr(&out));
    let json = stdout_json(&out);
    assert_eq!(json["outcome"], "verdict", "{json}");
    assert_eq!(json["status_text"], "approve", "{json}");
}

/// E7: a fix round reports its own turn: not the settled state before it, and
/// not the previous round's reply, even when the agent goes idle before the
/// new reply is written.
#[test]
fn a7_fix_round_ignores_the_previous_turn_and_reply() {
    let server = start_server();
    operator_workspace(&server);
    let b = brief(&server, "task.md", "x\n");
    let mut child = start_cwd(
        &server,
        "d1",
        &b,
        &["--await", "--timeout", "30000", "--json"],
    );
    let pane = make_ready(&server, "d1");
    wait_typed(&server, 1);
    play_turn(&server, &pane, "t1", "DONE: round one");
    let status = exited_within(&mut child, WITHIN).expect("round 1 returns");
    assert_eq!(status.code(), Some(0));
    finish(child);

    let r1 = brief(&server, "r1.md", "fix it\n");
    let mut child = cli_spawn(
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
            "30000",
            "--json",
        ],
    );
    let lines = wait_typed(&server, 2);
    assert_eq!(lines[1], expected_line(&r1));
    thread::sleep(Duration::from_millis(3 * SETTLE_MS));
    assert!(
        child.try_wait().unwrap().is_none(),
        "the idle state before the round is not this round's result"
    );
    report(&server, &pane, "working");
    thread::sleep(Duration::from_millis(100));
    report(&server, &pane, "idle");
    thread::sleep(Duration::from_millis(4 * SETTLE_MS));
    assert!(
        child.try_wait().unwrap().is_none(),
        "settled, but the newest reply is round 1's: it must not be reported"
    );
    write_reply(&server, "t2", "DONE: round two");
    let status = exited_within(&mut child, WITHIN).expect("round 2 returns");
    let out = finish(child);
    assert_eq!(status.code(), Some(0), "stderr {}", stderr(&out));
    let json = stdout_json(&out);
    assert_eq!(json["status_text"], "round two", "{json}");
    assert_eq!(json["round"], 2, "{json}");
}

/// E8: worktree mode creates the branch and checkout, runs the agent there,
/// and reap removes all of it.
#[test]
fn a8_worktree_mode_and_reap() {
    let server = start_server();
    let operator = operator_workspace(&server);
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
            "feat/p578-w1",
            "--base",
            "main",
            "--json",
        ],
    );
    make_ready(&server, "w1");
    let status = exited_within(&mut child, WITHIN).expect("start returns");
    let out = finish(child);
    assert_eq!(status.code(), Some(0), "stderr {}", stderr(&out));
    let json = stdout_json(&out);
    assert_eq!(json["branch"], "feat/p578-w1", "{json}");
    let worktree = PathBuf::from(json["worktree"].as_str().expect("worktree path"));
    assert!(worktree.join("README.md").is_file(), "{json}");
    let cwds = read_lines(&server.base.join("cwd.log"));
    assert_eq!(
        fs::canonicalize(&cwds[0]).unwrap(),
        fs::canonicalize(&worktree).unwrap(),
        "the agent runs in the worktree"
    );
    assert_eq!(
        focused_workspace(&server).as_deref(),
        Some(operator.as_str())
    );
    let workspace_id = json["workspace_id"].as_str().unwrap().to_string();

    let reaped = cli(&server, &["delegate", "reap", "w1", "--json"]);
    assert_eq!(reaped.status.code(), Some(0), "reap: {}", stderr(&reaped));
    assert_eq!(stdout_json(&reaped)["removed"], true);
    assert!(!worktree.exists(), "the checkout is removed");
    assert!(
        workspaces(&server)
            .iter()
            .all(|ws| ws["workspace_id"] != workspace_id.as_str()),
        "the workspace is closed"
    );
    assert!(
        !walk(&server.base.join("state"))
            .iter()
            .any(|p| p.ends_with("w1.json")),
        "the registry entry is removed"
    );
    let again = cli(&server, &["delegate", "status", "w1"]);
    assert_eq!(again.status.code(), Some(2), "a reaped delegate is gone");
}

/// E9: reap and send refuse an agent that is not a delegate, and leave it
/// running.
#[test]
fn a9_non_delegates_are_refused() {
    let server = start_server();
    let operator = operator_workspace(&server);
    let work = work_dir(&server);
    let started = cli(
        &server,
        &[
            "agent",
            "start",
            "mine",
            "--cwd",
            &work,
            "--no-focus",
            "--",
            "/bin/sh",
            "-c",
            "sleep 60",
        ],
    );
    assert!(started.status.success(), "{}", stderr(&started));
    delegate_pane(&server, "mine");
    let before = workspaces(&server).len();
    let b = brief(&server, "task.md", "x\n");
    for args in [
        vec!["delegate", "reap", "mine"],
        vec!["delegate", "send", "mine", "--brief", b.as_str()],
        vec!["delegate", "wait", "mine", "--timeout", "500"],
        vec!["delegate", "status", "mine"],
        vec!["delegate", "result", "mine"],
    ] {
        let out = cli(&server, &args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("not a delegate"),
            "{args:?}: {}",
            stderr(&out)
        );
    }
    assert!(agent_get(&server, "mine").is_some(), "the agent is alive");
    assert_eq!(workspaces(&server).len(), before);
    assert_eq!(
        focused_workspace(&server).as_deref(),
        Some(operator.as_str())
    );
    assert!(typed(&server).is_empty(), "nothing was typed anywhere");
}

/// E10: a name already taken by a live agent: exit 1, nothing created.
#[test]
fn a10_taken_name_exits_1_and_creates_nothing() {
    let server = start_server();
    operator_workspace(&server);
    let work = work_dir(&server);
    let started = cli(
        &server,
        &[
            "agent",
            "start",
            "taken",
            "--cwd",
            &work,
            "--no-focus",
            "--",
            "/bin/sh",
            "-c",
            "sleep 60",
        ],
    );
    assert!(started.status.success(), "{}", stderr(&started));
    delegate_pane(&server, "taken");
    let before = workspaces(&server).len();
    let b = brief(&server, "task.md", "x\n");
    let out = cli(
        &server,
        &["delegate", "start", "taken", "--brief", &b, "--cwd", &work],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("agent_name_taken"),
        "{}",
        stderr(&out)
    );
    assert_eq!(workspaces(&server).len(), before);
    assert!(read_lines(&server.base.join("argv.log")).is_empty());
}

/// E11: the harness dies before it is ready: exit 1, its workspace rolled
/// back, no registry entry.
#[test]
fn a11_failed_start_rolls_back() {
    let server = start_server();
    operator_workspace(&server);
    fs::write(server.base.join("die"), "").unwrap();
    let before = workspaces(&server).len();
    let b = brief(&server, "task.md", "x\n");
    let work = work_dir(&server);
    let mut child = cli_spawn(
        &server,
        &[
            "delegate",
            "start",
            "d1",
            "--brief",
            &b,
            "--cwd",
            &work,
            "--ready-timeout",
            "5000",
        ],
    );
    let status = exited_within(&mut child, WITHIN).expect("start gives up");
    let out = finish(child);
    assert_eq!(status.code(), Some(1), "stderr {}", stderr(&out));
    assert_eq!(
        read_lines(&server.base.join("argv.log")).len(),
        1,
        "the harness was launched once before the start gave up"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while workspaces(&server).len() != before && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        workspaces(&server).len(),
        before,
        "the workspace is rolled back"
    );
    assert!(
        !walk(&server.base.join("state"))
            .iter()
            .any(|p| p.ends_with("d1.json")),
        "no registry entry"
    );
}

/// #817: a harness that exits as the brief arrives is a failed start, never a
/// submitted round, and the delegate it leaves behind still answers `status`
/// and `result` as `gone` instead of "not a delegate".
#[test]
fn a21_harness_that_dies_at_the_brief_fails_the_start_and_reports_gone() {
    let server = start_server();
    operator_workspace(&server);
    fs::write(server.base.join("die-on-input"), "").unwrap();
    let b = brief(&server, "task.md", "x\n");
    let mut child = start_cwd(&server, "d1", &b, &["--json"]);
    make_ready(&server, "d1");
    let status = exited_within(&mut child, Duration::from_secs(40)).expect("start returns");
    let out = finish(child);
    assert_eq!(status.code(), Some(4), "stderr {}", stderr(&out));
    assert_eq!(stdout_json(&out)["outcome"], "gone", "{}", stdout(&out));
    assert!(
        !server.base.join("typed.log").exists(),
        "the harness never took the brief"
    );

    let status = cli(&server, &["delegate", "status", "d1", "--json"]);
    assert_eq!(status.status.code(), Some(0), "{}", stderr(&status));
    let json = stdout_json(&status);
    assert_eq!(json["agent_status"], "gone", "{json}");
    assert_eq!(json["round"], 1, "{json}");

    let result = cli(&server, &["delegate", "result", "d1", "--json"]);
    assert_eq!(result.status.code(), Some(4), "{}", stderr(&result));
    assert_eq!(stdout_json(&result)["outcome"], "gone");

    let reaped = cli(&server, &["delegate", "reap", "d1"]);
    assert_eq!(reaped.status.code(), Some(0), "reap: {}", stderr(&reaped));
    let gone = cli(&server, &["delegate", "status", "d1"]);
    assert_eq!(gone.status.code(), Some(2), "{}", stderr(&gone));
    assert!(
        stderr(&gone).contains("not a delegate"),
        "{}",
        stderr(&gone)
    );
}

/// #817: once a delegate's agent is gone, `send` and `wait` answer `gone`
/// (exit 4) from its record like `status` and `result`, and `send` types nothing
/// and leaves the round where it was.
#[test]
fn a22_send_and_wait_on_a_gone_delegate_report_gone() {
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
    let closed = request(
        &server,
        &format!(r#"{{"id":"pc","method":"pane.close","params":{{"pane_id":"{pane}"}}}}"#),
    );
    assert!(closed.get("error").is_none(), "{closed}");

    let b2 = brief(&server, "fix.md", "y\n");
    let sent = cli(
        &server,
        &["delegate", "send", "d1", "--brief", &b2, "--json"],
    );
    assert_eq!(sent.status.code(), Some(4), "{}", stderr(&sent));
    let json = stdout_json(&sent);
    assert_eq!(json["outcome"], "gone", "{json}");
    assert_eq!(json["round"], 1, "{json}");
    assert_eq!(typed(&server).len(), 1, "send typed nothing");

    let waited = cli(&server, &["delegate", "wait", "d1", "--json"]);
    assert_eq!(waited.status.code(), Some(4), "{}", stderr(&waited));
    assert_eq!(stdout_json(&waited)["outcome"], "gone");

    let status = cli(&server, &["delegate", "status", "d1", "--json"]);
    assert_eq!(status.status.code(), Some(0), "{}", stderr(&status));
    assert_eq!(stdout_json(&status)["round"], 1);
}

/// E12: `status` shows the delegate with a null goal; `result` maps an
/// unfinished turn to `running`.
#[test]
fn a12_status_and_result() {
    let server = start_server();
    operator_workspace(&server);
    let b = brief(&server, "task.md", "x\n");
    let mut child = start_cwd(
        &server,
        "d1",
        &b,
        &["--model", "opencode/fledge-alpha-free", "--json"],
    );
    let pane = make_ready(&server, "d1");
    assert_eq!(
        exited_within(&mut child, WITHIN).and_then(|s| s.code()),
        Some(0)
    );
    let out = finish(child);
    let started = stdout_json(&out);

    report(&server, &pane, "working");
    let status = cli(&server, &["delegate", "status", "d1", "--json"]);
    assert_eq!(status.status.code(), Some(0), "{}", stderr(&status));
    let json = stdout_json(&status);
    assert_eq!(json["name"], "d1", "{json}");
    assert_eq!(json["agent_status"], "working", "{json}");
    assert_eq!(json["mode"], "cwd", "{json}");
    assert_eq!(json["harness"], "opencode", "{json}");
    assert_eq!(json["model"], "opencode/fledge-alpha-free", "{json}");
    assert_eq!(json["round"], 1, "{json}");
    assert_eq!(json["workspace_id"], started["workspace_id"], "{json}");
    assert!(json["goal"].is_null(), "{json}");
    assert!(
        json.as_object().unwrap().contains_key("goal"),
        "goal is present and null: {json}"
    );

    // The user message is in, the reply is not finished yet.
    let conn = rusqlite::Connection::open(db_path(&server.base)).unwrap();
    let at = now_ms() as i64;
    conn.execute(
        "INSERT INTO message VALUES ('u1', ?1, ?2, ?2, '{\"role\":\"user\"}')",
        rusqlite::params![SES, at],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO part VALUES ('u1-p', 'u1', ?1, ?2, ?2, '{\"type\":\"text\",\"text\":\"brief\"}')",
        rusqlite::params![SES, at],
    )
    .unwrap();
    let result = cli(&server, &["delegate", "result", "d1", "--json"]);
    assert_eq!(result.status.code(), Some(0), "{}", stderr(&result));
    assert_eq!(stdout_json(&result)["outcome"], "running");
}

/// E13: the delegate's pane closes while the await blocks: exit 4 `gone`.
#[test]
fn a13_closed_pane_is_gone() {
    let server = start_server();
    operator_workspace(&server);
    let b = brief(&server, "task.md", "x\n");
    let mut child = start_cwd(
        &server,
        "d1",
        &b,
        &["--await", "--timeout", "30000", "--json"],
    );
    let pane = make_ready(&server, "d1");
    wait_typed(&server, 1);
    report(&server, &pane, "working");
    // Allow start's confirmation poll to hand off to the await before closing.
    thread::sleep(Duration::from_millis(500));
    let closed = request(
        &server,
        &format!(r#"{{"id":"pc","method":"pane.close","params":{{"pane_id":"{pane}"}}}}"#),
    );
    assert!(closed.get("error").is_none(), "{closed}");
    let status = exited_within(&mut child, WITHIN).expect("the await notices");
    let out = finish(child);
    assert_eq!(status.code(), Some(4), "stderr {}", stderr(&out));
    assert_eq!(stdout_json(&out)["outcome"], "gone");
}

/// E14: a delegate name must be a safe file name: letters, digits, `.`, `_`,
/// `-`, starting with a letter or digit, at most 64 characters.
#[test]
fn a14_unsafe_names_are_usage_errors() {
    let server = start_server();
    operator_workspace(&server);
    let before = workspaces(&server).len();
    let work = work_dir(&server);
    let b = brief(&server, "task.md", "x\n");
    let state = server.base.join("state");
    let startup_files: std::collections::BTreeSet<_> = walk(&state).into_iter().collect();
    let long = "n".repeat(65);
    for name in ["../x", "a/b", ".hidden", "-dash", "sp ace", long.as_str()] {
        let out = cli(
            &server,
            &["delegate", "start", name, "--brief", &b, "--cwd", &work],
        );
        assert_eq!(out.status.code(), Some(2), "{name:?}: {}", stderr(&out));
    }
    assert_eq!(workspaces(&server).len(), before);
    // Rejected names must add no files beyond the server's startup state.
    let unexpected: Vec<_> = walk(&state)
        .into_iter()
        .filter(|path| !startup_files.contains(path))
        .collect();
    assert!(
        unexpected.is_empty(),
        "unsafe delegate names wrote state: {unexpected:?}"
    );
}

/// E15: one round at a time: a `send` while a round is being awaited is
/// refused as busy and types nothing.
#[test]
fn a15_send_while_a_round_is_awaited_is_busy() {
    let server = start_server();
    operator_workspace(&server);
    let b = brief(&server, "task.md", "x\n");
    let mut child = start_cwd(
        &server,
        "d1",
        &b,
        &["--await", "--timeout", "30000", "--json"],
    );
    let pane = make_ready(&server, "d1");
    wait_typed(&server, 1);
    let r1 = brief(&server, "r1.md", "y\n");
    let busy = cli(&server, &["delegate", "send", "d1", "--brief", &r1]);
    assert_eq!(busy.status.code(), Some(1), "{}", stderr(&busy));
    assert!(stderr(&busy).contains("busy"), "{}", stderr(&busy));
    play_turn(&server, &pane, "t1", "DONE: one");
    let status = exited_within(&mut child, WITHIN).expect("round 1 returns");
    assert_eq!(status.code(), Some(0));
    finish(child);
    assert_eq!(typed(&server).len(), 1, "the busy send typed nothing");
}

/// E16: the screen goes idle before opencode has written the reply (the
/// normal plugin order): `no_result` inside the grace is "not yet".
#[test]
fn a16_reply_written_after_idle_is_still_reported() {
    let server = start_server();
    operator_workspace(&server);
    let b = brief(&server, "task.md", "x\n");
    let mut child = start_cwd(
        &server,
        "d1",
        &b,
        &["--await", "--timeout", "30000", "--json"],
    );
    let pane = make_ready(&server, "d1");
    wait_typed(&server, 1);
    report(&server, &pane, "working");
    thread::sleep(Duration::from_millis(100));
    report(&server, &pane, "idle");
    thread::sleep(Duration::from_millis(4 * SETTLE_MS));
    assert!(
        child.try_wait().unwrap().is_none(),
        "no reply yet: the await keeps polling"
    );
    write_reply(&server, "t1", "DONE: late reply");
    let status = exited_within(&mut child, WITHIN).expect("the await returns");
    let out = finish(child);
    assert_eq!(status.code(), Some(0), "stderr {}", stderr(&out));
    assert_eq!(stdout_json(&out)["status_text"], "late reply");
}

/// E17: reap in worktree mode still removes the checkout when the
/// delegate's workspace was already closed by hand.
#[test]
fn a17_reap_after_the_workspace_was_closed_removes_the_checkout() {
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
            "feat/p578-w2",
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
    let workspace_id = json["workspace_id"].as_str().unwrap().to_string();
    let closed = request(
        &server,
        &format!(
            r#"{{"id":"wc","method":"workspace.close","params":{{"workspace_id":"{workspace_id}"}}}}"#
        ),
    );
    assert!(closed.get("error").is_none(), "{closed}");
    assert!(
        worktree.exists(),
        "closing the workspace leaves the checkout"
    );

    let reaped = cli(&server, &["delegate", "reap", "w1", "--json"]);
    assert_eq!(reaped.status.code(), Some(0), "reap: {}", stderr(&reaped));
    assert!(!worktree.exists(), "the checkout is removed by path");
    assert!(
        !walk(&server.base.join("state"))
            .iter()
            .any(|p| p.ends_with("w1.json")),
        "the registry entry is removed"
    );
}

/// E18: the brief is typed only into an idle agent: a start whose agent
/// shows a permission prompt waits, and so does a send.
#[test]
fn a18_brief_waits_for_an_idle_agent() {
    let server = start_server();
    operator_workspace(&server);
    fs::write(server.base.join("screen"), screen_for("blocked")).unwrap();
    let b = brief(&server, "task.md", "x\n");
    let mut child = start_cwd(
        &server,
        "d1",
        &b,
        &["--await", "--timeout", "30000", "--json"],
    );
    let pane = delegate_pane(&server, "d1");
    report_session(&server, &pane);
    report(&server, &pane, "blocked");
    thread::sleep(Duration::from_millis(4 * SETTLE_MS));
    assert!(typed(&server).is_empty(), "nothing is typed into a prompt");
    report(&server, &pane, "idle");
    assert_eq!(wait_typed(&server, 1), vec![expected_line(&b)]);
    play_turn(&server, &pane, "t1", "DONE: one");
    let status = exited_within(&mut child, WITHIN).expect("round 1 returns");
    assert_eq!(status.code(), Some(0), "stderr {}", stderr(&finish(child)));

    report(&server, &pane, "blocked");
    let r1 = brief(&server, "r1.md", "y\n");
    let mut sender = cli_spawn(
        &server,
        &["delegate", "send", "d1", "--brief", &r1, "--json"],
    );
    thread::sleep(Duration::from_millis(4 * SETTLE_MS));
    assert_eq!(
        typed(&server).len(),
        1,
        "a send does not type into a prompt"
    );
    report(&server, &pane, "idle");
    let status = exited_within(&mut sender, WITHIN).expect("the send returns");
    assert_eq!(status.code(), Some(0), "stderr {}", stderr(&finish(sender)));
    assert_eq!(wait_typed(&server, 2)[1], expected_line(&r1));
}

/// E19: a failed start in worktree mode removes the checkout it created.
#[test]
fn a19_failed_worktree_start_removes_its_checkout() {
    let server = start_server();
    operator_workspace(&server);
    let repo = committed_repo(&server);
    let repo_s = repo.to_string_lossy().into_owned();
    fs::write(server.base.join("die"), "").unwrap();
    let before = workspaces(&server).len();
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
            "feat/p578-w3",
            "--ready-timeout",
            "5000",
        ],
    );
    let status = exited_within(&mut child, WITHIN).expect("start gives up");
    let out = finish(child);
    assert_eq!(status.code(), Some(1), "stderr {}", stderr(&out));
    assert_eq!(read_lines(&server.base.join("argv.log")).len(), 1);
    let deadline = Instant::now() + Duration::from_secs(5);
    while workspaces(&server).len() != before && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(workspaces(&server).len(), before, "the workspace is gone");
    let leftovers = walk(&server.base.join("wt"));
    assert!(
        leftovers.is_empty(),
        "no checkout is left behind: {leftovers:?}"
    );
    assert!(
        !walk(&server.base.join("state"))
            .iter()
            .any(|p| p.ends_with("w1.json")),
        "no registry entry"
    );
}

/// A process exists before its first screen paint, so that alone is not ready.
#[test]
fn a20_start_waits_for_the_first_screen_paint() {
    let server = start_server();
    operator_workspace(&server);
    fs::write(server.base.join("starting"), "").unwrap();
    let b = brief(&server, "task.md", "x\n");
    let work = work_dir(&server);
    let mut child = cli_spawn(
        &server,
        &[
            "delegate", "start", "d1", "--brief", &b, "--cwd", &work, "--json",
        ],
    );
    let pane = delegate_pane(&server, "d1");
    let deadline = Instant::now() + WITHIN;
    while !server.base.join("startup-entered").exists() {
        assert!(
            Instant::now() < deadline,
            "the harness reaches its startup gate"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let premature = exited_within(&mut child, Duration::from_secs(2));
    let screen = cli(
        &server,
        &[
            "pane", "read", &pane, "--source", "recent", "--format", "text",
        ],
    );
    fs::write(server.base.join("screen"), screen_for("idle")).unwrap();
    fs::remove_file(server.base.join("starting")).unwrap();
    assert!(String::from_utf8_lossy(&screen.stdout).trim().is_empty());
    assert!(
        premature.is_none(),
        "a blank startup screen must not receive the brief"
    );
    wait_typed(&server, 1);
    report(&server, &pane, "working");
    let status = exited_within(&mut child, WITHIN).expect("start returns after first paint");
    let out = finish(child);
    assert_eq!(status.code(), Some(0), "{}", stderr(&out));
    wait_typed(&server, 1);
}

/// A startup session cannot confirm a brief that the input handler discarded.
#[test]
fn opencode_start_fails_loudly_when_startup_discards_brief() {
    let server = start_server();
    operator_workspace(&server);
    fs::write(server.base.join("ignore-input-ms"), "60000").unwrap();
    let b = brief(&server, "task.md", "x\n");
    let before = workspaces(&server).len();
    let mut child = start_cwd(&server, "d1", &b, &["--json"]);
    make_ready(&server, "d1");
    let status =
        exited_within(&mut child, Duration::from_secs(40)).expect("confirmation is bounded");
    let out = finish(child);
    assert_eq!(status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("brief submission could not be confirmed"),
        "{}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("flk delegate reap d1"),
        "{}",
        stderr(&out)
    );
    assert!(!server.base.join("typed.log").exists());
    assert_eq!(workspaces(&server).len(), before + 1);
    assert!(walk(&server.base.join("state"))
        .iter()
        .any(|p| p.ends_with("d1.json")));
}

#[test]
fn opencode_late_submission_confirms_without_teardown() {
    let server = start_server();
    operator_workspace(&server);
    fs::write(server.base.join("late-submit-ms"), "1500").unwrap();
    let b = brief(&server, "late.md", "x\n");
    let mut child = start_cwd(&server, "d1", &b, &["--json"]);
    let pane = make_ready(&server, "d1");
    assert!(exited_within(&mut child, Duration::from_millis(500)).is_none());
    let status = exited_within(&mut child, WITHIN).expect("late turn confirms");
    let out = finish(child);
    assert_eq!(status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(typed(&server), vec![expected_line(&b)]);
    assert_eq!(agent_get(&server, "d1").unwrap()["pane_id"], pane);
}

#[test]
fn opencode_held_composer_retries_enter_only_once() {
    let server = start_server();
    operator_workspace(&server);
    fs::write(server.base.join("hold-first-enter"), "").unwrap();
    let b = brief(&server, "held.md", "x\n");
    let mut child = start_cwd(&server, "d1", &b, &["--json"]);
    let pane = make_ready(&server, "d1");
    let deadline = Instant::now() + WITHIN;
    while !fs::read_to_string(server.base.join("screen"))
        .unwrap_or_default()
        .contains("╹")
    {
        assert!(Instant::now() < deadline, "the composer is drawn");
        thread::sleep(Duration::from_millis(20));
    }
    thread::sleep(Duration::from_millis(200));
    let painted = cli(
        &server,
        &[
            "pane", "read", &pane, "--source", "recent", "--format", "text",
        ],
    );

    let status = exited_within(&mut child, Duration::from_secs(40)).expect("retry confirms");
    let out = finish(child);
    assert_eq!(
        status.code(),
        Some(0),
        "{}\ncomposer:\n{}",
        stderr(&out),
        stdout(&painted)
    );
    assert_eq!(typed(&server), vec![expected_line(&b)]);
    assert_eq!(
        read_lines(&server.base.join("input.log")),
        vec![expected_line(&b), String::new()]
    );
}

#[test]
fn opencode_empty_composer_never_retypes_after_dropped_input() {
    let server = start_server();
    operator_workspace(&server);
    fs::write(server.base.join("ignore-input-ms"), "10000").unwrap();
    let b = brief(&server, "empty.md", "x\n");
    let mut child = start_cwd(&server, "d1", &b, &["--json"]);
    let pane = make_ready(&server, "d1");
    fs::write(
        server.base.join("screen"),
        "┃\n┃  Ask anything…\n┃\n┃  Build test-model\n╹\ntab agents ctrl+p commands\n",
    )
    .unwrap();
    report_session(&server, &pane);
    let status =
        exited_within(&mut child, Duration::from_secs(10)).expect("confirmation is bounded");
    let out = finish(child);
    assert_eq!(status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("submission could not be confirmed"));
    assert!(typed(&server).is_empty());
    assert_eq!(
        read_lines(&server.base.join("input.log")),
        vec![expected_line(&b)]
    );
    assert!(
        agent_get(&server, "d1").is_some(),
        "workspace is kept for inspection"
    );
}

#[test]
fn opencode_ambiguous_submission_keeps_workspace_and_does_not_retry() {
    let server = start_server();
    operator_workspace(&server);
    fs::write(server.base.join("manual-submit"), "").unwrap();
    let b = brief(&server, "ambiguous.md", "x\n");
    let before = workspaces(&server).len();
    let mut child = start_cwd(&server, "d1", &b, &["--json"]);
    make_ready(&server, "d1");
    let status = exited_within(&mut child, Duration::from_secs(40))
        .expect("ambiguous confirmation is bounded");
    let out = finish(child);
    assert_eq!(status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("Workspace kept for inspection"),
        "{}",
        stderr(&out)
    );
    assert_eq!(workspaces(&server).len(), before + 1);
    assert!(agent_get(&server, "d1").is_some());
    assert_eq!(typed(&server), vec![expected_line(&b)]);
    assert_eq!(
        read_lines(&server.base.join("input.log")),
        vec![expected_line(&b)]
    );
}

/// #818: both placement paths must return one agent at the root address.
#[test]
fn delegate_start_uses_single_root_pane() {
    for worktree in [false, true] {
        let server = start_server_with_shell_startup(true);
        operator_workspace(&server);
        let b = brief(&server, "task.md", "x\n");
        let mut child = if worktree {
            let repo = committed_repo(&server);
            cli_spawn(
                &server,
                &[
                    "delegate",
                    "start",
                    "d1",
                    "--brief",
                    &b,
                    "--worktree",
                    "--repo",
                    repo.to_str().unwrap(),
                    "--branch",
                    "test/818",
                    "--json",
                ],
            )
        } else {
            start_cwd(&server, "d1", &b, &["--json"])
        };
        let pane = make_ready(&server, "d1");
        let status = exited_within(&mut child, WITHIN).expect("delegate started");
        let out = finish(child);
        assert_eq!(status.code(), Some(0), "{}", stderr(&out));
        let result = stdout_json(&out);
        let workspace = result["workspace_id"].as_str().unwrap();
        assert_eq!(pane, format!("{workspace}:p1"));
        let listed = request(
            &server,
            &serde_json::json!({
                "id": "panes", "method": "pane.list", "params": {"workspace_id": workspace}
            })
            .to_string(),
        );
        assert_eq!(listed["result"]["panes"].as_array().unwrap().len(), 1);
    }
}

/// Start through the socket immediately after allocation, while shell rc runs.
#[test]
fn agent_start_replaces_slow_startup_shell_after_workspace_or_worktree_create() {
    for worktree in [false, true] {
        let server = start_server_with_shell_startup(true);
        let allocation = if worktree {
            let repo = committed_repo(&server);
            serde_json::json!({
                "id": "create", "method": "worktree.create",
                "params": {"cwd": repo, "branch": "test/818-api", "path": server.base.join("checkout")}
            })
        } else {
            serde_json::json!({
                "id": "create", "method": "workspace.create",
                "params": {"cwd": server.base.join("work")}
            })
        };
        let created = request(&server, &allocation.to_string());
        let workspace = created["result"]["workspace"]["workspace_id"]
            .as_str()
            .unwrap_or_else(|| panic!("allocation failed: {created}"));
        let started = request(
            &server,
            &serde_json::json!({
                "id": "start", "method": "agent.start",
                "params": {"name": "slow-rc-agent", "workspace_id": workspace,
                    "argv": ["/bin/sh", "-c", "sleep 30"]}
            })
            .to_string(),
        );
        assert_eq!(started["result"]["type"], "agent_started", "{started}");
        assert_eq!(
            started["result"]["agent"]["pane_id"],
            format!("{workspace}:p1")
        );
        let listed = request(
            &server,
            &serde_json::json!({
                "id": "panes", "method": "pane.list", "params": {"workspace_id": workspace}
            })
            .to_string(),
        );
        assert_eq!(listed["result"]["panes"].as_array().unwrap().len(), 1);
    }
}
