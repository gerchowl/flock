use super::*;
use crate::structured::launch::{RunAs, StructuredConfig};
use crate::structured::supervision::ProcessIdentity;
use std::ffi::{CString, OsString};
use std::fs::{self, File, OpenOptions};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::Instant;

const TOKEN: &str = "fixture-token-not-a-credential";
const SESSION: &str = "00000000-0000-4000-8000-000000000001";

struct Fixture {
    _lifetime: File,
    root: SessionConfigDir,
}
impl Fixture {
    fn new() -> Self {
        let root = SessionConfigDir::create(&std::env::temp_dir()).expect("fixture root");
        let path = root.path().join("lifetime");
        let name = CString::new(path.as_os_str().as_bytes()).expect("FIFO path");
        // SAFETY: private owned fixture path; no ambient process or file is used.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let lifetime = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .expect("fixture lifeline");
        let fixture = Self {
            _lifetime: lifetime,
            root,
        };
        fixture.file("token", TOKEN.as_bytes(), 0o600);
        fixture.file(
            "events.jsonl",
            include_bytes!("../../../tests/fixtures/claude-session/events.jsonl"),
            0o600,
        );
        fixture.file(
            "restricted-init.json",
            include_bytes!("../../../tests/fixtures/structured-runtime/restricted-init.json"),
            0o600,
        );
        let python = TracedCommand::new("python3", "fixture-python")
            .args(["-c", "import sys; print(sys.executable)"])
            .output_traced()
            .expect("fixture interpreter");
        assert!(python.status.success());
        let python = PathBuf::from(
            String::from_utf8(python.stdout)
                .expect("interpreter path")
                .trim(),
        )
        .canonicalize()
        .expect("canonical interpreter");
        let script = format!(
            "#!{}\n{}",
            python.display(),
            include_str!("../../../tests/fixtures/structured-runtime/claude.py")
        );
        fixture.file("claude", script.as_bytes(), 0o700);
        fixture
    }
    fn file(&self, name: &str, bytes: &[u8], mode: u32) -> PathBuf {
        let path = self.root.path().join(name);
        fs::write(&path, bytes).expect("fixture file");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("fixture mode");
        path
    }
    fn profile(&self, mode: &str, version: &str) -> LaunchProfile {
        self.file(
            "case.json",
            serde_json::json!({"mode":mode,"version":version})
                .to_string()
                .as_bytes(),
            0o600,
        );
        let config = StructuredConfig {
            run_as: RunAs::Operator,
            token_file: Some(self.root.path().join("token")),
            profile: "fixture".into(),
        };
        let binary = VerifiedBinary::check(&self.root.path().join("claude"), |_, _| {
            Ok("2.1.296 (Claude Code)".into())
        })
        .expect("preparation seam");
        let profile =
            LaunchProfile::prepare(
                &config,
                self.root.path(),
                SESSION,
                binary,
                |key| match key {
                    "HOME" | "TMPDIR" => Some(self.root.path().as_os_str().to_owned()),
                    "PATH" => Some(self.root.path().join("bin").into_os_string()),
                    "LANG" | "LC_ALL" => Some(OsString::from("C")),
                    "USER" | "LOGNAME" => Some(OsString::from("fixture-user")),
                    _ => None,
                },
            )
            .expect("profile");
        let args: Vec<_> = profile
            .args()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        self.file(
            "argv.json",
            serde_json::to_vec(&args).expect("argv JSON").as_slice(),
            0o600,
        );
        profile
    }
    fn launch(&self, mode: &str, version: &str) -> Result<Running, LaunchError> {
        self.launch_at(mode, version, SESSION)
    }
    fn launch_at(&self, mode: &str, version: &str, session: &str) -> Result<Running, LaunchError> {
        let profile = self.profile(mode, version);
        let args = profile.args();
        let index = args
            .iter()
            .position(|arg| arg == "--settings")
            .expect("settings arg");
        let generated = PathBuf::from(&args[index + 1])
            .parent()
            .expect("generated parent")
            .to_owned();
        let persistent = profile.config_dir().to_owned();
        let state = self.root.path().to_owned();
        let location = StreamLocation {
            data_dir: state.clone(),
            agent: "fixture-agent".into(),
            session: session.into(),
        };
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let result = run(&state, profile, location, |runtime| {
                let _ = ready_tx.send(runtime);
            });
            let _ = result_tx.send(result);
        });
        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(runtime) => Ok(Running {
                runtime,
                result: result_rx,
                worker: Some(worker),
                finished: false,
                generated,
                persistent,
            }),
            Err(error) => {
                let result = result_rx
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap_or_else(|cause| panic!("start producer failed: {error}; {cause}"));
                worker.join().expect("start worker");
                match result {
                    Err(error) => Err(error),
                    Ok(exit) => panic!("missing ready: {exit:?}"),
                }
            }
        }
    }
    fn stream(&self) -> PathBuf {
        self.root
            .path()
            .join("sessions/fixture-agent")
            .join(format!("{SESSION}.jsonl"))
    }
    fn identity(&self, name: &str) -> ProcessIdentity {
        let pid = fs::read_to_string(self.root.path().join(name))
            .expect("producer pid")
            .parse()
            .expect("pid");
        ProcessIdentity::capture(pid).expect("owned fixture alive")
    }
}

