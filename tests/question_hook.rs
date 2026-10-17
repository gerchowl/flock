//! #1180 (ADR-0034 §3, §4): the Claude question hook, from the installed stub
//! to the verdict Claude reads on stdout.
//!
//! Two halves. The fail-closed path runs the stub with no server, or with a
//! server that cannot decide, and with a missing, crashing, garbled, silent or
//! hanging `flk`: a hinted delegate is refused with its fallback sentence, and
//! a root passes within the 500 ms of every other hook. The server half starts
//! a real isolated flk and runs the stub inside a root pane and inside panes
//! that root started, so `hook.decide` attests each one by genuine process
//! ancestry and reads the policy #1179 put on its record.

// The fixture drives the compiled CLI and the stub with raw Command, as the
// other hook tests do.
#![allow(clippy::disallowed_methods)]
mod support;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use support::environment::Command;
use support::socket_dir::SocketDir;
use support::{
    cleanup_test_base, register_runtime_dir, register_spawned_flock_pid,
    unregister_spawned_flock_pid, wait_for_socket,
};

const STUB: &str = "src/integration/assets/claude/flock-agent-state.sh";
const ASK: &str = r#"{"hook_event_name":"PreToolUse","session_id":"s1","tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"Which color?","header":"Color","options":[{"label":"red","description":"r"},{"label":"blue","description":"b"}],"multiSelect":false}]}}"#;

const ROUTE: &str = "flk could not reach your architect. Do not ask the human. Choose the most conservative option, state the assumption in your result, or end your turn with a blocked report.";
const ASK_OPERATOR: &str = "flk could not reach the operator's question queue, and nobody is watching this pane. Choose the most conservative option, state the assumption in your result, or end your turn with a blocked report.";
const DENY: &str = "Questions are disabled for this agent. Choose the most conservative option, state the assumption in your result, or end your turn with a blocked report.";
const UNROUTED: &str = "state the question and options in your final message and end your turn";

