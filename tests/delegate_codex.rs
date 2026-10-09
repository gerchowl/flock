//! Codex delegate acceptance: real session hook, fake screen, and fixture rollout.
// The fixture drives the compiled CLI with raw Command, as the existing delegate tests do.
#![allow(clippy::disallowed_methods)]
mod support;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use support::{
    cleanup_test_base, register_runtime_dir, register_spawned_flock_pid,
    unregister_spawned_flock_pid, wait_for_socket,
};
const SETTLE: &str = "300";
const SETTLE_MS: u64 = 300;
const WITHIN: Duration = Duration::from_secs(20);
const SES: &str = "00000000-0000-4000-8000-000000000613";
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

fn write_fake_codex(base: &Path) {
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
         while IFS= read -r line; do printf '%s\\n' \"$line\" >> '{base}/typed.log'; printf '\\033[2J\\033[H• Working (1s • esc to interrupt)\\n'; sleep 0.4; printf '\\033[2J\\033[H%s\\n' \"$(cat '{base}/screen')\"; done\n",
        base = base.display()
    );
    let path = bin.join("codex");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn start_server() -> Server {
    start_server_with_rows(24)
}

fn start_server_with_rows(rows: u16) -> Server {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let base = PathBuf::from(format!("/tmp/hdcodex-{}-{nanos}", std::process::id()));
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
    write_fake_codex(&base);
    fs::write(base.join("screen"), screen_for("working")).unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
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
    cmd.env("CODEX_HOME", base.join("home/.codex"));
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
        .env("CODEX_HOME", server.base.join("home/.codex"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("PATH", search_path(&server.base));
    cmd
}

fn cli(server: &Server, args: &[&str]) -> Output {
    command(server, args).output().unwrap()
}

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

fn timestamp_now() -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = elapsed.as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let mut year = 1970i64;
    let mut days = days as i64;
    loop {
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let length = if leap { 366 } else { 365 };
        if days < length {
            break;
        }
        days -= length;
        year += 1;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let months = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 0usize;
    while days >= months[month] {
        days -= months[month];
        month += 1;
    }
    format!(
        "{year:04}-{:02}-{:02}T{hour:02}:{minute:02}:{second:02}.{:03}Z",
        month + 1,
        days + 1,
        elapsed.subsec_millis()
    )
}

fn play_turn(server: &Server, pane: &str, text: &str) {
    report(server, pane, "working");
    thread::sleep(Duration::from_millis(100));
    write_reply(server, "t1", text);
    report(server, pane, "idle");
}

fn make_ready(server: &Server, name: &str) -> String {
    let pane = delegate_pane(server, name);
    report_session(server, &pane);
    report(server, &pane, "idle");
    pane
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

fn start_codex(server: &Server, name: &str, brief_path: &str, extra: &[&str]) -> Child {
    let work = work_dir(server);
    let mut args = vec![
        "delegate",
        "start",
        name,
        "--harness",
        "codex",
        "--brief",
        brief_path,
        "--cwd",
        &work,
        "--settle",
        SETTLE,
    ];
    args.extend_from_slice(extra);
    cli_spawn(server, &args)
}

const HOOK_REVIEW: &str = "Hooks need review\n1 hook is new or changed.\nHooks can run outside the sandbox after you trust them.\n\n› 1. Review hooks\n2. Trust all and continue\n3. Continue without trusting (hooks won't run)\n\nenter confirm · esc skip\n";

fn screen_for(state: &str) -> String {
    match state {
        "working" => "OpenAI Codex\n• Working (1s • esc to interrupt)\n› \n".into(),
        "idle" => "OpenAI Codex\n› Ask Codex to do anything\n  ? for shortcuts\n".into(),
        other => panic!("unexpected state {other}"),
    }
}

fn report_session(server: &Server, pane: &str) {
    let mut child = command(server, &["hook", "codex", "session"])
        .env("FLOCK_ENV", "1")
        .env("FLOCK_PANE_ID", pane)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            format!(r#"{{"hook_event_name":"SessionStart","session_id":"{SES}"}}"#).as_bytes(),
        )
        .unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(result.status.success(), "{}", stderr(&result));
}

fn write_reply(server: &Server, turn: &str, text: &str) {
    let dir = server.base.join("home/.codex/sessions/2026/01/01");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("rollout-2026-01-01T00-00-00-{SES}.jsonl"));
    let timestamp = timestamp_now();
    let records = [
        serde_json::json!({"timestamp": timestamp, "type": "event_msg", "payload": {"type":"task_started", "turn_id":turn}}),
        serde_json::json!({"timestamp": timestamp, "type": "response_item", "payload": {"type":"message", "role":"assistant", "phase":"final_answer", "content":[{"type":"output_text","text":text}]}}),
        serde_json::json!({"timestamp": timestamp, "type": "event_msg", "payload": {"type":"task_complete", "turn_id":turn, "last_agent_message":text}}),
    ];
    let mut body = fs::read_to_string(&path).unwrap_or_default();
    for record in records {
        body.push_str(&format!("{record}\n"));
    }
    fs::write(path, body).unwrap();
}

#[test]
fn codex_delegate_start_ready_submit_settled_and_result() {
    let server = start_server();
    let operator = operator_workspace(&server);
    let b = brief(&server, "task.md", "build it\n");
    let mut child = start_codex(
        &server,
        "d1",
        &b,
        &["--model", "m1", "--await", "--timeout", "30000", "--json"],
    );
    let pane = make_ready(&server, "d1");
    assert_eq!(wait_typed(&server, 1), vec![expected_line(&b)]);
    assert!(exited_within(&mut child, Duration::from_millis(3 * SETTLE_MS)).is_none());
    play_turn(&server, &pane, "Built it.\nDONE: built and pushed");
    assert_eq!(exited_within(&mut child, WITHIN).unwrap().code(), Some(0));
    let out = finish(child);
    let result = stdout_json(&out);
    assert_eq!(result["outcome"], "done", "{result}");
    assert_eq!(result["status_text"], "built and pushed");
    assert_eq!(result["session_id"], SES);
    assert_ne!(result["workspace_id"], operator);
    assert_eq!(agent_get(&server, "d1").unwrap()["agent"], "codex");
    assert_eq!(
        read_lines(&server.base.join("argv.log")),
        vec!["--ask-for-approval never --sandbox workspace-write --model m1"]
    );
    assert!(!read_lines(&server.base.join("argv.log"))
        .join(" ")
        .contains("danger-full-access"));
    assert_recorded_sandbox(&server, "workspace-write");
    let read = cli(&server, &["delegate", "result", "d1", "--json"]);
    assert!(read.status.success(), "{}", stderr(&read));
    assert!(stdout_json(&read)["text"]
        .as_str()
        .unwrap()
        .contains("Built it."));
}

#[test]
fn codex_delegate_hook_review_is_refused_without_typing_the_brief() {
    let server = start_server();
    operator_workspace(&server);
    let before = workspaces(&server).len();
    let b = brief(&server, "task.md", "do it\n");
    let mut child = start_codex(
        &server,
        "blocked",
        &b,
        &["--ready-timeout", "20000", "--json"],
    );
    let pane = delegate_pane(&server, "blocked");
    // Wait for the fake harness's first paint before presenting its dialog,
    // matching the startup sequence instead of racing initial PTY geometry.
    report(&server, &pane, "working");
    fs::write(server.base.join("screen"), HOOK_REVIEW).unwrap();
    let status = exited_within(&mut child, WITHIN).expect("hook review refusal");
    let out = finish(child);
    if status.success() {
        let pane = delegate_pane(&server, "blocked");
        let screen = cli(
            &server,
            &[
                "pane", "read", &pane, "--source", "recent", "--format", "text",
            ],
        );
        panic!(
            "unexpected readiness: {}\nscreen: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&screen.stdout),
            stderr(&screen)
        );
    }
    assert_eq!(status.code(), Some(1), "{}", stderr(&out));
    let message = stderr(&out);
    assert!(
        message.contains("Codex is waiting on its hook-review dialog"),
        "{message}"
    );
    assert!(
        message.contains("#626") && message.contains("flk integration install codex"),
        "{message}"
    );
    assert!(typed(&server).is_empty());
    assert_eq!(workspaces(&server).len(), before);
    assert!(agent_get(&server, "blocked").is_none());
}

/// Log every byte, including input without Enter, so a menu cannot silently
/// receive a brief while the CLI still reports a refusal.
fn startup_screen_harness(server: &Server, screen: &str) {
    fs::write(server.base.join("screen"), screen).unwrap();
    fs::create_dir_all(server.base.join("raw")).unwrap();
    let script = server.base.join("raw/codex");
    fs::write(
        &script,
        r#"import os, sys, tty
from pathlib import Path
base = Path(__file__).parent.parent
tty.setraw(0)
log = base / 'startup-input'
log.write_bytes(b'')
screen = (base / 'screen').read_text()
sys.stdout.write('\x1b[2J\x1b[H' + screen.replace('\n', '\r\n'))
sys.stdout.flush()
while True:
    byte = os.read(0, 1)
    with log.open('ab') as out:
        out.write(byte)
    if byte == b'\r':
        sys.stdout.write('\x1b[2J\x1b[H• Working (1s • esc to interrupt)\r\n')
        sys.stdout.flush()
"#,
    )
    .unwrap();
    fs::write(
        bin_dir(&server.base).join("codex"),
        format!("#!/bin/sh\nexec python3 '{}'\n", script.display()),
    )
    .unwrap();
}

#[test]
fn codex_delegate_startup_passive_banners_submit_and_confirm() {
    let server = start_server_with_rows(40);
    operator_workspace(&server);
    startup_screen_harness(
        &server,
        include_str!("fixtures/codex/startup-passive-banners.txt"),
    );
    let b = brief(&server, "task.md", "do it\n");
    let mut child = start_codex(
        &server,
        "banners",
        &b,
        &["--ready-timeout", "10000", "--json"],
    );
    let status = exited_within(&mut child, WITHIN).expect("confirmed submission");
    let out = finish(child);
    assert!(status.success(), "{} / {}", stdout(&out), stderr(&out));
    assert_eq!(stdout_json(&out)["name"], "banners");
    assert_eq!(stdout_json(&out)["round"], 1);
    assert_eq!(
        fs::read(server.base.join("startup-input")).unwrap(),
        format!("{}\r", expected_line(&b)).as_bytes()
    );
    // Only the child's receipt of Enter paints Working. A successful start
    // therefore proves the guarded submit observed that transition.
    assert_eq!(
        agent_get(&server, "banners").unwrap()["agent_status"],
        "working"
    );
}

#[test]
fn codex_delegate_startup_update_dialog_refuses_without_typing() {
    let server = start_server_with_rows(40);
    operator_workspace(&server);
    let before = workspaces(&server).len();
    startup_screen_harness(
        &server,
        include_str!("fixtures/codex/startup-update-dialog.txt"),
    );
    let b = brief(&server, "task.md", "do it\n");
    let mut child = start_codex(
        &server,
        "update",
        &b,
        &["--ready-timeout", "10000", "--json"],
    );
    let status = exited_within(&mut child, WITHIN).expect("update dialog refusal");
    let out = finish(child);
    assert_eq!(
        status.code(),
        Some(1),
        "{} / {}",
        stdout(&out),
        stderr(&out)
    );
    assert!(stderr(&out).contains("the brief was not submitted"));
    assert!(
        stderr(&out).contains("unknown_composer"),
        "{}",
        stderr(&out)
    );
    assert!(fs::read(server.base.join("startup-input"))
        .unwrap()
        .is_empty());
    assert_eq!(workspaces(&server).len(), before);
    assert!(agent_get(&server, "update").is_none());
}

#[test]
fn codex_delegate_session_hook_can_arrive_after_submit() {
    let server = start_server();
    operator_workspace(&server);
    let b = brief(&server, "task.md", "do it\n");
    let mut child = start_codex(
        &server,
        "d1",
        &b,
        &[
            "--sandbox",
            "danger-full-access",
            "--await",
            "--timeout",
            "30000",
            "--json",
        ],
    );
    let pane = delegate_pane(&server, "d1");
    report(&server, &pane, "idle");
    wait_typed(&server, 1);
    let result = cli(&server, &["agent", "result", &pane]);
    assert!(
        stderr(&result).contains("no_agent_session"),
        "{}",
        stderr(&result)
    );
    report(&server, &pane, "working");
    thread::sleep(Duration::from_millis(4 * SETTLE_MS));
    assert!(child.try_wait().unwrap().is_none());
    report_session(&server, &pane);
    write_reply(&server, "late-hook", "DONE: session reported");
    report(&server, &pane, "idle");
    assert_eq!(exited_within(&mut child, WITHIN).unwrap().code(), Some(0));
    let out = finish(child);
    assert_eq!(stdout_json(&out)["status_text"], "session reported");
    assert_recorded_sandbox(&server, "danger-full-access");
    assert_eq!(
        read_lines(&server.base.join("argv.log")),
        vec!["--ask-for-approval never --sandbox danger-full-access"]
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

fn assert_sandbox_refusal_creates_nothing(harness: &str) {
    let server = start_server();
    operator_workspace(&server);
    let before = workspaces(&server);
    let repo = committed_repo(&server);
    let b = brief(&server, "task.md", "do it\n");
    let state = server.base.join("state");
    let startup_files: std::collections::BTreeSet<_> = walk(&state).into_iter().collect();
    let out = cli(
        &server,
        &[
            "delegate",
            "start",
            "refused",
            "--harness",
            harness,
            "--sandbox",
            "workspace-write",
            "--brief",
            &b,
            "--worktree",
            "--repo",
            repo.to_str().unwrap(),
            "--branch",
            "test/sandbox-refusal",
        ],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains(&format!("--sandbox is not supported by {harness}")));
    assert!(out.stdout.is_empty());
    assert_eq!(workspaces(&server), before, "no workspace created");
    assert!(
        walk(&server.base.join("wt")).is_empty(),
        "no checkout created"
    );
    // Refusal must add no files beyond the server's startup state.
    let unexpected: Vec<_> = walk(&state)
        .into_iter()
        .filter(|path| !startup_files.contains(path))
        .collect();
    assert!(
        unexpected.is_empty(),
        "sandbox refusal wrote registry or other state: {unexpected:?}"
    );
    assert!(agent_get(&server, "refused").is_none(), "no agent started");
    assert!(
        read_lines(&server.base.join("argv.log")).is_empty(),
        "no harness launched"
    );
    let listing = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(listing.status.success());
    assert_eq!(
        String::from_utf8_lossy(&listing.stdout)
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .count(),
        1,
        "no git checkout registered"
    );
}

#[test]
fn delegate_opencode_sandbox_refusal_creates_nothing() {
    assert_sandbox_refusal_creates_nothing("opencode");
}

#[test]
fn delegate_claude_sandbox_refusal_creates_nothing() {
    assert_sandbox_refusal_creates_nothing("claude");
}

fn assert_recorded_sandbox(server: &Server, sandbox: &str) {
    let status = cli(server, &["delegate", "status", "d1", "--json"]);
    assert!(status.status.success(), "{}", stderr(&status));
    assert_eq!(stdout_json(&status)["sandbox"], sandbox);
    let text = cli(server, &["delegate", "status", "d1"]);
    assert!(text.status.success());
    assert!(String::from_utf8_lossy(&text.stdout).contains(&format!("sandbox {sandbox}")));
    let registry = walk(&server.base.join("state"))
        .into_iter()
        .find(|path| path.file_name().is_some_and(|name| name == "d1.json"))
        .expect("delegate registry entry");
    let entry: serde_json::Value = serde_json::from_slice(&fs::read(registry).unwrap()).unwrap();
    assert_eq!(entry["sandbox"], sandbox);
}

/// Drive the real API and PTY. The child ignores the first Enter and accepts only
/// the guarded retry, leaving the owned composer visible during confirmation.
#[test]
fn guarded_submit_socket_pty_retries_enter_without_retyping() {
    guarded_socket("codex", false, false);
}

#[test]
fn guarded_submit_slow_reader_confirms_with_one_enter_retry() {
    guarded_socket("codex", true, false);
}

#[test]
fn guarded_submit_claude_socket_pty_retries_without_retyping() {
    guarded_socket("claude", false, false);
}

#[test]
fn guarded_submit_opencode_socket_pty_retries_without_retyping() {
    guarded_socket("opencode", false, false);
}

#[test]
fn guarded_submit_first_enter_works_with_late_composer_repaint() {
    guarded_socket("codex", false, true);
}

#[test]
fn guarded_submit_codex_waits_for_footer_after_partial_repaint() {
    guarded_socket_repaint("codex", false, true, 600);
}

#[test]
fn guarded_submit_codex_missing_footer_expires_without_enter() {
    guarded_socket_repaint("codex", false, true, 3000);
}

fn guarded_socket(kind: &str, slow: bool, first_works: bool) {
    guarded_socket_repaint(kind, slow, first_works, 0);
}

fn guarded_socket_repaint(kind: &str, slow: bool, first_works: bool, footer_delay_ms: u64) {
    let server = start_server();
    let script = r#"import os, sys, tty, time
from pathlib import Path
tty.setraw(0)
log = Path(__file__).parent.parent / 'guarded-bytes'
text = b''
enters = 0
paste = False
pending = b''
def draw(working=False):
    body = text.decode()
    chrome = '• Working (1s • esc to interrupt)\r\n' if working else ''
    sys.stdout.write('\x1b[2J\x1b[HOpenAI Codex\r\n' + chrome + '› ' + body + '\r\n  ? for shortcuts\r\n')
    sys.stdout.flush()
sys.stdout.write('\x1b[?2004h')
draw()
if (log.parent / 'slow-reader').exists():
    while not (log.parent / 'idle-detected').exists(): time.sleep(0.01)
    time.sleep(0.35)
while True:
    byte = os.read(0, 1)
    with log.open('ab') as out: out.write(byte)
    if byte == b'\x1b' or pending:
        pending += byte
        if pending == b'\x1b[200~': paste = True; pending = b''
        elif pending == b'\x1b[201~':
            paste = False
            pending = b''
            delay_file = log.parent / 'footer-delay'
            if delay_file.exists():
                sys.stdout.write('\x1b[2J\x1b[HOpenAI Codex\r\n› ' + text.decode() + '\r\n')
                sys.stdout.flush()
                delay = float(delay_file.read_text())
                if delay > 2000:
                    while not (log.parent / 'footer-observed').exists(): time.sleep(0.01)
                    while not (log.parent / 'release-footer').exists(): time.sleep(0.01)
                else:
                    time.sleep(delay / 1000)
            draw()
        continue
    if byte == b'\r' and not paste:
        enters += 1
        if (log.parent / 'first-works').exists() and enters == 1:
            draw(True)
            time.sleep(2.3)
            text = b''
            draw()
        elif enters == 2: draw(True)
    else:
        text += byte
        if not paste: draw()
"#;
    let script = match kind {
        "claude" => script.replace("chrome = '• Working (1s • esc to interrupt)\\r\\n'", "chrome = '✻ Crunching… (esc to interrupt)\\r\\n'")
            .replace("'› ' + body + '\\r\\n  ? for shortcuts\\r\\n'", "'────────────────────────────────────────\\r\\n❯ ' + body + '\\r\\n────────────────────────────────────────\\r\\n  ? for shortcuts\\r\\n'"),
        "opencode" => script.replace("chrome = '• Working (1s • esc to interrupt)\\r\\n'", "chrome = '■■■■⬝⬝ esc interrupt opencode\\r\\n'")
            .replace("'› ' + body + '\\r\\n  ? for shortcuts\\r\\n'", "'┃\\r\\n┃  ' + (body or 'Ask anything…') + '\\r\\n┃\\r\\n┃  Build test-model\\r\\n╹\\r\\ntab agents ctrl+p commands\\r\\n'"),
        _ => script.to_owned(),
    };
    let path = bin_dir(&server.base).join(kind);
    fs::create_dir_all(server.base.join("raw")).unwrap();
    let python = server.base.join("raw").join(kind);
    fs::write(&python, script).unwrap();
    fs::write(
        &path,
        format!("#!/bin/sh\nexec python3 '{}'\n", python.display()),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    if footer_delay_ms > 0 {
        fs::write(
            server.base.join("footer-delay"),
            footer_delay_ms.to_string(),
        )
        .unwrap();
    }
    if slow {
        fs::write(server.base.join("slow-reader"), "").unwrap();
    }
    if first_works {
        fs::write(server.base.join("first-works"), "").unwrap();
    }
    let ws = operator_workspace(&server);
    let started = request(
        &server,
        &serde_json::json!({
            "id":"start", "method":"agent.start", "params":{
                "name":"guarded", "workspace_id":ws, "argv":[kind], "focus":true
            }
        })
        .to_string(),
    );
    assert!(started.get("error").is_none(), "{started}");
    let pane = delegate_pane(&server, "guarded");
    let deadline = Instant::now() + WITHIN;
    loop {
        let agent = agent_get(&server, "guarded").unwrap();
        if matches!(agent["agent_status"].as_str(), Some("idle" | "done")) {
            break;
        }
        assert!(Instant::now() < deadline, "{agent}");
        thread::sleep(Duration::from_millis(30));
    }
    // Capture the isolated bottom-buffer fixture through the public CLI.
    let capture = cli(
        &server,
        &[
            "pane", "read", &pane, "--source", "recent", "--format", "text",
        ],
    );
    assert!(capture.status.success(), "{}", stderr(&capture));
    assert!(stdout(&capture).contains(if kind == "opencode" {
        "commands"
    } else {
        "? for shortcuts"
    }));
    let ansi = cli(
        &server,
        &[
            "pane", "read", &pane, "--source", "recent", "--format", "ansi",
        ],
    );
    assert!(ansi.status.success());
    fs::write(server.base.join("idle-detected"), "").unwrap();
    let submit = serde_json::json!({
        "id":"submit", "method":"agent.send", "params":{"target":pane,"text":"hello","submit":true}
    });
    let response = if footer_delay_ms > 2000 {
        let mut stream = UnixStream::connect(&server.socket).unwrap();
        writeln!(stream, "{submit}").unwrap();
        let deadline = Instant::now() + WITHIN;
        loop {
            let screen = request(
                &server,
                &serde_json::json!({
                    "id":"partial", "method":"pane.read", "params":{
                        "pane_id":pane, "source":"detection", "format":"text"
                    }
                })
                .to_string(),
            );
            let text = screen["result"]["read"]["text"].as_str().unwrap();
            if text.contains("› hello") && !text.contains("? for shortcuts") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "partial frame not observed: {screen}"
            );
            thread::sleep(Duration::from_millis(10));
        }
        fs::write(server.base.join("footer-observed"), "").unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        serde_json::from_str::<serde_json::Value>(&line).unwrap()
    } else {
        request(&server, &submit.to_string())
    };
    if footer_delay_ms > 2000 {
        assert_eq!(response["result"]["outcome"], "unconfirmed", "{response}");
        assert!(
            response["result"]["attempt"]["submit_sent_at_ms"].is_null(),
            "{response}"
        );
        assert_eq!(
            response["result"]["reason"], "owned_composer_not_visible",
            "{response}"
        );
        assert_eq!(
            fs::read(server.base.join("guarded-bytes")).unwrap(),
            b"\x1b[200~hello\x1b[201~"
        );
        fs::write(server.base.join("release-footer"), "").unwrap();
        return;
    }
    assert_eq!(
        response["result"]["outcome"], "observed_accepted",
        "{response}"
    );
    assert_eq!(response["result"]["retried"], !first_works, "{response}");
    if first_works {
        thread::sleep(Duration::from_millis(2400));
    }
    let bytes = fs::read(server.base.join("guarded-bytes")).unwrap();
    assert_eq!(
        bytes,
        if first_works {
            b"\x1b[200~hello\x1b[201~\r".as_slice()
        } else {
            b"\x1b[200~hello\x1b[201~\r\r".as_slice()
        }
    );
    let read = cli(
        &server,
        &[
            "pane", "read", &pane, "--source", "recent", "--format", "text",
        ],
    );
    assert!(
        first_works
            || stdout(&read).contains(if kind == "opencode" {
                "esc interrupt"
            } else {
                "esc to interrupt"
            })
    );
}
