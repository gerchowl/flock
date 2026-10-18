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
    support::write_fake_gh(&bin_dir(&base));

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
        // Callers already observed registration. A missing agent here has
        // disappeared, so waiting for another status cannot make progress.
        if got.get("error").is_some() {
            let agents = request(server, r#"{"id":"al","method":"agent.list","params":{}}"#);
            panic!("pane {pane_id} cannot show {state}: {got}; agent.list: {agents}");
        }
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
    delegate_pane(&server, "w1");
    // Refusal rolls the pane back as soon as the dialog is detected. Wait for
    // the command's outcome, since the blocked state may already be gone.
    fs::write(
        server.base.join("screen"),
        trust_dialog(&checkout.display().to_string()),
    )
    .unwrap();

    let status = exited_within(&mut child, WITHIN);
    let out = finish(child);
    assert!(
        status.is_some(),
        "the start did not refuse: {}; agent.list: {}",
        stderr(&out),
        request(&server, r#"{"id":"al","method":"agent.list","params":{}}"#)
    );
    let status = status.unwrap();
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
    // #938: under `--json` the one thing on stdout is the failure, with the
    // dialog the agent was showing.
    let json = stdout_json(&out);
    assert_eq!(json["outcome"], "start_failed", "{json}");
    assert_eq!(json["kept"], false, "{json}");
    assert!(
        json["data"]["pane_tail"]
            .as_array()
            .is_some_and(|tail| tail.iter().any(|line| line
                .as_str()
                .is_some_and(|l| l.contains("Yes, I trust this folder")))),
        "{json}"
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
    // Rolled back with nothing typed: a refusal, 10 since #1025 (was 1).
    assert_eq!(status.code(), Some(10), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("note: exit code is now 10 (was 1); see CHANGELOG"),
        "{}",
        stderr(&out)
    );
    assert!(stderr(&out).contains("input_not_empty"), "{}", stderr(&out));
    assert!(!stderr(&out).contains("Workspace kept"));
    // #925: the pane is gone after the rollback, so the error carries what it
    // showed, read before the teardown.
    assert!(
        stderr(&out).contains("delegate refused: last ")
            && stderr(&out).contains("  | \u{276f} operator draft"),
        "the error carries the pane's last lines: {}",
        stderr(&out)
    );
    assert_eq!(workspaces(&server).len(), before);
    assert!(agent_get(&server, "refused").is_none());
    assert!(typed(&server).is_empty());
    let list = cli(&server, &["delegate", "list", "--json"]);
    assert!(!stdout(&list).contains("refused"), "{}", stdout(&list));
}

/// #1025: a `send` the composer refuses before typing restores the registry
/// and exits 10, the refusal code, naming the old 1 on stderr.
#[test]
fn delegate_send_refused_before_typing_exits_10() {
    let server = start_server();
    operator_workspace(&server);
    let b = brief(&server, "task.md", "x\n");
    let mut child = start_claude(&server, "s1", &b, &["--json"]);
    let pane = make_ready(&server, "s1");
    let status = exited_within(&mut child, WITHIN).expect("start returns");
    let out = finish(child);
    assert_eq!(status.code(), Some(0), "stderr {}", stderr(&out));
    let typed_before = wait_typed(&server, 1).len();
    // `report` paints its own screen, so the draft goes on after it.
    report(&server, &pane, "idle");
    fs::write(
        server.base.join("screen"),
        claude_prompt_box().replace("❯ ", "❯ operator draft"),
    )
    .unwrap();
    let deadline = Instant::now() + WITHIN;
    while !stdout(&cli(&server, &["pane", "read", &pane])).contains("operator draft") {
        assert!(Instant::now() < deadline, "the draft was never painted");
        thread::sleep(Duration::from_millis(30));
    }
    let agent_refusal = cli(
        &server,
        &["agent", "send", "--json", "--no-submit", "s1", "extra"],
    );
    assert_eq!(
        agent_refusal.status.code(),
        Some(10),
        "{}",
        stderr(&agent_refusal)
    );
    let refusal = stdout_json(&agent_refusal);
    assert_eq!(refusal["error"]["code"], "input_not_empty");
    assert_eq!(
        refusal["error"]["data"]["composer"]["visible_bytes"],
        "operator draft".len()
    );
    assert!(stderr(&agent_refusal).contains("flk delegate send s1 --brief FILE"));
    assert!(stderr(&agent_refusal).contains("also refuses drafts"));
    assert!(!stdout(&agent_refusal).contains("operator draft"));
    assert_eq!(
        typed(&server).len(),
        typed_before,
        "no-submit refusal typed nothing"
    );
    let b2 = brief(&server, "next.md", "y\n");
    let sent = cli(&server, &["delegate", "send", "s1", "--brief", &b2]);
    let message = stderr(&sent);
    assert_eq!(sent.status.code(), Some(10), "{message}");
    assert!(message.contains("input_not_empty"), "{message}");
    assert!(message.contains("agent UI"), "{message}");
    assert!(
        message.contains("note: exit code is now 10 (was 1); see CHANGELOG"),
        "{message}"
    );
    assert_eq!(typed(&server).len(), typed_before, "nothing was typed");
    // The registry went back to the round that ran.
    let status = cli(&server, &["delegate", "status", "s1", "--json"]);
    assert_eq!(stdout_json(&status)["round"], 1, "{}", stderr(&status));
}

#[test]
fn report_a_removed_agent_fails_with_the_agent_list() {
    let server = start_server();
    operator_workspace(&server);
    let b = brief(&server, "task.md", "x\n");
    let mut child = start_claude(&server, "d1", &b, &["--json"]);
    let pane = make_ready(&server, "d1");
    let status = exited_within(&mut child, WITHIN).expect("start returns");
    let out = finish(child);
    assert_eq!(status.code(), Some(0), "stderr {}", stderr(&out));
    let started = stdout_json(&out);
    let ws = started["workspace_id"].as_str().unwrap();
    let closed = request(
        &server,
        &format!(r#"{{"id":"wc","method":"workspace.close","params":{{"workspace_id":"{ws}"}}}}"#),
    );
    assert!(closed.get("error").is_none(), "workspace.close: {closed}");

    let began = Instant::now();
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        report(&server, &pane, "idle");
    }))
    .expect_err("a removed agent cannot become idle");
    let message = failure.downcast_ref::<String>().expect("panic diagnostic");
    assert!(message.contains("agent_not_found"), "{message}");
    assert!(message.contains("agent.list:"), "{message}");
    assert!(
        began.elapsed() < WITHIN,
        "must fail before the status deadline"
    );
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", stderr(&out));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// #925 (c): with `--keep-on-failure` a start the composer refuses keeps its
/// workspace, agent and registry entry, prints the pane's last lines, and
/// `reap` removes what was kept.
#[test]
fn keep_on_failure_keeps_a_refused_start_for_inspection() {
    let server = start_server();
    operator_workspace(&server);
    let before = workspaces(&server).len();
    let b = brief(&server, "kept.md", "brief\n");
    let mut child = start_claude(&server, "kept", &b, &["--keep-on-failure", "--json"]);
    let pane = delegate_pane(&server, "kept");
    report_session(&server, &pane);
    fs::write(
        server.base.join("screen"),
        claude_prompt_box().replace("❯ ", "❯ leftover suggestion"),
    )
    .unwrap();
    let status = exited_within(&mut child, WITHIN).expect("refusal returns");
    let out = finish(child);
    let message = stderr(&out);
    // A kept start persists, so it stays a failure (#1025).
    assert_eq!(status.code(), Some(1), "{message}");
    assert!(!message.contains("note: exit code"), "{message}");
    assert!(message.contains("input_not_empty"), "{message}");
    assert!(
        message.contains("  | \u{276f} leftover suggestion"),
        "the pane's last lines: {message}"
    );
    assert!(
        message.contains("kept for inspection (--keep-on-failure)")
            && message.contains("flk delegate reap kept"),
        "{message}"
    );
    // #938: `--json` carries the same lines, structured.
    let json = stdout_json(&out);
    assert_eq!(json["outcome"], "start_failed", "{json}");
    assert_eq!(json["kept"], true, "{json}");
    let tail = json["data"]["pane_tail"].as_array().expect("pane_tail");
    assert!(
        tail.iter()
            .any(|line| line == "\u{276f} leftover suggestion"),
        "{json}"
    );
    assert!(typed(&server).is_empty(), "nothing was typed");
    assert_eq!(workspaces(&server).len(), before + 1, "the workspace stays");
    assert!(agent_get(&server, "kept").is_some(), "the agent stays");

    let status = cli(&server, &["delegate", "status", "kept", "--json"]);
    assert_eq!(status.status.code(), Some(0), "{}", stderr(&status));
    let json = stdout_json(&status);
    assert_eq!(json["round"], 0, "no round ran: {json}");

    let reaped = cli(&server, &["delegate", "reap", "kept", "--json"]);
    assert_eq!(reaped.status.code(), Some(0), "{}", stderr(&reaped));
    assert_eq!(workspaces(&server).len(), before, "reap removes it");
    let after = cli(&server, &["delegate", "status", "kept"]);
    assert_eq!(after.status.code(), Some(2), "{}", stderr(&after));
}

/// #925 (b): a `--repo` that is a linked worktree starts from its main
/// checkout, branching from the linked worktree's HEAD, instead of failing
/// with `linked_worktree_source`.
#[test]
fn a_linked_worktree_repo_starts_from_its_main_checkout() {
    let server = start_server();
    operator_workspace(&server);
    let repo = committed_repo(&server);
    let linked = server.base.join("story-wt");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "story",
            &linked.to_string_lossy(),
        ],
    );
    git(
        &linked,
        &["commit", "-q", "--allow-empty", "-m", "story work"],
    );
    let story_head = git_out(&linked, &["rev-parse", "HEAD"]);
    assert_ne!(story_head, git_out(&repo, &["rev-parse", "HEAD"]));
    let b = brief(&server, "task.md", "x\n");

    let mut child = cli_spawn(
        &server,
        &[
            "delegate",
            "start",
            "lw",
            "--harness",
            "claude",
            "--brief",
            &b,
            "--worktree",
            "--repo",
            &linked.to_string_lossy(),
            "--branch",
            "feat/p925-linked",
            "--json",
        ],
    );
    make_ready(&server, "lw");
    let status = exited_within(&mut child, WITHIN).expect("start returns");
    let out = finish(child);
    let message = stderr(&out);
    assert_eq!(status.code(), Some(0), "{message}");
    assert!(
        message.contains("is a linked worktree; starting from its main checkout")
            && message.contains(&story_head[..12]),
        "the swap is said, with the commit it branches from: {message}"
    );
    let json = stdout_json(&out);
    let checkout = PathBuf::from(json["worktree"].as_str().expect("a checkout"));
    assert_eq!(git_out(&checkout, &["rev-parse", "HEAD"]), story_head);
    assert_eq!(
        git_out(&checkout, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "feat/p925-linked"
    );
    assert_eq!(json["base_sha"], story_head.as_str(), "{json}");
}