struct Running {
    runtime: Runtime,
    result: mpsc::Receiver<Result<Exit, LaunchError>>,
    worker: Option<JoinHandle<()>>,
    finished: bool,
    generated: PathBuf,
    persistent: PathBuf,
}
impl Running {
    fn receive(&self) -> Received {
        self.runtime
            .events
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|error| {
                panic!(
                    "missing fixture event: {error}; worker: {:?}",
                    self.result.try_recv()
                )
            })
    }
    fn wait<T>(&self, mut poll: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(value) = poll() {
                return value;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.result.recv_timeout(TICK.min(remaining)) {
                Ok(result) => panic!("worker exited before condition: {result:?}"),
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("worker disconnected"),
                Err(mpsc::RecvTimeoutError::Timeout) => assert!(
                    !remaining.is_zero(),
                    "condition deadline with live producer"
                ),
            }
        }
    }
    fn submit(&self) {
        self.offer(&serde_json::json!({"type":"user","message":{"role":"user","content":"hello"}}));
    }
    fn offer(&self, input: &Value) {
        self.wait(|| match self.runtime.try_submit(input) {
            Ok(()) => Some(()),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => None,
            Err(error) => panic!("user input: {error}"),
        });
    }
    fn finish(&mut self) -> Exit {
        let result = self
            .result
            .recv_timeout(Duration::from_secs(10))
            .expect("bounded owned worker");
        self.finished = true;
        self.worker
            .take()
            .expect("worker")
            .join()
            .expect("runtime worker");
        result.expect("runtime started")
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        if !self.finished {
            self.runtime.stop();
            let _ = self.result.recv_timeout(Duration::from_secs(10));
            if let Some(worker) = self.worker.take().filter(|worker| worker.is_finished()) {
                let _ = worker.join();
            }
        }
    }
}

#[test]
fn natural_exit_with_stalled_consumer_confirms_io_and_cleans_up() {
    let fixture = Fixture::new();
    let mut running = fixture.launch("normal", "2.1.296").expect("launch");
    running.receive();
    let root = fixture.identity("model.pid");
    running.submit();
    // Never consume the five subsequent notifications: four slots and one
    // pending record fill, despite the child and reader reaching natural EOF.
    let exit = running.finish();
    assert_eq!(exit.outcome, Outcome::Exited, "{exit:?}");
    assert!(exit.status.expect("reaped status").success());
    assert!(exit.error.is_none());
    assert!(exit.delivery_cancelled);
    assert!(!exit.input_inflight);
    assert!(!root.alive());
    assert!(!running.generated.exists());
    assert!(running.persistent.join("transcript.jsonl").exists());
    assert_eq!(
        fs::read_to_string(fixture.stream())
            .expect("durable replay")
            .lines()
            .count(),
        6
    );
}

#[test]
fn idle_session_submission_never_reports_collector_contention_as_busy() {
    let fixture = Fixture::new();
    let mut running = fixture.launch("idle-turns", "2.1.296").expect("launch");
    running.receive();
    running.submit();
    running.receive();
    running.receive();
    running.receive();
    let input = serde_json::json!({"type":"user","message":{"content":"next"}});
    for turn in 0..1000 {
        running
            .runtime
            .try_submit(&input)
            .unwrap_or_else(|error| panic!("idle turn {turn}: {error}"));
        assert!(matches!(
            running.receive().event,
            super::super::claude::SessionEvent::Success(_)
        ));
    }
    running.runtime.stop();
    assert_eq!(running.finish().outcome, Outcome::Stopped);
}

