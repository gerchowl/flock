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
         while IFS= read -r line; do printf '%s\\n' \"$line\" >> '{base}/typed.log'; done\n",
        base = base.display()
    );
    let path = bin.join("codex");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn start_server() -> Server {
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
        "idle" => "OpenAI Codex\n› Ask Codex to do anything\n".into(),
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
        vec!["--model m1 --ask-for-approval never --sandbox danger-full-access"]
    );
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
    fs::write(server.base.join("screen"), HOOK_REVIEW).unwrap();
    let b = brief(&server, "task.md", "do it\n");
    let mut child = start_codex(
        &server,
        "blocked",
        &b,
        &["--ready-timeout", "20000", "--json"],
    );
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

#[test]
fn codex_delegate_session_hook_can_arrive_after_submit() {
    let server = start_server();
    operator_workspace(&server);
    let b = brief(&server, "task.md", "do it\n");
    let mut child = start_codex(
        &server,
        "d1",
        &b,
        &["--await", "--timeout", "30000", "--json"],
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
}