/// #925 (a): `--repo .` is the caller's directory. Inside a repository the
/// start succeeds from it; outside one it fails non-zero with the server's
/// reason, and nothing is created.
#[test]
fn a_relative_repo_resolves_against_the_caller_and_never_exits_zero_on_error() {
    let server = start_server();
    operator_workspace(&server);
    let repo = committed_repo(&server);
    let b = brief(&server, "task.md", "x\n");
    let before = workspaces(&server).len();
    let args = |name: &'static str, branch: &'static str| {
        [
            "delegate",
            "start",
            name,
            "--harness",
            "claude",
            "--brief",
            &b,
            "--worktree",
            "--repo",
            ".",
            "--branch",
            branch,
            "--json",
        ]
        .map(str::to_string)
    };

    let outside = command(&server, &[])
        .args(args("rel-out", "feat/p925-out"))
        .current_dir(server.base.join("work"))
        .output()
        .unwrap();
    assert_eq!(outside.status.code(), Some(1), "{}", stderr(&outside));
    assert!(
        stderr(&outside).contains("not_git_worktree")
            && !stderr(&outside).contains("must be an absolute path"),
        "the path reached the server absolute: {}",
        stderr(&outside)
    );
    assert!(stdout(&outside).is_empty());
    assert_eq!(workspaces(&server).len(), before, "nothing was created");

    let mut inside = command(&server, &[])
        .args(args("rel-in", "feat/p925-in"))
        .current_dir(&repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    make_ready(&server, "rel-in");
    let status = exited_within(&mut inside, WITHIN).expect("start returns");
    let out = finish(inside);
    assert_eq!(status.code(), Some(0), "{}", stderr(&out));
    let json = stdout_json(&out);
    let checkout = PathBuf::from(json["worktree"].as_str().expect("a checkout"));
    assert_eq!(
        git_out(&checkout, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "feat/p925-in"
    );
}

/// #925 (b): a linked worktree of a bare repository has no main checkout to
/// start from, so it is refused as a usage error (exit 2) before anything is
/// created, naming the fix and never announcing a swap.
#[test]
fn a_linked_worktree_of_a_bare_repo_is_refused() {
    let server = start_server();
    operator_workspace(&server);
    let repo = committed_repo(&server);
    let bare = server.base.join("bare.git");
    git(
        &server.base,
        &[
            "clone",
            "-q",
            "--bare",
            &repo.to_string_lossy(),
            &bare.to_string_lossy(),
        ],
    );
    let linked = server.base.join("bare-wt");
    git(
        &bare,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "story",
            &linked.to_string_lossy(),
            "main",
        ],
    );
    let b = brief(&server, "task.md", "x\n");
    let before = workspaces(&server).len();
    let out = cli(
        &server,
        &[
            "delegate",
            "start",
            "bw",
            "--harness",
            "claude",
            "--brief",
            &b,
            "--worktree",
            "--repo",
            &linked.to_string_lossy(),
            "--branch",
            "feat/p925-bare",
        ],
    );
    let message = stderr(&out);
    assert_eq!(out.status.code(), Some(2), "{message}");
    assert!(
        message.contains("of a bare repository") && message.contains("pass a main checkout"),
        "{message}"
    );
    assert!(
        !message.contains("starting from its main checkout"),
        "{message}"
    );
    assert!(stdout(&out).is_empty());
    assert_eq!(workspaces(&server).len(), before, "nothing was created");
    assert!(agent_get(&server, "bw").is_none());
}

/// #925 (c): `--keep-on-failure` on the readiness path. An agent that never
/// reaches its prompt times out, and the start keeps its workspace, agent and
/// record (round 0) instead of rolling back; `reap` removes them.
#[test]
fn keep_on_failure_keeps_a_start_whose_readiness_timed_out() {
    let server = start_server();
    operator_workspace(&server);
    let before = workspaces(&server).len();
    let b = brief(&server, "slow.md", "brief\n");
    let mut child = start_claude(
        &server,
        "slow",
        &b,
        &["--keep-on-failure", "--ready-timeout", "1500"],
    );
    delegate_pane(&server, "slow");
    fs::write(server.base.join("screen"), "booting the harness\n").unwrap();
    let status = exited_within(&mut child, WITHIN).expect("the timeout returns");
    let out = finish(child);
    let message = stderr(&out);
    assert_eq!(status.code(), Some(1), "{message}");
    assert!(message.contains("ready timeout"), "{message}");
    assert!(
        message.contains("kept for inspection (--keep-on-failure)"),
        "{message}"
    );
    assert!(typed(&server).is_empty(), "nothing was typed");
    assert_eq!(workspaces(&server).len(), before + 1, "the workspace stays");
    assert!(agent_get(&server, "slow").is_some(), "the agent stays");

    let status = cli(&server, &["delegate", "status", "slow", "--json"]);
    assert_eq!(status.status.code(), Some(0), "{}", stderr(&status));
    assert_eq!(stdout_json(&status)["round"], 0);

    let reaped = cli(&server, &["delegate", "reap", "slow", "--json"]);
    assert_eq!(reaped.status.code(), Some(0), "{}", stderr(&reaped));
    assert_eq!(workspaces(&server).len(), before, "reap removes it");
}

#[test]
fn permission_trust_start_keeps_the_named_block_for_status_and_wait() {
    assert_permission_trust_start(
        include_str!("fixtures/attention-1023/claude-trust-with-permissions.txt"),
        RetainedBlockCase::Observe,
    );
}

#[test]
fn gated_permission_trust_start_keeps_the_named_block_for_status_and_wait() {
    assert_permission_trust_start(
        include_str!("fixtures/attention-1023/claude-trust-gated-grants.txt"),
        RetainedBlockCase::Observe,
    );
}

#[test]
fn directory_permission_trust_start_keeps_the_named_block_for_status_and_wait() {
    let screen = include_str!("fixtures/attention-1023/claude-trust-gated-grants.txt")
        .replace("Accessing workspace:", "Now in a new directory:")
        .replace(
            "Quick safety check: Is this a project you created or one you trust?",
            "This session hasn't worked here before. Is this a directory you created or one you trust?",
        )
        .replace("Yes, I trust this folder", "Yes, trust it and apply them");
    assert_permission_trust_start(&screen, RetainedBlockCase::Observe);
}

#[derive(Clone, Copy)]
enum RetainedBlockCase {
    Observe,
    AwaitAndSend,
    RegistryFailure(bool),
}

#[test]
fn permission_trust_start_await_names_block_and_send_after_operator_answer_works() {
    assert_permission_trust_start(
        include_str!("fixtures/attention-1023/claude-trust-with-permissions.txt"),
        RetainedBlockCase::AwaitAndSend,
    );
}

#[test]
fn permission_trust_registry_failure_is_typed_and_retains_pane_without_partial_entry() {
    for keep_on_failure in [false, true] {
        assert_permission_trust_start(
            include_str!("fixtures/attention-1023/claude-trust-with-permissions.txt"),
            RetainedBlockCase::RegistryFailure(keep_on_failure),
        );
    }
}

fn assert_permission_trust_start(screen: &str, case: RetainedBlockCase) {
    let server = start_server();
    operator_workspace(&server);
    let repo = committed_repo(&server);
    let b = brief(&server, "permissions.md", "Do not approve permissions.\n");
    let mut args = vec![
        "delegate",
        "start",
        "permissions",
        "--harness",
        "claude",
        "--brief",
        &b,
        "--worktree",
        "--repo",
        repo.to_str().unwrap(),
        "--branch",
        "fixture/permissions",
        "--ready-timeout",
        "20000",
        "--json",
    ];
    if matches!(case, RetainedBlockCase::AwaitAndSend) {
        args.push("--await");
    }
    if matches!(case, RetainedBlockCase::RegistryFailure(true)) {
        args.push("--keep-on-failure");
    }
    let mut child = cli_spawn(&server, &args);
    let pane = delegate_pane(&server, "permissions");
    let refused_entry = if matches!(case, RetainedBlockCase::RegistryFailure(_)) {
        let lock = walk(&server.base.join("state"))
            .into_iter()
            .find(|path| path.ends_with("permissions.lock"))
            .expect("start holds its registry lock");
        let path = lock.with_extension("json");
        fs::create_dir(&path).unwrap();
        Some(path)
    } else {
        None
    };
    fs::write(server.base.join("screen"), screen).unwrap();
    let deadline = Instant::now() + WITHIN;
    let captured = loop {
        let out = cli(
            &server,
            &[
                "pane", "read", &pane, "--source", "recent", "--format", "text",
            ],
        );
        let screen = String::from_utf8_lossy(&out.stdout).into_owned();
        if screen.contains("pre-approves") {
            break screen;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "delegate exited before painting: {screen}"
        );
        assert!(Instant::now() < deadline, "fixture did not paint: {screen}");
        thread::yield_now();
    };
    let status = exited_within(&mut child, WITHIN);
    let out = finish(child);
    if let Some(path) = refused_entry {
        assert_eq!(status.and_then(|s| s.code()), Some(1), "{}", stderr(&out));
        let error = stdout_json(&out);
        assert_eq!(error["error"]["code"], "write_failed", "{error}");
        assert_eq!(error["error"]["data"]["pane_id"], pane);
        assert_eq!(error["error"]["data"]["pane_retained"], true);
        assert!(error["error"]["message"].as_str().unwrap().contains(&pane));
        assert!(path.is_dir(), "the pre-existing destination is untouched");
        assert!(!path
            .parent()
            .unwrap()
            .join(".permissions.json.tmp")
            .exists());
        assert_eq!(agent_get(&server, "permissions").unwrap()["pane_id"], pane);
        assert!(typed(&server).is_empty());
        return;
    }
    assert_eq!(
        status.and_then(|s| s.code()),
        Some(6),
        "{}\ncaptured recent pane:\n{captured}",
        stderr(&out)
    );
    let start = stdout_json(&out);
    assert_eq!(start["outcome"], "agent_blocked");
    assert_eq!(
        start["startup_dialog"],
        "claude_folder_trust_with_permissions"
    );
    assert_eq!(start["round"], 0);
    assert!(Path::new(start["worktree"].as_str().unwrap()).exists());
    assert_eq!(agent_get(&server, "permissions").unwrap()["pane_id"], pane);
    let status = cli(&server, &["delegate", "status", "permissions", "--json"]);
    assert!(status.status.success(), "{}", stderr(&status));
    assert_eq!(
        stdout_json(&status)["startup_dialog"],
        "claude_folder_trust_with_permissions"
    );
    let waited = cli(
        &server,
        &[
            "delegate",
            "wait",
            "permissions",
            "--settle",
            SETTLE,
            "--timeout",
            "5000",
            "--json",
        ],
    );
    assert_eq!(waited.status.code(), Some(6), "{}", stderr(&waited));
    assert_eq!(
        stdout_json(&waited)["startup_dialog"],
        "claude_folder_trust_with_permissions"
    );
    assert!(typed(&server).is_empty(), "security dialog received input");
    if matches!(case, RetainedBlockCase::AwaitAndSend) {
        // The operator answers the dialog through the real pane, then the
        // fixture paints the prompt and reports the real SessionStart hook.
        let answered = cli(&server, &["pane", "send-keys", &pane, "Down", "Enter"]);
        assert!(answered.status.success(), "{}", stderr(&answered));
        assert_eq!(wait_typed(&server, 1), vec!["\u{1b}[B".to_string()]);
        report_session(&server, &pane);
        report(&server, &pane, "idle");
        // SessionStart may publish idle before the renderer paints its prompt.
        // Gate on the live screen too, so send cannot race that producer.
        let deadline = Instant::now() + WITHIN;
        loop {
            let shown = request(
                &server,
                &serde_json::json!({
                    "id": "operator-ready", "method": "pane.read",
                    "params": {"pane_id": pane, "source": "detection", "format": "text"},
                })
                .to_string(),
            );
            let text = shown["result"]["read"]["text"].as_str().unwrap_or("");
            if text.contains(&"─".repeat(48))
                && text.lines().any(|line| line.trim() == "❯")
                && !text.contains("pre-approves")
                && !text.contains("Crunching")
            {
                break;
            }
            assert!(
                agent_get(&server, "permissions").is_some(),
                "fixture exited: {shown}"
            );
            assert!(
                Instant::now() < deadline,
                "operator answer never reached the prompt: {shown}"
            );
            thread::yield_now();
        }
        // The existing operator-input guard still applies after a manual key.
        // Observe the fixture's normal quiet condition before the one send.
        let settled = cli(
            &server,
            &[
                "agent",
                "wait",
                &pane,
                "--status",
                "settled",
                "--settle",
                SETTLE,
                "--timeout",
                "5000",
                "--json",
            ],
        );
        assert!(settled.status.success(), "{}", stderr(&settled));
        let next = brief(&server, "after-trust.md", "Now run the task.\n");
        let mut sent = cli_spawn(
            &server,
            &[
                "delegate",
                "send",
                "permissions",
                "--brief",
                &next,
                "--ready-timeout",
                "20000",
                "--json",
            ],
        );
        let deadline = Instant::now() + WITHIN;
        let lines = loop {
            let lines = typed(&server);
            if lines.len() >= 2 {
                break lines;
            }
            if let Some(status) = sent.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "send exited before input: {}",
                    stderr(&finish(sent))
                );
            }
            assert!(
                agent_get(&server, "permissions").is_some(),
                "fixture exited before receiving input"
            );
            assert!(
                Instant::now() < deadline,
                "send never reached the live fixture: {lines:?}"
            );
            thread::yield_now();
        };
        assert_eq!(lines, vec!["\u{1b}[B".to_string(), expected_line(&next)]);
        assert!(
            exited_within(&mut sent, WITHIN).unwrap().success(),
            "{}",
            stderr(&finish(sent))
        );
        let status = stdout_json(&cli(
            &server,
            &["delegate", "status", "permissions", "--json"],
        ));
        assert_eq!(status["round"], 1);
        assert!(status["startup_dialog"].is_null(), "{status}");
    }
}