fn assert_uninspectable_init_refused(mode: &str) {
    let fixture = Fixture::new();
    let mut running = fixture.launch(mode, "2.1.296").expect("launch");
    running.receive();
    let root = fixture.identity("model.pid");
    running.submit();
    let exit = running.finish();
    assert_eq!(exit.outcome, Outcome::Refused, "{exit:?}");
    assert!(exit
        .error
        .expect("refusal")
        .contains("structured_uninspectable_output"));
    assert!(exit.delivery_cancelled);
    assert!(!root.alive());
    assert!(!running.generated.exists());
    assert!(fs::read_to_string(fixture.stream())
        .expect("durable evidence")
        .contains("\"origin\":\"flk\""));
}

#[test]
fn oversized_init_cannot_bypass_socket_refusal() {
    assert_uninspectable_init_refused("init-oversize");
}

#[test]
fn non_utf8_init_cannot_bypass_socket_refusal() {
    assert_uninspectable_init_refused("init-invalid-utf8");
}

#[test]
fn missing_child_pipes_without_workers_allows_confirmed_cleanup() {
    let fixture = Fixture::new();
    let profile = fixture.profile("no-read", "2.1.296");
    let persistent = profile.config_dir().to_owned();
    let mut command = TracedCommand::new(profile.binary(), "fixture-no-pipes");
    command
        .args(profile.args())
        .env_clear()
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    profile.configure_environment(|key, value| {
        command.env(key, value);
    });
    let (mut child, profile) = profile
        .try_spawn(|| command.spawn_traced())
        .expect("owned child");
    let files = profile.into_session_files();
    let generated = files.path().to_owned();
    let root = ProcessIdentity::capture(child.id()).expect("owned birth");
    let error = platform::structured::child_pipes(&mut child).expect_err("no piped endpoints");
    let error = startup_failed(child, files, persistent.clone(), error, true);
    assert!(error.to_string().contains("missing stdout pipe"));
    assert!(!root.alive());
    assert!(!generated.exists());
    assert!(persistent.is_dir());
}

#[test]
fn real_fake_launch_checks_env_argv_hooks_large_line_final_line_and_cleanup() {
    let fixture = Fixture::new();
    let mut running = fixture.launch("normal", "2.1.296").expect("launch");
    assert!(matches!(
        running.receive().event,
        super::super::claude::SessionEvent::HookStarted(_)
    ));
    let root = fixture.identity("model.pid");
    running.submit();
    let events: Vec<_> = (0..5).map(|_| running.receive()).collect();
    assert!(matches!(
        events[0].event,
        super::super::claude::SessionEvent::HookResponse(_)
    ));
    assert!(matches!(
        events[1].event,
        super::super::claude::SessionEvent::Init(_)
    ));
    assert!(events[2].record.raw.len() >= 1_048_576);
    assert_eq!(events[4].record.raw, "final-without-lf");
    let exit = running.finish();
    assert_eq!(exit.outcome, Outcome::Exited);
    assert_eq!(exit.status.expect("status").code(), Some(0));
    assert!(!exit.input_inflight);
    assert!(!exit.delivery_cancelled);
    assert!(!root.alive());
    assert!(!running.generated.exists());
    assert!(running.persistent.join("transcript.jsonl").exists());
    let records: Vec<super::super::transport::StreamRecord> = fs::read_to_string(fixture.stream())
        .expect("durable stream")
        .lines()
        .map(|line| serde_json::from_str(line).expect("complete record"))
        .collect();
    assert_eq!(records.len(), 6);
    assert_eq!(
        records.iter().map(|record| record.seq).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5, 6]
    );
}

#[test]
fn failed_model_spawn_removes_armed_files_and_preserves_profile() {
    let fixture = Fixture::new();
    let profile = fixture.profile("normal", "remove");
    let persistent = profile.config_dir().to_owned();
    let error = run(
        fixture.root.path(),
        profile,
        StreamLocation {
            data_dir: fixture.root.path().to_owned(),
            agent: "fixture-agent".into(),
            session: SESSION.into(),
        },
        |_| panic!("failed spawn published a runtime"),
    )
    .expect_err("removed executable cannot spawn");
    assert!(matches!(error, LaunchError::Io(ref error) if error.kind() == io::ErrorKind::NotFound));
    assert!(persistent.is_dir());
    assert_eq!(
        fs::read_dir(fixture.root.path().join("structured/sessions"))
            .expect("session listing")
            .count(),
        0
    );
}