fn unique_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = PathBuf::from(format!("/tmp/hq{tag}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn stub_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(STUB)
}

/// What Claude reads from the hook: the deny reason, `Some("")` for a pass
/// (`{}`), `None` for no output at all.
fn reason(stdout: &str) -> Option<String> {
    let stdout = stdout.trim();
    if stdout.is_empty() {
        return None;
    }
    let out: serde_json::Value = serde_json::from_str(stdout)
        .unwrap_or_else(|err| panic!("hook stdout is not JSON ({err}): {stdout:?}"));
    if out == serde_json::json!({}) {
        return Some(String::new());
    }
    assert_eq!(out["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "deny");
    Some(
        out["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap_or_else(|| panic!("no reason: {out}"))
            .to_string(),
    )
}

/// Run the stub's question action the way Claude does, outside any server.
fn run_stub(socket: &Path, bin: &Path, hint: Option<&str>) -> (Option<String>, Duration) {
    let (stdout, elapsed) = run_stub_raw(socket, bin, hint);
    (reason(&stdout), elapsed)
}

/// support::environment's sandbox for the flk the stub runs. The stub reaches
/// flk through `FLOCK_BIN`, not its argv, so the wrapper cannot see that it
/// is a flk launch and the test hands the isolated environment over itself.
fn isolation(socket: &Path) -> Vec<(&'static str, PathBuf)> {
    let root = socket.parent().expect("socket dir").join("iso");
    support::environment::isolated_env(&root, &root.join("run"))
}

fn start_stub(socket: &Path, bin: &Path, hint: Option<&str>) -> FixtureChild {
    start_stub_with_events(socket, bin, hint, None)
}

fn start_stub_with_events(
    socket: &Path,
    bin: &Path,
    hint: Option<&str>,
    events: Option<UnixStream>,
) -> FixtureChild {
    let mut command = Command::new("/bin/sh");
    if events.is_some() {
        command.arg("-x");
    }
    command
        .arg(stub_path())
        .arg("question")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("FLOCK_ENV", "1")
        .env("FLOCK_PANE_ID", "p_test")
        .env("FLOCK_SOCKET_PATH", socket)
        .env("FLOCK_BIN", bin)
        // The flk the stub starts inherits these: a sandbox HOME and XDG
        // tree, never the test runner's.
        .envs(isolation(socket))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(hint) = hint {
        command.env("FLOCK_QUESTIONS", hint);
    }
    let mut child = FixtureChild::spawn_with_events(command, events);
    child
        .child()
        .stdin
        .take()
        .unwrap()
        .write_all(ASK.as_bytes())
        .unwrap();
    child
}

fn run_stub_raw(socket: &Path, bin: &Path, hint: Option<&str>) -> (String, Duration) {
    start_stub(socket, bin, hint).finish()
}

/// Own a new process group so failed fixture stages cannot leave descendants.
struct FixtureChild {
    child: Option<Child>,
    group: libc::pid_t,
    started: Instant,
}

impl FixtureChild {
    fn spawn_with_events(mut command: Command, events: Option<UnixStream>) -> Self {
        if let Some(events) = &events {
            // POSIX shells support these redirection descriptors portably.
            assert!(
                (3..=9).contains(&events.as_raw_fd()),
                "fixture needs an inherited fd in 3..9"
            );
            command.env("FLOCK_TEST_EVENT_FD", events.as_raw_fd().to_string());
        }
        // SAFETY: only async-signal-safe syscalls run after fork. The stream is
        // owned and open in the parent before spawn allocates its exec-error
        // pipe, so their descriptors cannot collide. No descriptor is replaced.
        unsafe {
            command.pre_exec(move || {
                if libc::setpgid(0, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                if let Some(events) = &events {
                    if libc::fcntl(events.as_raw_fd(), libc::F_SETFD, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let started = Instant::now();
        let child = command.spawn().unwrap();
        Self {
            group: child.id() as libc::pid_t,
            child: Some(child),
            started,
        }
    }

    fn child(&mut self) -> &mut Child {
        self.child.as_mut().expect("fixture child")
    }

    /// Drain each readable pipe before waiting again, sharing one fail ceiling.
    // This test helper prints captured shell evidence into nextest's reports.
    #[allow(clippy::print_stderr)]
    fn finish(&mut self) -> (String, Duration) {
        let deadline = Instant::now() + WITHIN;
        let stages = support::readiness::Stages::new(WITHIN);
        let mut stdout = self.child().stdout.take().expect("fixture stdout");
        let mut stderr = self.child().stderr.take().expect("fixture stderr");
        let mut output = Vec::new();
        let mut errors = Vec::new();
        let mut open = [true, true];
        while open.iter().any(|&open| open) {
            let descriptors = [stdout.as_raw_fd(), stderr.as_raw_fd()];
            let indices: Vec<_> = (0..2).filter(|&i| open[i]).collect();
            let ready = stages
                .readable(&indices.iter().map(|&i| descriptors[i]).collect::<Vec<_>>())
                .expect("hook output stayed silent without completing");
            let index = indices[ready];
            let mut bytes = [0; 4096];
            let count = if index == 0 {
                stdout.read(&mut bytes)
            } else {
                stderr.read(&mut bytes)
            }
            .expect("read fixture output");
            if count == 0 {
                open[index] = false;
            } else if index == 0 {
                output.extend_from_slice(&bytes[..count]);
            } else {
                errors.extend_from_slice(&bytes[..count]);
            }
        }
        let status = self
            .wait_for_exit(deadline.saturating_duration_since(Instant::now()))
            .expect("hook did not exit after closing output");
        assert!(
            status.success(),
            "hook exit {status}: {}",
            String::from_utf8_lossy(&errors)
        );
        if !errors.is_empty() {
            eprintln!("hook stderr/trace:\n{}", String::from_utf8_lossy(&errors));
        }
        (
            String::from_utf8_lossy(&output).into_owned(),
            self.started.elapsed(),
        )
    }
    /// Observe exit without reaping, retaining PID ownership until cleanup ends.
    fn observe_exit(&mut self, ceiling: Duration) -> io::Result<()> {
        let pid = self.child().id();
        let (send, receive) = std::sync::mpsc::channel();
        let observer = thread::spawn(move || {
            let result = loop {
                // WNOWAIT leaves the child waitable, so its PID cannot be reused.
                let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
                let result = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        pid as libc::id_t,
                        &mut info,
                        libc::WEXITED | libc::WNOWAIT,
                    )
                };
                if result == 0 {
                    break Ok(());
                }
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    break Err(error);
                }
            };
            let _ = send.send(result);
        });
        let result = receive.recv_timeout(ceiling).map_err(|error| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("fixture exit stayed silent: {error}"),
            )
        });
        if result.is_err() {
            // The observer has not reaped our leader, reserving this group ID.
            unsafe {
                libc::kill(-self.group, libc::SIGKILL);
            }
        }
        observer.join().expect("exit observer panicked");
        result??;
        Ok(())
    }

    /// Explicit reaping clears ownership before any later Drop can signal.
    fn wait_for_exit(&mut self, ceiling: Duration) -> io::Result<std::process::ExitStatus> {
        self.observe_exit(ceiling)?;
        let status = self.child().wait()?;
        self.child.take();
        Ok(status)
    }
}

impl Drop for FixtureChild {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            // Even an exited leader reserves its PID until reaped. Signal first
            // so live descendants cannot survive their leader's exit.
            unsafe {
                libc::kill(-self.group, libc::SIGKILL);
            }
            let _ = child.wait();
        }
    }
}

fn parse_fixture_pid(contents: &str) -> io::Result<libc::pid_t> {
    contents
        .trim()
        .parse::<libc::pid_t>()
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid fixture PID"))
}

/// Wait on the inherited evidence pipe. EOF, invalid data and silence fail.
fn wait_for_pid_event(events: &mut UnixStream, ceiling: Duration) -> io::Result<libc::pid_t> {
    let line = support::readiness::Stages::new(ceiling).line(events)?;
    let value = line.strip_prefix("pid ").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected PID event: {line}"),
        )
    })?;
    parse_fixture_pid(value)
}

