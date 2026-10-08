//! Acceptance tests for #612: `flk delegate --harness claude`.
//!
//! The fixture is `tests/delegate.rs`'s, unchanged in shape — an isolated
//! server, a fake harness on its PATH that records its argv and every line
//! typed into it, a screen the test draws, and a store the test writes the
//! agent's reply into — with the two harness-specific halves swapped for
//! Claude's:
//!
//! - the **session** arrives from the `SessionStart` hook, which the test
//!   plays through `pane.report_agent_session` with flock's reserved
//!   `flock:claude` source, exactly as the installed hook does (#612);
//! - the **reply** is a Claude transcript, a JSONL file under
//!   `$HOME/.claude/projects/<project>/<session>.jsonl`, which is what
//!   `agent.result` reads for a Claude pane (#585, #575).
//!
//! The screens are Claude-shaped because the harness is: the prompt box the
//! detector anchors on, the spinner above it while working, and the first-run
//! folder-trust dialog a fresh worktree always lands on (#605).

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
/// The session id the fake `SessionStart` hook reports.
const SES: &str = "ses_p612";
/// The reserved source flock's Claude hook reports under (`agent_resume`).
const CLAUDE_HOOK_SOURCE: &str = "flock:claude";

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

/// The fake `claude`: records its argv and cwd, dies at once when `<base>/die`
/// exists, redraws its screen whenever `<base>/screen` changes, and appends
/// every line typed into its pane to `<base>/typed.log`. Byte-for-byte the
/// fake in `tests/delegate.rs`, named `claude` — which is all that tells
/// flock's agent identification which detector to run.
fn write_fake_claude(base: &Path) {
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
         while IFS= read -r line; do printf '%s\\n' \"$line\" >> '{base}/typed.log'; printf '\\033[2J\\033[H✻ Crunching… (esc to interrupt)\\n'; sleep 0.4; printf '\\033[2J\\033[H%s\\n' \"$(cat '{base}/screen')\"; done\n",
        base = base.display()
    );
    let path = bin.join("claude");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn start_server() -> Server {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let base = PathBuf::from(format!("/tmp/hdclaude-{}-{nanos}", std::process::id()));
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
    write_fake_claude(&base);

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

/// A delegate verb running in the background, so the test can play the harness
/// while it blocks.
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

/// Collect a background verb's output; one still running is killed first, so a
/// failing test never hangs on it.
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

// -------------------------------------------------------------- Claude UI

/// The prompt box, which is what the detector anchors a Claude pane on: two
/// `─` rules with the `❯` input line between them.
fn claude_prompt_box() -> String {
    format!(
        "{}\n{}\n{}\n",
        "\u{2500}".repeat(48),
        "\u{276f} ",
        "\u{2500}".repeat(48)
    )
}

/// The screen for a status, as Claude Code draws it.
fn screen_for(state: &str) -> String {
    match state {
        "working" => format!(
            "\u{23fa} Reading the brief.\n\u{273b} Crunching\u{2026} (esc to interrupt)\n{}",
            claude_prompt_box()
        ),
        "blocked" => "\u{25b3} Permission required\n".to_string(),
        "idle" => claude_prompt_box(),
        other => panic!("no screen for {other}"),
    }
}

/// The first-run folder-trust dialog, reconstructed from the component Claude
/// Code 2.1.285 and 2.1.289 render: a bordered box titled "Accessing
/// workspace:", the safety-check question, and a confirm widget offering the
/// refusal first with its own chord hints. A `--worktree` checkout is always a
/// folder Claude has no trust record for, so a delegate started this way lands
/// here on every run (#605).
fn trust_dialog(cwd: &str) -> String {
    format!(
        "\u{256d}\u{2500} Accessing workspace: \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256e}\n\
         \u{2502} {cwd}\n\
         \u{2502}\n\
         \u{2502} Quick safety check: Is this a project you created or one you trust?\n\
         \u{2502} Claude Code'll be able to read, edit, and execute files here.\n\
         \u{2502}\n\
         \u{276f} No, exit\n\
         \u{2502}   Yes, I trust this folder\n\
         \u{2502}\n\
         \u{2502} Enter to confirm \u{b7} Esc to cancel\n\
         \u{2570}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256f}\n",
        cwd = cwd
    )
}

/// Draw `state` on the delegate's pane, and wait until flock reports it
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

/// Draw an arbitrary screen and wait until flock reports `blocked`.
fn report_blocked_screen(server: &Server, pane_id: &str, screen: &str) {
    fs::write(server.base.join("screen"), screen).unwrap();
    let deadline = Instant::now() + WITHIN;
    loop {
        let got = request(
            server,
            &format!(r#"{{"id":"ag","method":"agent.get","params":{{"target":"{pane_id}"}}}}"#),
        );
        let status = got["result"]["agent"]["agent_status"]
            .as_str()
            .unwrap_or("");
        if status == "blocked" {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "pane {pane_id} never showed blocked on the drawn screen: {got}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// Play the `SessionStart` hook: the session id reaches flock the way the
/// installed Claude hook delivers it, from inside the harness.
fn report_session(server: &Server, pane_id: &str) {
    let reported = request(
        server,
        &format!(
            r#"{{"id":"ses","method":"pane.report_agent_session","params":{{"pane_id":"{pane_id}","source":"{CLAUDE_HOOK_SOURCE}","agent":"claude","agent_session_id":"{SES}","session_start_source":"startup"}}}}"#
        ),
    );
    assert!(reported.get("error").is_none(), "{reported}");
}

// ------------------------------------------------------- Claude transcript

/// Where `agent_resume::claude_transcript_path` looks: one file named after the
/// session, under any project directory of the pane's `$HOME`. The slug is a
/// fixture (`-work`), derived from this fixture's own directory name.
fn transcript_path(server: &Server) -> PathBuf {
    server
        .base
        .join("home")
        .join(".claude")
        .join("projects")
        .join("-work")
        .join(format!("{SES}.jsonl"))
}

/// RFC 3339 UTC, the shape `agent_transcript` parses for a reply's age.
fn timestamp_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // A fixed civil-from-days conversion: the test only needs a timestamp that
    // parses and reads as now, not an astronomical calendar.
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
        "{year:04}-{:02}-{:02}T{hour:02}:{minute:02}:{second:02}Z",
        month + 1,
        days + 1
    )
}

fn transcript_line(kind: &str, text: &str) -> String {
    let content = if kind == "user" {
        serde_json::Value::String(text.to_string())
    } else {
        serde_json::json!([{"type": "text", "text": text}])
    };
    format!(
        "{}\n",
        serde_json::json!({
            "type": kind,
            "timestamp": timestamp_now(),
            "message": {"role": kind, "content": content},
        })
    )
}

/// Append one user message and one finished assistant reply to the session's
/// transcript: the whole of what `agent.result` reads for a Claude pane.
fn write_reply(server: &Server, turn: &str, text: &str) {
    let path = transcript_path(server);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut body = fs::read_to_string(&path).unwrap_or_default();
    body.push_str(&transcript_line("user", "brief"));
    body.push_str(&transcript_line("assistant", text));
    fs::write(&path, body).unwrap();
    let _ = turn;
}

/// Play one whole turn: working, the reply lands in the transcript, idle.
fn play_turn(server: &Server, pane: &str, text: &str) {
    report(server, pane, "working");
    thread::sleep(Duration::from_millis(100));
    write_reply(server, "t1", text);
    report(server, pane, "idle");
}

/// Play the hook through readiness: the agent exists, names its session, and
/// shows its prompt box.
fn make_ready(server: &Server, name: &str) -> String {
    let pane = delegate_pane(server, name);
    report_session(server, &pane);
    report(server, &pane, "idle");
    pane
}

// ------------------------------------------------------------------ setup

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

fn start_claude(server: &Server, name: &str, brief_path: &str, extra: &[&str]) -> Child {
    let work = work_dir(server);
    let mut args = vec![
        "delegate",
        "start",
        name,
        "--harness",
        "claude",
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

// ------------------------------------------------------------------ tests

/// The whole round, on the Claude path: start in its own workspace, wait for
/// readiness, submit the brief, wait for the turn to settle, and read the reply
/// back out of the transcript the hook's session names.
///
/// It is the opencode `a2` test with two harness-shaped pieces swapped: the
/// session report is the `SessionStart` hook rather than the plugin, and the
/// reply lands in a JSONL transcript rather than a session database. The argv
/// is asserted too, because `--model` reaching Claude is the one thing the
/// table has to get right that a reply cannot show.
#[test]
fn c1_claude_start_await_and_result() {
    let server = start_server();
    let operator = operator_workspace(&server);
    let before = workspaces(&server).len();
    let b = brief(&server, "task.md", "build it\n");
    let mut child = start_claude(
        &server,
        "d1",
        &b,
        &[
            "--model",
            "sonnet",
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
    play_turn(&server, &pane, "Built it.\nDONE: built and pushed");
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
        vec!["--model sonnet"],
        "argv is claude's, and the model is passed through as --model"
    );
    let after = workspaces(&server);
    assert_eq!(
        after.len(),
        before + 1,
        "the delegate got its own workspace"
    );
    assert_ne!(json["workspace_id"], operator.as_str(), "{json}");
    let agent = agent_get(&server, "d1").unwrap();
    assert_eq!(agent["agent"], "claude", "{agent}");
    assert_eq!(agent["workspace_id"], json["workspace_id"], "{agent}");
    assert_eq!(agent["pane_id"], pane.as_str(), "{agent}");
    let delegate_ws = after
        .iter()
        .find(|ws| ws["workspace_id"] == json["workspace_id"])
        .expect("the delegate's workspace is listed");
    assert_eq!(
        delegate_ws["pane_count"], 1,
        "the workspace holds the agent alone: {delegate_ws}"
    );
}

/// The refusal #612 exists to make safe. A fresh worktree is a directory Claude
/// has no trust record for, so `delegate start --worktree --harness claude`
/// lands on the folder-trust dialog every single run. Three things have to hold
/// and none of them is "wait for the timeout": the start fails, it says the
/// dialog by name and points at #605, and the brief is never typed — a sentence
/// at a trust dialog would answer the dialog, not the brief.
#[test]
fn c2_claude_trust_dialog_is_refused_and_nothing_is_typed() {
    let server = start_server();
    operator_workspace(&server);
    let repo = committed_repo(&server);
    let repo_s = repo.to_string_lossy().into_owned();
    let b = brief(&server, "task.md", "x\n");
    let before = workspaces(&server).len();
    let checkout = server.base.join("wt").join("claude-trust-checkout");
    fs::create_dir_all(checkout.parent().unwrap()).unwrap();

    let mut child = cli_spawn(
        &server,
        &[
            "delegate",
            "start",
            "w1",
            "--harness",
            "claude",
            "--brief",
            &b,
            "--worktree",
            "--repo",
            &repo_s,
            "--branch",
            "feat/p612-trust",
            // Generous, so a refusal that waits this long for the timeout is
            // a failure rather than a fast path that happens to look right.
            "--ready-timeout",
            "20000",
            "--json",
        ],
    );
    let pane = delegate_pane(&server, "w1");
    report_blocked_screen(
        &server,
        &pane,
        &trust_dialog(&checkout.display().to_string()),
    );

    let status = exited_within(&mut child, WITHIN).expect("the start refuses");
    let out = finish(child);
    assert_eq!(status.code(), Some(1), "stderr {}", stderr(&out));
    let message = stderr(&out);
    assert!(
        message.contains("folder-trust dialog") && message.contains("#605"),
        "the refusal names the dialog and the issue: {message}"
    );
    assert!(
        message.contains("delegate w1:"),
        "and names the delegate: {message}"
    );
    assert!(
        stdout(&out).is_empty(),
        "a failure prints nothing on stdout: {:?}",
        stdout(&out)
    );
    assert!(
        typed(&server).is_empty(),
        "nothing was typed at the dialog: {:?}",
        typed(&server)
    );

    // A refused start is not a start: the workspace is rolled back and no
    // registry entry survives it.
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
        walk(&server.base.join("wt")).is_empty(),
        "and so is the checkout: {:?}",
        walk(&server.base.join("wt"))
    );
    assert!(
        !walk(&server.base.join("state"))
            .iter()
            .any(|p| p.ends_with("w1.json")),
        "no registry entry"
    );
}

/// The window a Claude turn really has: the pane is at its prompt and the brief
/// is submitted BEFORE the `SessionStart` hook has reported, so the first
/// `agent.result` refuses `no_agent_session`.
///
/// That refusal is "not yet" for a harness whose session arrives
/// asynchronously, so the await keeps polling and the round is still reported.
/// Read as a failure instead, every Claude delegate whose hook report lags the
/// first poll would end `no_result` at exit 5 — the one outcome that says the
/// agent did nothing.
#[test]
fn c3_claude_not_yet_before_the_session_hook_reports() {
    let server = start_server();
    operator_workspace(&server);
    let b = brief(&server, "task.md", "x\n");
    let mut child = start_claude(
        &server,
        "d1",
        &b,
        &["--await", "--timeout", "30000", "--json"],
    );
    let pane = delegate_pane(&server, "d1");
    // Ready, but no session: the hook has not reported yet.
    report(&server, &pane, "idle");
    wait_typed(&server, 1);
    let probe = cli(&server, &["agent", "result", &pane]);
    assert_eq!(probe.status.code(), Some(1), "{}", stderr(&probe));
    assert!(
        stderr(&probe).contains("no_agent_session"),
        "the refusal really is the no-session one: {}",
        stderr(&probe)
    );

    report(&server, &pane, "working");
    thread::sleep(Duration::from_millis(4 * SETTLE_MS));
    assert!(
        child.try_wait().unwrap().is_none(),
        "a missing session is not yet, not an error"
    );
    report_session(&server, &pane);
    write_reply(&server, "t1", "DONE: after the hook reported");
    report(&server, &pane, "idle");
    let status = exited_within(&mut child, WITHIN).expect("the await returns");
    let out = finish(child);
    assert_eq!(status.code(), Some(0), "stderr {}", stderr(&out));
    let json = stdout_json(&out);
    assert_eq!(json["outcome"], "done", "{json}");
    assert_eq!(json["status_text"], "after the hook reported", "{json}");
}

/// `--harness` is the only thing separating the two paths, so a typo is still
/// a usage error, refused before anything is created — and the refusal names
/// the harnesses this build does drive.
#[test]
fn c4_an_unsupported_harness_is_a_usage_error() {
    let server = start_server();
    operator_workspace(&server);
    let before = workspaces(&server).len();
    let work = work_dir(&server);
    let b = brief(&server, "task.md", "x\n");
    let out = cli(
        &server,
        &[
            "delegate",
            "start",
            "u1",
            "--brief",
            &b,
            "--cwd",
            &work,
            "--harness",
            "aider",
        ],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("not supported") && stderr(&out).contains("opencode|claude|codex"),
        "the refusal says what this build drives: {}",
        stderr(&out)
    );
    assert!(stdout(&out).is_empty());
    assert_eq!(workspaces(&server).len(), before, "nothing was created");
    assert!(agent_get(&server, "u1").is_none(), "no agent was started");
    assert!(
        read_lines(&server.base.join("argv.log")).is_empty(),
        "no harness was launched"
    );
}

#[test]
fn guarded_delegate_start_rolls_back_when_composer_refuses_before_typing() {
    let server = start_server();
    operator_workspace(&server);
    let before = workspaces(&server).len();
    let b = brief(&server, "refused.md", "brief\n");
    let mut child = start_claude(&server, "refused", &b, &["--json"]);
    let pane = delegate_pane(&server, "refused");
    report_session(&server, &pane);
    fs::write(
        server.base.join("screen"),
        claude_prompt_box().replace("❯ ", "❯ operator draft"),
    )
    .unwrap();
    let status = exited_within(&mut child, WITHIN).expect("refusal returns");
    let out = finish(child);
    assert_eq!(status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("input_not_empty"), "{}", stderr(&out));
    assert!(!stderr(&out).contains("Workspace kept"));
    assert_eq!(workspaces(&server).len(), before);
    assert!(agent_get(&server, "refused").is_none());
    assert!(typed(&server).is_empty());
    let list = cli(&server, &["delegate", "list", "--json"]);
    assert!(!stdout(&list).contains("refused"), "{}", stdout(&list));
}