#[test]
fn actual_version_recheck_refuses_old_failed_and_timed_out_binaries() {
    for version in ["2.1.287", "fail", "hang"] {
        let fixture = Fixture::new();
        let error = match fixture.launch("normal", version) {
            Err(error) => error,
            Ok(_) => panic!("version admitted: {version}"),
        };
        match version {
            "2.1.287" => assert!(matches!(error, LaunchError::UnsupportedHarness)),
            "hang" => assert!(
                matches!(error, LaunchError::Io(ref error) if error.kind() == io::ErrorKind::TimedOut)
            ),
            _ => assert!(error.to_string().contains("version command failed")),
        }
        assert!(!fixture.root.path().join("model.pid").exists());
        assert_eq!(
            fs::read_dir(fixture.root.path().join("structured/sessions"))
                .expect("session listing")
                .count(),
            0
        );
    }
}

#[test]
fn registry_refusal_prevents_first_input_including_null_malformed_and_links() {
    for mode in [
        "registry-socket",
        "registry-null",
        "registry-malformed",
        "registry-link",
    ] {
        let fixture = Fixture::new();
        let mut running = fixture.launch(mode, "2.1.296").expect("launch");
        assert!(matches!(
            running.receive().event,
            super::super::claude::SessionEvent::HookStarted(_)
        ));
        let root = fixture.identity("model.pid");
        running.submit();
        let exit = running.finish();
        assert_eq!(exit.outcome, Outcome::Refused, "{mode}: {exit:?}");
        assert!(exit.error.is_some());
        assert!(!fixture.root.path().join("input-received").exists());
        assert!(!root.alive());
        if mode == "registry-socket" || mode == "registry-null" {
            assert!(!running.generated.exists());
            assert!(!running
                .persistent
                .join("sessions")
                .join(format!("{}.json", root.pid))
                .exists());
        } else {
            assert!(running.generated.exists(), "unsafe cleanup retained");
        }
    }
}

#[test]
fn init_socket_presence_refuses_even_null_and_unmodelled_wrong_type() {
    for mode in [
        "init-socket-string",
        "init-socket-null",
        "init-socket-wrong-type",
    ] {
        let fixture = Fixture::new();
        let mut running = fixture.launch(mode, "2.1.296").expect("launch");
        running.receive();
        running.submit();
        let exit = running.finish();
        assert_eq!(exit.outcome, Outcome::Refused, "{mode}: {exit:?}");
        assert!(exit
            .error
            .expect("refusal")
            .contains("structured_messaging_socket_forbidden"));
        assert!(fixture.root.path().join("input-received").exists());
        assert!(!running.generated.exists());
        let records = fs::read_to_string(fixture.stream()).expect("stream");
        assert!(records.contains("messaging_socket_path"));
    }
}

#[test]
fn natural_exit_distinguishes_idle_missing_result_and_unexplained_143() {
    for (mode, expected, code, active) in [
        ("idle", Outcome::Exited, 0, false),
        ("missing-result", Outcome::Crashed, 0, true),
        ("exit-143", Outcome::Crashed, 143, true),
    ] {
        let fixture = Fixture::new();
        let mut running = fixture.launch(mode, "2.1.296").expect("launch");
        running.receive();
        if active {
            running.submit();
            running.receive();
            running.receive();
        }
        let exit = running.finish();
        assert_eq!(exit.outcome, expected, "{mode}: {exit:?}");
        assert_eq!(exit.status.expect("status").code(), Some(code));
        assert_eq!(exit.input_inflight, active);
        assert!(!running.generated.exists());
    }
}

#[test]
fn requested_stop_precedes_interrupted_exit_one() {
    let fixture = Fixture::new();
    let mut running = fixture.launch("interrupt-1", "2.1.296").expect("launch");
    running.receive();
    let root = fixture.identity("model.pid");
    running.submit();
    running.receive();
    running.receive();
    running.runtime.stop();
    let exit = running.finish();
    assert_eq!(exit.outcome, Outcome::Stopped, "{exit:?}");
    assert_eq!(exit.status.expect("status").code(), Some(1));
    assert!(exit.input_inflight);
    assert!(!root.alive());
    assert!(!running.generated.exists());
}