/// Preserve the failed protocol stage and error kind in the final hook report.
fn at_stage(stage: &str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("fixture {stage}: {error}"))
}

fn invocation_evidence(
    events: &mut UnixStream,
    path: &Path,
    pid_file: &Path,
    ceiling: Duration,
) -> io::Result<libc::pid_t> {
    let stages = support::readiness::Stages::new(ceiling);
    let pid = wait_for_pid_event(events, ceiling).map_err(|err| at_stage("pid", err))?;
    for (stage, expected) in [
        ("path", format!("path {}", path.display())),
        ("argv", "argv hook claude question".to_owned()),
        ("published", "published".to_owned()),
    ] {
        stages
            .expect(events, &expected)
            .map_err(|err| at_stage(stage, err))?;
    }
    let file_pid = fs::read_to_string(pid_file)
        .and_then(|contents| parse_fixture_pid(&contents))
        .map_err(|err| at_stage("published", err))?;
    if pid != file_pid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("fixture published: PID mismatch: event {pid}, file {file_pid}"),
        ));
    }
    Ok(pid)
}

/// Publish inside one directory, so readers cannot see a truncated PID.
fn publish_fixture_pid(pid_file: &Path) -> String {
    format!(
        r#"printf '%s\n' "$$" > '{0}.tmp' && mv '{0}.tmp' '{0}' || exit 3"#,
        pid_file.display()
    )
}

fn event_publisher(dir: &Path, body: &str) -> (FixtureChild, UnixStream) {
    let script = fake_flk(dir, "publisher", body);
    let (events, writer) = UnixStream::pair().unwrap();
    let mut command = Command::new(script);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    (
        FixtureChild::spawn_with_events(command, Some(writer)),
        events,
    )
}

#[test]
fn pid_event_waits_for_a_gated_publisher() {
    let directory = SocketDir::reserve("hqpid");
    let (mut publisher, mut events) = event_publisher(directory.path(),
        "printf 'ready\\n' >&\"$FLOCK_TEST_EVENT_FD\"\nIFS= read -r stage\n[ \"$stage\" = publish ] || exit 2\nprintf 'pid %s\\n' \"$$\" >&\"$FLOCK_TEST_EVENT_FD\"\nIFS= read -r stage");
    let stages = support::readiness::Stages::new(WITHIN);
    stages.expect(&mut events, "ready").unwrap();
    let mut gate = publisher.child().stdin.take().unwrap();
    gate.write_all(b"publish\n").unwrap();
    assert_eq!(
        wait_for_pid_event(&mut events, WITHIN).unwrap(),
        publisher.child().id() as libc::pid_t
    );
}

#[test]
fn pid_event_fails_when_the_publisher_exits() {
    let directory = SocketDir::reserve("hqdead");
    let (mut child, mut events) = event_publisher(directory.path(), "exit 7");
    let error = wait_for_pid_event(&mut events, WITHIN).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    child.wait_for_exit(WITHIN).unwrap();
}

#[test]
fn pid_event_fails_when_a_live_publisher_stays_silent() {
    let directory = SocketDir::reserve("hqsilent");
    let (mut child, mut events) = event_publisher(directory.path(), "IFS= read -r stage");
    let error = wait_for_pid_event(&mut events, Duration::from_millis(50)).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert_eq!(
        unsafe { libc::kill(child.child().id() as libc::pid_t, 0) },
        0
    );
}

