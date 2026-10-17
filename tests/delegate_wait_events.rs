//! #1030 acceptance: `flk delegate wait` on a server with `events.read`
//! makes a request per state change, not five a second.
//!
//! The fixture below is copied from the frozen `tests/delegate.rs` (a fake
//! `opencode` whose screen the test draws, an opencode database under an
//! isolated `XDG_DATA_HOME`, HOME/worktree/git isolation). The waiter talks
//! to the isolated server through a proxy socket that counts its requests.

// TracedCommand (logging redesign PR-3) polices flock's shipped code; this
// harness drives the compiled flock binary through raw Command.
#![allow(clippy::disallowed_methods)]
// The shared fixture is copied whole; not every helper is used here.
#![allow(dead_code)]

mod support;

use support::paths::app_dir_name;

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

impl Server {
    /// Stop this server process and start a new one on the same sandbox:
    /// a cold restart, not a live handoff (#1010). The session snapshot is
    /// what carries the panes across, so the caller waits for it first.
    fn restart(mut self) -> Server {
        let pid = self.child.process_id();
        let _ = self.child.kill();
        if let Some(pid) = pid {
            let deadline = Instant::now() + Duration::from_secs(5);
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
        let _ = fs::remove_file(&self.socket);
        let base = self.base.clone();
        // The base outlives this process: Drop would delete it.
        std::mem::forget(self);
        spawn_server(base)
    }

    /// Wait until the debounced session snapshot mentions every `needle`.
    fn wait_session_saved(&self, needles: &[&str]) {
        let path = self
            .base
            .join("config")
            .join(app_dir_name())
            .join("session.json");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let saved = fs::read_to_string(&path).unwrap_or_default();
            if needles.iter().all(|needle| saved.contains(needle)) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "session snapshot never recorded {needles:?}: {saved}"
            );
            thread::sleep(Duration::from_millis(100));
        }
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
printf '%s\n' "${{FLEET_PROFILE:-}}" >> '{base}/profile.log'
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
    start_server_with(slow_startup, "")
}

fn start_server_with(slow_startup: bool, extra_config: &str) -> Server {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let base = PathBuf::from(format!("/tmp/hdel-{}-{nanos}", std::process::id()));
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
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
            "onboarding = false\n\n{terminal_config}\n[worktrees]\ndirectory = \"{}\"\n{extra_config}",
            base.join("wt").display()
        ),
    )
    .unwrap();
    write_fake_opencode(&base);
    support::write_fake_gh(&bin_dir(&base));
    create_db(&base);
    spawn_server(base)
}

/// Start a server on a sandbox `base` that is already laid out, so a test
/// can stop one and start a fresh process on the same state (#1010).
fn spawn_server(base: PathBuf) -> Server {
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket = runtime_dir.join("flock.sock");
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

/// Forward every connection on `proxy` to `server`, recording the method of
/// each request in order.
fn counting_proxy(proxy: &Path, server: PathBuf) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
    let methods = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let listener = std::os::unix::net::UnixListener::bind(proxy).unwrap();
    let seen = methods.clone();
    thread::spawn(move || {
        for client in listener.incoming() {
            let Ok(client) = client else { return };
            let mut reader = BufReader::new(client.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            let request: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
            seen.lock()
                .unwrap()
                .push(request["method"].as_str().unwrap_or("?").to_string());
            let Ok(mut upstream) = UnixStream::connect(&server) else {
                continue;
            };
            upstream.write_all(line.as_bytes()).unwrap();
            let mut down = upstream.try_clone().unwrap();
            let mut client_out = client;
            thread::spawn(move || {
                let _ = std::io::copy(&mut reader, &mut upstream);
            });
            thread::spawn(move || {
                let _ = std::io::copy(&mut down, &mut client_out);
                let _ = client_out.shutdown(std::net::Shutdown::Both);
            });
        }
    });
    methods
}

/// #1030: the wait samples on each event about the delegate and once when
/// the dwell is due, and makes no request while nothing changes. Polling
/// every 200 ms, as before, cost `agent.get` and a pane read five times a
/// second for the whole wait.
#[test]
fn a_delegate_wait_requests_once_per_state_change() {
    let server = start_server();
    operator_workspace(&server);
    let b = brief(&server, "task.md", "x\n");
    // The registry is keyed by the socket path, so the whole delegate talks
    // through the proxy, and only the wait's own requests are counted.
    let proxy = server.base.join("count.sock");
    let methods = counting_proxy(&proxy, server.socket.clone());
    let work = work_dir(&server);
    let mut child = command(
        &server,
        &[
            "delegate", "start", "d1", "--brief", &b, "--cwd", &work, "--settle", SETTLE, "--json",
        ],
    )
    .env("FLOCK_SOCKET_PATH", &proxy)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    let pane = make_ready(&server, "d1");
    exited_within(&mut child, WITHIN).expect("start returns after submitting");
    let cursor = stdout_json(&finish(child))["turn_cursor"]
        .as_str()
        .expect("turn_cursor")
        .to_string();

    let skip = methods.lock().unwrap().len();
    let mut waiter = command(
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
    )
    .env("FLOCK_SOCKET_PATH", &proxy)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    let started = Instant::now();
    // The submit itself plays a turn on the fake, which the wait settles on
    // and then rejects for want of a reply: let that pass first.
    thread::sleep(Duration::from_millis(1500));
    report(&server, &pane, "working");
    thread::sleep(Duration::from_millis(500));
    let quiet_from = methods.lock().unwrap().len();
    thread::sleep(Duration::from_millis(2000));
    let quiet = methods.lock().unwrap()[quiet_from..].to_vec();
    write_reply(&server, "t1", "DONE: counted");
    report(&server, &pane, "idle");
    let status = exited_within(&mut waiter, WITHIN).expect("the wait returns");
    let waited = started.elapsed();
    let out = finish(waiter);
    assert_eq!(status.code(), Some(0), "stderr {}", stderr(&out));
    assert_eq!(stdout_json(&out)["outcome"], "done");

    let methods = methods.lock().unwrap()[skip..].to_vec();
    let count = |method: &str| methods.iter().filter(|seen| *seen == method).count();
    eprintln!("{} requests in {waited:?}: {methods:?}", methods.len());
    assert!(
        count("events.read") >= 2,
        "the wait is evented: {methods:?}"
    );
    // Two seconds of a working agent with nothing changing. The polling
    // wait sampled `agent.get` and the pane every 200 ms: 20 requests.
    assert!(quiet.len() <= 1, "{quiet:?} while nothing changed");
    assert!(
        (methods.len() as f64) < waited.as_secs_f64() * 5.0,
        "{} requests in {waited:?}: {methods:?}",
        methods.len()
    );
}