#[test]
fn full_output_queue_does_not_block_stop_or_cleanup() {
    let fixture = Fixture::new();
    let mut running = fixture.launch("flood", "2.1.296").expect("launch");
    running.receive();
    let root = fixture.identity("model.pid");
    running.submit();
    // More durable records than outward/pending capacity proves backpressure
    // without consuming notifications or depending on elapsed wall time.
    running.wait(|| {
        let records = fs::read_to_string(fixture.stream()).ok()?;
        (records.lines().count() >= 10).then_some(())
    });
    running.runtime.stop();
    let exit = running.finish();
    assert_eq!(exit.outcome, Outcome::Stopped, "{exit:?}");
    assert!(exit.delivery_cancelled);
    assert!(!root.alive());
    assert!(!running.generated.exists());
    assert!(fs::read_to_string(fixture.stream())
        .expect("stream")
        .lines()
        .all(|line| serde_json::from_str::<super::super::transport::StreamRecord>(line).is_ok()));
}

#[test]
fn blocked_stdin_does_not_block_server_stop() {
    let fixture = Fixture::new();
    let mut running = fixture.launch("no-read", "2.1.296").expect("launch");
    running.receive();
    let root = fixture.identity("model.pid");
    running.offer(&serde_json::json!({"type":"user","message":{"content":"x".repeat(1_048_576)}}));
    running.wait(|| running.runtime.transport.input_inflight().then_some(()));
    assert!(!fixture.root.path().join("input-received").exists());
    running.runtime.stop();
    let exit = running.finish();
    assert_eq!(exit.outcome, Outcome::Stopped, "{exit:?}");
    assert!(exit.input_inflight);
    assert!(!root.alive());
    assert!(!running.generated.exists());
}

#[test]
fn crash_reaps_previously_observed_detached_descendant() {
    let fixture = Fixture::new();
    let mut running = fixture.launch("tree", "2.1.296").expect("launch");
    running.receive();
    running.submit();
    for _ in 0..3 {
        running.receive();
    }
    let root = fixture.identity("model.pid");
    let detached = fixture.identity("grandchild.pid");
    running.wait(|| {
        let data = fs::read(running.generated.join("owner.json")).ok()?;
        let owner: Value = serde_json::from_slice(&data).ok()?;
        owner["tree"]
            .as_array()?
            .iter()
            .any(|entry| {
                entry["pid"].as_u64() == Some(u64::from(detached.pid))
                    && entry["birth"].as_u64() == Some(detached.birth)
            })
            .then_some(())
    });
    assert!(root.alive());
    // SAFETY: birth-checked process created by this test, never an ambient pid.
    assert_eq!(unsafe { libc::kill(root.pid as i32, libc::SIGUSR2) }, 0);
    let exit = running.finish();
    assert_eq!(exit.outcome, Outcome::Crashed, "{exit:?}");
    assert_eq!(exit.status.expect("status").code(), Some(7));
    assert!(!root.alive());
    assert!(!detached.alive());
    assert!(!running.generated.exists());
}

#[test]
fn corrupt_durable_stream_reports_failure_and_retains_generated_files() {
    let fixture = Fixture::new();
    let stream = fixture.stream();
    fs::create_dir_all(stream.parent().expect("stream parent")).expect("stream dirs");
    fs::write(&stream, "{broken}\n").expect("corrupt complete record");
    let mut running = fixture.launch("normal", "2.1.296").expect("launch");
    let exit = running.finish();
    assert_eq!(exit.outcome, Outcome::Failed, "{exit:?}");
    assert!(exit.error.is_some());
    assert!(running.generated.exists());
    assert_eq!(fs::read_to_string(stream).expect("stream"), "{broken}\n");
}

#[test]
fn publication_panic_shuts_down_owned_process_and_retains_uncertain_files() {
    let fixture = Fixture::new();
    let profile = fixture.profile("no-read", "2.1.296");
    let args = profile.args();
    let index = args
        .iter()
        .position(|arg| arg == "--settings")
        .expect("settings");
    let generated = PathBuf::from(&args[index + 1])
        .parent()
        .expect("parent")
        .to_owned();
    let exit = run(
        fixture.root.path(),
        profile,
        StreamLocation {
            data_dir: fixture.root.path().to_owned(),
            agent: "fixture-agent".into(),
            session: SESSION.into(),
        },
        |_| panic!("owned publication failure"),
    )
    .expect("worker result");
    assert_eq!(exit.outcome, Outcome::Failed, "{exit:?}");
    assert!(exit.status.is_some());
    assert!(exit.error.expect("error").contains("publication panicked"));
    assert!(generated.exists());
}