#[test]
fn pid_event_rejects_empty_partial_nonpositive_and_unexpected_publications() {
    let directory = SocketDir::reserve("hqinvalid");
    for event in ["pid ", "pid 12partial", "pid 0", "pid -1", "wrong-stage"] {
        let (_child, mut events) = event_publisher(
            directory.path(),
            &format!("printf '%s\\n' '{event}' >&\"$FLOCK_TEST_EVENT_FD\"\nIFS= read -r stage"),
        );
        assert_eq!(
            wait_for_pid_event(&mut events, WITHIN).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}

#[test]
fn fixture_finish_drains_output_and_drop_does_not_signal_a_reaped_group() {
    let directory = SocketDir::reserve("hqfinish");
    let (mut sentinel, mut events) = event_publisher(
        directory.path(),
        "IFS= read -r stage\nprintf 'alive\\n' >&\"$FLOCK_TEST_EVENT_FD\"\nIFS= read -r stage",
    );
    // Each script has a different path while the sentinel is still executing.
    let script = fake_flk(
        directory.path(),
        "finishes",
        "i=0\nwhile [ \"$i\" -lt 8192 ]; do\nprintf 1234567890abcdef\ni=$((i+1))\ndone\nprintf diagnostic >&2",
    );
    let mut command = Command::new(script);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = FixtureChild::spawn_with_events(command, None);
    assert_eq!(child.finish().0, "1234567890abcdef".repeat(8192));
    assert!(child.child.is_none());
    // Point at another owned fixture: a stale-group signal would kill it.
    child.group = sentinel.child().id() as libc::pid_t;
    drop(child);
    sentinel
        .child()
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"check\n")
        .unwrap();
    support::readiness::Stages::new(WITHIN)
        .expect(&mut events, "alive")
        .unwrap();
    // The acknowledgment proves the sentinel survived without reaping it.
}

#[test]
fn fixture_exit_wait_fails_when_output_closes_but_the_child_lives() {
    let directory = SocketDir::reserve("hqexit");
    let (mut child, mut events) = event_publisher(
        directory.path(),
        "exec 1>&- 2>&-\nprintf 'closed\\n' >&\"$FLOCK_TEST_EVENT_FD\"\nIFS= read -r stage",
    );
    support::readiness::Stages::new(WITHIN)
        .expect(&mut events, "closed")
        .unwrap();
    let error = child.wait_for_exit(Duration::from_millis(50)).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(!child.wait_for_exit(WITHIN).unwrap().success());
}

#[test]
fn invocation_eof_names_each_missing_stage() {
    let directory = SocketDir::reserve("hqstage");
    let lines = [
        "pid 123\n",
        "path fixture\n",
        "argv hook claude question\n",
        "published\n",
    ];
    for (index, stage) in ["pid", "path", "argv", "published"].iter().enumerate() {
        let (mut events, mut writer) = UnixStream::pair().unwrap();
        writer
            .write_all(lines[..index].concat().as_bytes())
            .unwrap();
        drop(writer);
        let error = invocation_evidence(
            &mut events,
            Path::new("fixture"),
            &directory.path().join("unused.pid"),
            WITHIN,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(
            error.to_string(),
            format!("fixture {stage}: stage producer closed")
        );
    }
}

#[test]
fn invocation_silence_names_each_missing_stage() {
    let directory = SocketDir::reserve("hqstage");
    let lines = [
        "pid 123\n",
        "path fixture\n",
        "argv hook claude question\n",
        "published\n",
    ];
    for (index, stage) in ["pid", "path", "argv", "published"].iter().enumerate() {
        let (mut events, mut writer) = UnixStream::pair().unwrap();
        writer
            .write_all(lines[..index].concat().as_bytes())
            .unwrap();
        let error = invocation_evidence(
            &mut events,
            Path::new("fixture"),
            &directory.path().join("unused.pid"),
            Duration::from_millis(50),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(
            error.to_string(),
            format!("fixture {stage}: fixture stage stayed silent")
        );
    }
}

#[test]
fn invocation_pid_mismatch_returns_a_published_error() {
    let directory = SocketDir::reserve("hqmismatch");
    let path = directory.path().join("fixture.pid");
    fs::write(&path, "456\n").unwrap();
    let (mut events, mut writer) = UnixStream::pair().unwrap();
    writer
        .write_all(b"pid 123\npath fixture\nargv hook claude question\npublished\n")
        .unwrap();
    let error = invocation_evidence(&mut events, Path::new("fixture"), &path, WITHIN).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(
        error.to_string(),
        "fixture published: PID mismatch: event 123, file 456"
    );
}

#[test]
fn fixture_event_descriptor_is_owned_before_the_exec_error_pipe() {
    let directory = SocketDir::reserve("hqfd");
    let _occupied = UnixStream::pair().unwrap();
    let (mut child, mut events) = event_publisher(
        directory.path(),
        "printf 'pid %s\\n' \"$$\" >&\"$FLOCK_TEST_EVENT_FD\"\nIFS= read -r stage",
    );
    assert_eq!(
        wait_for_pid_event(&mut events, WITHIN).unwrap(),
        child.child().id() as libc::pid_t
    );
}

#[test]
fn fixture_drop_kills_descendants_of_an_exited_unreaped_leader() {
    let directory = SocketDir::reserve("hqgroup");
    let (mut child, mut events) = event_publisher(directory.path(),
        "/bin/sh -c 'printf \"descendant %s\\n\" \"$$\" >&\"$FLOCK_TEST_EVENT_FD\"; IFS= read -r stay' <&0 &\nexit 0");
    // Keep the stdin writer outside the owner: dropping Child alone must not
    // release the descendant's gate and disguise a missing group signal.
    let _gate = child.child().stdin.take().unwrap();
    let stages = support::readiness::Stages::new(WITHIN);
    let line = stages.line(&mut events).unwrap();
    assert!(line.starts_with("descendant "));
    child.observe_exit(WITHIN).unwrap();
    drop(child);
    assert_eq!(
        stages.line(&mut events).unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );
}

fn fake_flk(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn flk() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_flk"))
}

/// The first exec of a freshly linked binary can pay a one-time OS scan of
/// it (measured near 2 s on macOS). That is not the hook's latency, so the
/// timed cases run a binary that has already started once.
fn warm(bin: &Path) {
    let status = Command::new(bin)
        .arg("--version")
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
}

/// ADR-0034 §4 with the server stopped: a hinted pane is refused with the
/// sentence for its hint, and a root (no hint, or `ask_human`) passes within
/// the 500 ms of every other hook call, so its dialog draws as today.
#[test]
fn with_the_server_stopped_a_hinted_delegate_is_refused_and_a_root_passes_at_once() {
    let dir = unique_dir("stop");
    let socket = dir.join("gone.sock");
    warm(&flk());
    for (hint, want) in [
        (Some("route"), ROUTE),
        (Some("ask_operator"), ASK_OPERATOR),
        (Some("deny"), DENY),
    ] {
        let (got, _) = run_stub(&socket, &flk(), hint);
        assert_eq!(got.as_deref(), Some(want), "{hint:?}");
    }
    for hint in [None, Some("ask_human")] {
        let (got, elapsed) = run_stub(&socket, &flk(), hint);
        assert_eq!(got.as_deref(), Some(""), "{hint:?} passes");
        assert!(
            elapsed < Duration::from_millis(500),
            "{hint:?}: {elapsed:?}"
        );
    }
    cleanup_test_base(&dir);
}

/// A server that accepts and never answers: a root still passes on the
/// 500 ms deadline, never the question deadline, and a hinted delegate is
/// refused by `flk hook` itself once its 5 s deadline passes, before the
/// stub's 10 s watchdog.
#[test]
fn a_silent_server_holds_a_root_500_ms_and_a_delegate_5_s() {
    let dir = unique_dir("silent");
    let socket = dir.join("silent.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let held = thread::spawn(move || {
        let mut open = Vec::new();
        for stream in listener.incoming().take(2) {
            open.push(stream.unwrap());
        }
        thread::sleep(Duration::from_secs(8));
        drop(open);
    });
    warm(&flk());

    let (root, elapsed) = run_stub(&socket, &flk(), None);
    assert_eq!(root.as_deref(), Some(""));
    assert!(
        elapsed >= Duration::from_millis(500) && elapsed < Duration::from_secs(5),
        "a root waits its 500 ms and no longer: {elapsed:?}"
    );

    let (delegate, elapsed) = run_stub(&socket, &flk(), Some("route"));
    assert_eq!(delegate.as_deref(), Some(ROUTE));
    assert!(
        elapsed >= Duration::from_secs(5) && elapsed < Duration::from_secs(10),
        "refused by flk's own deadline, not the watchdog: {elapsed:?}"
    );
    held.join().unwrap();
    cleanup_test_base(&dir);
}

/// A server too old for `hook.decide` answers it as an invalid request, and
/// the client's skew probe then learns its protocol is older
/// (`server_version_gap`, ADR-0034 §8): the §4 table applies exactly as when
/// the server is stopped.
#[test]
fn an_older_server_is_a_server_that_cannot_decide() {
    let dir = unique_dir("old");
    let socket = dir.join("old.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let finished = done.clone();
    let server = thread::spawn(move || {
        let mut methods = Vec::new();
        while !finished.load(std::sync::atomic::Ordering::Acquire) {
            let mut stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(err) => panic!("accept: {err}"),
            };
            stream.set_nonblocking(false).unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            let method = request["method"].as_str().unwrap_or_default().to_string();
            let answer = if method == "ping" {
                serde_json::json!({"id": request["id"], "result": {"type": "pong",
                    "version": "2.0.0", "protocol": 28}})
            } else {
                serde_json::json!({"id": "", "error": {"code": "invalid_request",
                    "message": "invalid request: unknown variant `hook.decide`, expected one of `ping`"}})
            };
            writeln!(stream, "{answer}").unwrap();
            methods.push(method);
        }
        methods
    });

    assert_eq!(
        run_stub(&socket, &flk(), Some("route")).0.as_deref(),
        Some(ROUTE)
    );
    assert_eq!(run_stub(&socket, &flk(), None).0.as_deref(), Some(""));
    done.store(true, std::sync::atomic::Ordering::Release);
    let methods = server.join().unwrap();
    assert_eq!(
        methods.iter().filter(|m| *m == "hook.decide").count(),
        2,
        "{methods:?}"
    );
    assert!(
        methods.iter().all(|m| m == "hook.decide" || m == "ping"),
        "{methods:?}"
    );
    cleanup_test_base(&dir);
}

/// The stub's own fail-closed path (ADR-0034 §4, "When the hook itself
/// fails"): a missing binary, a crash, output that is not JSON and no output
/// at all each refuse a hinted delegate, and leave a root's dialog alone.
#[test]
fn a_missing_crashing_or_garbled_flk_refuses_a_hinted_delegate() {
    let dir = unique_dir("broken");
    let socket = dir.join("unused.sock");
    let broken = [
        dir.join("no-such-flk"),
        fake_flk(&dir, "crash", "exit 3"),
        fake_flk(&dir, "garbage", "echo not json"),
        fake_flk(&dir, "silent", "exit 0"),
    ];
    for bin in &broken {
        let (got, _) = run_stub(&socket, bin, Some("route"));
        assert_eq!(got.as_deref(), Some(ROUTE), "{}", bin.display());
        let (got, _) = run_stub(&socket, bin, Some("deny"));
        assert_eq!(got.as_deref(), Some(DENY), "{}", bin.display());
    }
    // A root is not refused by a broken flk: whatever it printed passes
    // through as before, and the dialog draws as today.
    for bin in &broken {
        let (stdout, _) = run_stub_raw(&socket, bin, None);
        assert!(!stdout.contains("permissionDecision"), "{}", bin.display());
    }
    cleanup_test_base(&dir);
}

/// A `flk` that hangs is killed by the stub's 10 s watchdog, below the
/// entry's 30 s timeout, and the delegate is refused. The fake records its
/// pid, so the test can show the watchdog really ended it.
#[test]
fn a_hanging_flk_is_killed_by_the_watchdog_and_the_delegate_refused() {
    let directory = SocketDir::reserve("hqhang");
    let dir = directory.path();
    let pid_file = dir.join("hang.pid");
    let hang = fake_flk(
        dir,
        "hang",
        &format!(
            r#"printf 'pid %s\n' "$$" >&"$FLOCK_TEST_EVENT_FD"
printf 'path %s\n' "$0" >&"$FLOCK_TEST_EVENT_FD"
printf 'argv %s\n' "$*" >&"$FLOCK_TEST_EVENT_FD"
{0}
printf 'published\n' >&"$FLOCK_TEST_EVENT_FD"
exec sleep 60"#,
            publish_fixture_pid(&pid_file)
        ),
    );
    let (mut events, writer) = UnixStream::pair().unwrap();
    let mut hook = start_stub_with_events(
        &dir.join("unused.sock"),
        &hang,
        Some("ask_operator"),
        Some(writer),
    );
    let evidence = invocation_evidence(&mut events, &hang, &pid_file, WITHIN);
    if let Ok(pid) = &evidence {
        eprintln!("fixture invoked: path={} argv=hook claude question pid={pid}; PID publication confirmed", hang.display());
    }
    let (stdout, elapsed) = hook.finish();
    let pid = evidence.unwrap_or_else(|error| panic!("fixture invocation/publication failed: {error}; elapsed={elapsed:?}; hook stdout={stdout:?}; shell trace above"));
    // Retain the historical post-completion read: detect lost publication too.
    let final_pid = fs::read_to_string(&pid_file)
        .and_then(|contents| parse_fixture_pid(&contents))
        .unwrap_or_else(|error| panic!("PID publication lost after hook completion: {error}; fixture pid={pid}; elapsed={elapsed:?}; shell trace above"));
    assert_eq!(final_pid, pid);
    let got = reason(&stdout);

    assert_eq!(got.as_deref(), Some(ASK_OPERATOR));
    assert!(
        elapsed >= Duration::from_secs(10) && elapsed < Duration::from_secs(30),
        "the watchdog fired, not Claude's timeout: {elapsed:?}"
    );
    // Assert before dropping the group guard: only the watchdog can prove this.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    assert!(!alive, "the hung flk ({pid}) outlived the hook");
}

// ---- the server half ----

const WITHIN: Duration = Duration::from_secs(20);

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

fn start_server(config: &str) -> Server {
    let base = unique_dir("srv");
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket = runtime_dir.join("flock.sock");
    let app_dir = support::paths::app_dir_name();
    fs::create_dir_all(config_home.join(app_dir)).unwrap();
    fs::create_dir_all(&runtime_dir).unwrap();
    for dir in ["work", "home", "state", "data"] {
        fs::create_dir_all(base.join(dir)).unwrap();
    }
    register_runtime_dir(&runtime_dir);
    fs::write(
        config_home.join(app_dir).join("config.toml"),
        format!("onboarding = false\n{config}"),
    )
    .unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 100,
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
    cmd.env("FLOCK_SOCKET_PATH", &socket);
    cmd.env_remove("FLOCK_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("FLOCK_ENV");
    cmd.env_remove("FLOCK_HOST_NAME");
    cmd.env_remove("FLOCK_DISABLE_SOUND");
    cmd.env_remove("FLOCK_QUESTIONS");
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

fn request(server: &Server, value: serde_json::Value) -> serde_json::Value {
    let mut stream = UnixStream::connect(&server.socket).unwrap();
    stream.write_all(value.to_string().as_bytes()).unwrap();
    stream.write_all(b"\n").unwrap();
    stream.flush().unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

/// An agent pane that runs whatever shell the test drops into
/// `<base>/<name>.cmd`, with the pane's own environment.
struct Agent {
    name: &'static str,
}

fn runner(server: &Server, name: &str) -> PathBuf {
    let base = server.base.display();
    let script = server.base.join(name).join("agent");
    fs::create_dir_all(script.parent().unwrap()).unwrap();
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             while :; do\n\
             if [ -e '{base}/{name}.cmd' ]; then\n\
             sh '{base}/{name}.cmd' > '{base}/{name}.tmp' 2>> '{base}/{name}.err'\n\
             rm '{base}/{name}.cmd'\n\
             mv '{base}/{name}.tmp' '{base}/{name}.out'\n\
             fi\n\
             sleep 0.05\n\
             done\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// A root: started over the socket by the test, which is no agent, so the
/// operator start of ADR-0034 §2 (`ask_human`, no hint).
fn start_root(server: &Server, name: &'static str) -> Agent {
    let script = runner(server, name);
    let started = request(
        server,
        serde_json::json!({"id": "root", "method": "agent.start", "params": {
            "name": name, "cwd": server.base.join("work"), "argv": [script]}}),
    );
    assert_eq!(
        started["result"]["agent"]["questions"], "ask_human",
        "{started}"
    );
    Agent { name }
}

impl Agent {
    /// Run `body` inside this agent's pane and return its stdout.
    fn run(&self, server: &Server, body: &str) -> String {
        let out = server.base.join(format!("{}.out", self.name));
        let _ = fs::remove_file(&out);
        let prelude = format!(
            "export FLOCK_SOCKET_PATH='{}' FLOCK_BIN='{}'\n",
            server.socket.display(),
            env!("CARGO_BIN_EXE_flk")
        );
        fs::write(
            server.base.join(format!("{}.cmd", self.name)),
            format!("{prelude}{body}\n"),
        )
        .unwrap();
        let deadline = Instant::now() + WITHIN;
        while !out.exists() {
            assert!(
                Instant::now() < deadline,
                "{} never ran {body:?}: {}",
                self.name,
                fs::read_to_string(server.base.join(format!("{}.err", self.name)))
                    .unwrap_or_default()
            );
            thread::sleep(Duration::from_millis(30));
        }
        fs::read_to_string(out).unwrap()
    }

    /// A delegate this agent starts from its own pane, so the server records
    /// the edge and the policy (#1179).
    fn delegate(&self, server: &Server, name: &'static str, flags: &str) -> Agent {
        let script = runner(server, name);
        let started = self.run(
            server,
            &format!(
                "\"$FLOCK_BIN\" agent start {name} {flags} --cwd '{}' --no-focus -- '{}'",
                server.base.join("work").display(),
                script.display()
            ),
        );
        assert!(!started.trim().is_empty(), "{name} did not start");
        Agent { name }
    }

    /// The installed stub's question action, fired as Claude Code fires it,
    /// with `env` prefixed (to change the pane's own environment).
    fn ask(&self, server: &Server, env: &str, tool: &str) -> Option<String> {
        let payload = ASK.replace("AskUserQuestion", tool);
        reason(&self.run(
            server,
            &format!(
                "printf '%s' '{payload}' | {env} sh '{}' question",
                stub_path().display()
            ),
        ))
    }
}

/// Isolated flk, a root and two delegates it started: the root's question
/// tools pass, a `route` delegate's are refused with the interim sentence,
/// an explicit `deny` delegate's with its own, and a tool listed in
/// `[questions] human_tools` is refused like a question tool. The server's
/// record decides, not the pane's hint.
#[test]
fn the_server_decides_from_the_agent_record() {
    let server = start_server("[questions]\nhuman_tools = [\"mcp__tg-mcp__ask\"]\n");
    let root = start_root(&server, "root");
    let routed = root.delegate(&server, "routed", "");
    let denied = root.delegate(&server, "denied", "--questions deny");

    assert_eq!(
        routed.run(&server, "printf %s \"$FLOCK_QUESTIONS\""),
        "route"
    );
    for tool in ["AskUserQuestion", "ExitPlanMode", "mcp__tg-mcp__ask"] {
        assert_eq!(root.ask(&server, "", tool).as_deref(), Some(""), "{tool}");
        assert_eq!(
            routed.ask(&server, "", tool).as_deref(),
            Some(UNROUTED),
            "{tool}"
        );
        assert_eq!(
            denied.ask(&server, "", tool).as_deref(),
            Some(DENY),
            "{tool}"
        );
    }

    // The record is the authority: a delegate that drops its hint is still
    // refused, and a root that claims one still passes.
    assert_eq!(
        routed
            .ask(&server, "env -u FLOCK_QUESTIONS", "AskUserQuestion")
            .as_deref(),
        Some(UNROUTED)
    );
    assert_eq!(
        root.ask(&server, "env FLOCK_QUESTIONS=route", "AskUserQuestion")
            .as_deref(),
        Some("")
    );
    // A tool that asks nobody passes, whatever the policy.
    assert_eq!(routed.ask(&server, "", "Read").as_deref(), Some(""));
}

#[test]
fn claude_probe_uses_the_namespace_passed_by_the_test_profile() {
    let root = SocketDir::reserve("probe-ns");
    let probe = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/probes/question_hook_claude.py");
    for (index, spelling) in ["renamed-flk", "debug/flk"].iter().enumerate() {
        let sandbox = root.path().join(format!("case-{index}"));
        let binary = root.path().join(spelling);
        let output = Command::new("python3")
            .args([
                "-I",
                "-c",
                r#"
import pathlib, runpy, sys
probe, binary, root, namespace = sys.argv[1:]
Sandbox = runpy.run_path(probe)["Sandbox"]
sandbox = Sandbox(binary, root, namespace)
class FixtureBoundary(Exception):
    pass
def no_process(*args, **kwargs):
    assert args == ("integration", "install", "claude")
    raise FixtureBoundary()
sandbox.run = no_process
try:
    sandbox.prepare()
except FixtureBoundary:
    pass
else:
    raise AssertionError("fixture unexpectedly started a process")
config = pathlib.Path(root) / "x" / namespace / "config.toml"
assert config.read_text() == "onboarding = false\n"
other = "flock" if namespace == "flock-dev" else "flock-dev"
assert not (pathlib.Path(root) / "x" / other).exists()
"#,
            ])
            .arg(&probe)
            .arg(&binary)
            .arg(&sandbox)
            .arg(support::paths::app_dir_name())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
