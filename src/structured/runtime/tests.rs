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

pub(crate) struct Fixture {
    _lifetime: File,
    pub(crate) root: SessionConfigDir,
    leases: crate::structured::lease::SessionLeases,
}
impl Fixture {
    pub(crate) fn new() -> Self {
        Self::under(&std::env::temp_dir())
    }
    /// Owned short root for Unix sockets, independent of the caller's TMPDIR.
    pub(crate) fn for_socket() -> Self {
        // guardrails-ok(hermetic): Unix sockets need a short base on macOS.
        Self::under(std::path::Path::new("/tmp"))
    }
    pub(crate) fn under(parent: &std::path::Path) -> Self {
        let root = SessionConfigDir::create(parent).expect("fixture root");
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
            leases: Default::default(),
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
    pub(crate) fn file(&self, name: &str, bytes: &[u8], mode: u32) -> PathBuf {
        let path = self.root.path().join(name);
        fs::write(&path, bytes).expect("fixture file");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("fixture mode");
        path
    }
    pub(crate) fn mcp(&self) -> SessionMcp {
        SessionMcp::new(
            &self.root.path().join("claude"),
            &self.root.path().join("api.sock"),
        )
        .expect("owned fixture endpoint")
    }
    pub(crate) fn profile(&self, mode: &str, version: &str) -> LaunchProfile {
        self.profile_at(mode, version, SESSION)
    }
    pub(crate) fn profile_at(&self, mode: &str, version: &str, session: &str) -> LaunchProfile {
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
                session,
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
            storage: Default::default(),
            agent: "fixture-agent".into(),
            session: session.into(),
        };
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let lease = self
            .leases
            .reserve("fixture-agent", &location.session, None)
            .expect("lease");
        let mcp = self.mcp();
        let worker = std::thread::spawn(move || {
            let result = run(&state, profile, lease, location, mcp, |runtime| {
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
    fn receive_after_accepted(&self) -> Received {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let received = self
                .runtime
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| {
                    panic!(
                        "missing fixture event: {error}; worker: {:?}",
                        self.result.try_recv()
                    )
                });
            if !matches!(
                received.state_input(),
                Some(crate::structured::state::Input::Accepted { .. })
            ) {
                return received;
            }
        }
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
    running.receive_after_accepted();
    let root = fixture.identity("model.pid");
    running.submit();
    // Never consume subsequent notifications: four slots plus a pending record
    // fill, despite the child and reader reaching natural EOF.
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
        7
    );
}

#[test]
fn idle_session_submission_never_reports_collector_contention_as_busy() {
    let fixture = Fixture::new();
    let mut running = fixture.launch("idle-turns", "2.1.296").expect("launch");
    running.receive_after_accepted();
    running.submit();
    running.receive_after_accepted();
    running.receive_after_accepted();
    running.receive_after_accepted();
    let input = serde_json::json!({"type":"user","message":{"content":"next"}});
    for turn in 0..1000 {
        running
            .runtime
            .try_submit(&input)
            .unwrap_or_else(|error| panic!("idle turn {turn}: {error}"));
        assert!(matches!(
            running.receive_after_accepted().event,
            super::super::claude::SessionEvent::Success(_)
        ));
    }
    running.runtime.stop();
    assert_eq!(running.finish().outcome, Outcome::Stopped);
}

fn assert_uninspectable_init_refused(mode: &str) {
    let fixture = Fixture::new();
    let mut running = fixture.launch(mode, "2.1.296").expect("launch");
    running.receive_after_accepted();
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

fn assert_later_loss_keeps_session_running(mode: &str) {
    let fixture = Fixture::new();
    let mut running = fixture.launch(mode, "2.1.296").expect("launch");
    running.receive_after_accepted();
    let root = fixture.identity("model.pid");
    running.submit();
    running.receive_after_accepted();
    assert!(matches!(
        running.receive_after_accepted().event,
        super::super::claude::SessionEvent::Init(_)
    ));
    let loss = running.receive_after_accepted();
    assert_eq!(loss.record.origin, RecordOrigin::Flk);
    assert!(matches!(
        loss.event,
        super::super::claude::SessionEvent::Unknown { .. }
    ));
    // The producer emits this after the loss, then remains alive on stdin.
    running.receive_after_accepted();
    assert!(root.alive());
    assert_eq!(
        running
            .runtime
            .try_submit(&serde_json::json!({"type":"user"}))
            .expect_err("same turn is still active")
            .kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(matches!(
        running.result.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    assert!(running.generated.exists());
    let durable = fs::read_to_string(fixture.stream()).expect("durable evidence");
    assert!(durable.contains("\"origin\":\"flk\""));
    running.runtime.stop();
    let exit = running.finish();
    assert_eq!(exit.outcome, Outcome::Stopped, "{exit:?}");
    assert!(!root.alive());
    assert!(!running.generated.exists());
}

#[test]
fn oversized_line_after_clean_init_is_data_loss_not_refusal() {
    assert_later_loss_keeps_session_running("later-oversize");
}

#[test]
fn non_utf8_line_after_clean_init_is_data_loss_not_refusal() {
    assert_later_loss_keeps_session_running("later-invalid-utf8");
}

#[test]
fn oversized_result_after_clean_init_accepts_the_next_runtime_turn() {
    let fixture = Fixture::new();
    let mut running = fixture
        .launch("later-lossy-result", "2.1.296")
        .expect("launch");
    running.receive_after_accepted();
    let root = fixture.identity("model.pid");
    running.submit();
    running.receive_after_accepted();
    running.receive_after_accepted();
    let loss = running.receive_after_accepted();
    assert_eq!(loss.record.origin, RecordOrigin::Flk);
    assert!(matches!(
        loss.event,
        super::super::claude::SessionEvent::Unknown { .. }
    ));
    assert_eq!(
        serde_json::from_str::<Value>(&loss.record.raw).expect("summary")["turn_end"],
        "result"
    );
    running
        .runtime
        .try_submit(&serde_json::json!({"type":"user","message":{"content":"next"}}))
        .expect("lossy result reopens runtime input");
    assert!(matches!(
        running.receive_after_accepted().event,
        super::super::claude::SessionEvent::Success(_)
    ));
    assert!(root.alive());
    running.runtime.stop();
    let exit = running.finish();
    assert_eq!(exit.outcome, Outcome::Stopped, "{exit:?}");
    assert!(!exit.input_inflight);
    assert!(!root.alive());
    assert!(!running.generated.exists());
}

#[test]
fn later_inspectable_init_with_socket_is_still_refused() {
    let fixture = Fixture::new();
    let mut running = fixture
        .launch("later-init-socket", "2.1.296")
        .expect("launch");
    running.receive_after_accepted();
    running.submit();
    let exit = running.finish();
    assert_eq!(exit.outcome, Outcome::Refused, "{exit:?}");
    assert!(exit
        .error
        .expect("refusal")
        .contains("structured_messaging_socket_forbidden"));
    assert!(!running.generated.exists());
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
    let mut lease = fixture
        .leases
        .reserve("fixture-agent", SESSION, None)
        .unwrap();
    lease.spawned().unwrap();
    lease.bind(child.id()).unwrap();
    let error = startup_failed(child, files, persistent.clone(), error, true, &mut lease);
    assert!(error.to_string().contains("missing stdout pipe"));
    assert!(error.to_string().contains("cleanup confirmed"));
    assert!(!root.alive());
    assert!(!generated.exists());
    assert!(persistent.is_dir());
}

#[test]
fn real_fake_launch_checks_env_argv_hooks_large_line_final_line_and_cleanup() {
    let fixture = Fixture::new();
    let mut running = fixture.launch("normal", "2.1.296").expect("launch");
    assert!(matches!(
        running.receive_after_accepted().event,
        super::super::claude::SessionEvent::HookStarted(_)
    ));
    let root = fixture.identity("model.pid");
    running.submit();
    let events: Vec<_> = (0..5).map(|_| running.receive_after_accepted()).collect();
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
    assert_eq!(records.len(), 7);
    assert_eq!(
        records.iter().map(|record| record.seq).collect::<Vec<_>>(),
        (1..=7).collect::<Vec<_>>()
    );
    assert_eq!(
        records
            .iter()
            .filter(|r| serde_json::from_str::<Value>(&r.raw)
                .ok()
                .is_some_and(|v| v["type"] == "flk_input_accepted"))
            .count(),
        1
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
        crate::structured::lease::SessionLeases::default()
            .reserve("fixture-agent", SESSION, None)
            .expect("lease"),
        StreamLocation {
            data_dir: fixture.root.path().to_owned(),
            storage: Default::default(),
            agent: "fixture-agent".into(),
            session: SESSION.into(),
        },
        fixture.mcp(),
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
    let parent = SessionConfigDir::create(&std::env::temp_dir()).expect("owned parent");
    let long_parent = parent.path().join("long-temp-parent-".repeat(7));
    fs::create_dir(&long_parent).expect("long fixture parent");
    for mode in [
        "registry-socket",
        "registry-null",
        "registry-malformed",
        "registry-link",
    ] {
        let fixture = Fixture::under(&long_parent);
        let mut running = fixture.launch(mode, "2.1.296").expect("launch");
        assert!(matches!(
            running.receive_after_accepted().event,
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
    let parent = SessionConfigDir::create(&std::env::temp_dir()).expect("owned parent");
    // Exceed Unix sockaddr_un limits regardless of the runner's TMPDIR. The
    // fixture must bind successfully before it can emit the forbidden init.
    let long_parent = parent.path().join("long-temp-parent-".repeat(7));
    fs::create_dir(&long_parent).expect("long fixture parent");
    for mode in [
        "init-socket-string",
        "init-socket-null",
        "init-socket-wrong-type",
    ] {
        let fixture = Fixture::under(&long_parent);
        let mut running = fixture.launch(mode, "2.1.296").expect("launch");
        running.receive_after_accepted();
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
        running.receive_after_accepted();
        if active {
            running.submit();
            running.receive_after_accepted();
            running.receive_after_accepted();
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
    running.receive_after_accepted();
    let root = fixture.identity("model.pid");
    running.submit();
    running.receive_after_accepted();
    running.receive_after_accepted();
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
    running.receive_after_accepted();
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
    running.receive_after_accepted();
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
    running.receive_after_accepted();
    running.submit();
    for _ in 0..3 {
        running.receive_after_accepted();
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
        crate::structured::lease::SessionLeases::default()
            .reserve("fixture-agent", SESSION, None)
            .expect("lease"),
        StreamLocation {
            data_dir: fixture.root.path().to_owned(),
            storage: Default::default(),
            agent: "fixture-agent".into(),
            session: SESSION.into(),
        },
        fixture.mcp(),
        |_| panic!("owned publication failure"),
    )
    .expect("worker result");
    assert_eq!(exit.outcome, Outcome::Failed, "{exit:?}");
    assert!(exit.status.is_some());
    assert!(exit.error.expect("error").contains("publication panicked"));
    assert!(generated.exists());
}

#[test]
fn lease_is_released_after_model_startup_failure() {
    let fixture = Fixture::new();
    assert!(fixture.launch("normal", "remove").is_err());
    assert!(fixture
        .leases
        .reserve("fixture-agent", SESSION, None)
        .is_ok());
}

#[test]
fn lease_is_released_after_a_crash_with_confirmed_cleanup() {
    let fixture = Fixture::new();
    let mut running = fixture.launch("missing-result", "2.1.296").unwrap();
    running.receive_after_accepted();
    running.submit();
    let exit = running.finish();
    assert_eq!(exit.outcome, Outcome::Crashed, "{exit:?}");
    assert!(fixture
        .leases
        .reserve("fixture-agent", SESSION, None)
        .is_ok());
}

#[test]
fn lease_is_released_after_a_requested_stop_with_confirmed_cleanup() {
    let fixture = Fixture::new();
    let mut running = fixture.launch("normal", "2.1.296").unwrap();
    running.receive_after_accepted();
    running.runtime.stop();
    let exit = running.finish();
    assert_eq!(exit.outcome, Outcome::Stopped, "{exit:?}");
    assert!(fixture
        .leases
        .reserve("fixture-agent", SESSION, None)
        .is_ok());
}

#[test]
fn lease_profile_and_stream_must_name_the_same_agent_and_session() {
    for (agent, session) in [("other-agent", SESSION), ("fixture-agent", "other-session")] {
        let fixture = Fixture::new();
        let lease = fixture.leases.reserve(agent, session, None).unwrap();
        let error = run(
            fixture.root.path(),
            fixture.profile("idle", "2.1.296"),
            lease,
            StreamLocation {
                data_dir: fixture.root.path().to_owned(),
                storage: Default::default(),
                agent: "fixture-agent".into(),
                session: session.into(),
            },
            fixture.mcp(),
            |_| panic!("mismatched identity must never publish"),
        )
        .expect_err("mismatch");
        assert!(
            error.to_string().contains("lease/stream mismatch"),
            "{error}"
        );
        assert!(fixture.leases.reserve(agent, session, None).is_ok());
    }
}

#[test]
fn session_1441_fake_harness_receives_owned_flock_mcp_config() {
    let fixture = Fixture::new();
    let mut running = fixture.launch("mcp-config", "2.1.296").expect("launch");
    running.submit();
    let exit = running.finish();
    assert_eq!(
        exit.status.expect("reaped status").code(),
        Some(0),
        "{exit:?}"
    );
    let config: Value = serde_json::from_slice(
        &fs::read(fixture.root.path().join("observed-mcp.json")).expect("observed MCP"),
    )
    .unwrap();
    assert_eq!(config["mcpServers"].as_object().unwrap().len(), 1);
    assert!(!running.generated.exists());
}

#[test]
fn scripted_stream_drives_session_state_through_the_keyed_sink() {
    use crate::api::agent_state::{Phase, Turn};
    use crate::structured::claude::SessionEvent;
    use crate::structured::state::{SessionState, StateSink, Update};
    struct Sink(Vec<Update>);
    impl StateSink for Sink {
        fn publish(&mut self, update: Update) {
            self.0.push(update);
        }
    }
    let fixture = Fixture::new();
    let catalogue: Vec<Value> = include_str!("../../../tests/fixtures/claude-session/events.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let start = catalogue
        .iter()
        .find(|e| e["state"] == "started")
        .unwrap()
        .clone();
    let mut permission = catalogue
        .iter()
        .find(|e| e["type"] == "control_request")
        .unwrap()
        .clone();
    permission["request_id"] = "p1".into();
    let mut second = permission.clone();
    second["request_id"] = "p2".into();
    let mut limit = catalogue
        .iter()
        .find(|e| e["type"] == "rate_limit_event")
        .unwrap()
        .clone();
    limit["rate_limit_info"]["status"] = "rejected".into();
    let result = catalogue
        .iter()
        .find(|e| e["type"] == "result")
        .unwrap()
        .clone();
    // Extra init/hook traffic must not change which state transitions are asserted.
    let extra_init = catalogue
        .iter()
        .find(|e| e["type"] == "system" && e["subtype"] == "init")
        .unwrap()
        .clone();
    let extra_hook = catalogue
        .iter()
        .find(|e| e["subtype"] == "hook_response")
        .unwrap()
        .clone();
    let lines = [
        extra_init, start, permission, extra_hook, second, limit, result,
    ]
    .iter()
    .map(|e| format!("{e}\n"))
    .collect::<String>();
    fixture.file("state-events.jsonl", lines.as_bytes(), 0o600);
    let mut running = fixture.launch("state-reducer", "2.1.296").expect("launch");
    running.receive_after_accepted();
    running.submit();
    let now = Instant::now();
    let mut reducer = SessionState::new("fixture-agent".into());
    let mut sink = Sink(Vec::new());
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stage = 0;
    loop {
        let received = running
            .runtime
            .receive_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|error| {
                panic!(
                    "state stream stage {stage}: {error}; worker: {:?}",
                    running.result.try_recv()
                )
            });
        let Some(input) = received.state_input() else {
            let audit: Value = serde_json::from_str(&received.record.raw).unwrap();
            assert_eq!(audit["type"], "flk_permission_request");
            assert_eq!(audit["state"], "open");
            assert!(stage == 2 || stage == 3, "audit at stage {stage}");
            assert_eq!(audit["request_id"], format!("p{}", stage - 1));
            continue;
        };
        let accepted = matches!(&input, crate::structured::state::Input::Accepted { .. });
        reducer.publish(
            input,
            now,
            received.record.received_at_ms.try_into().unwrap(),
            Duration::from_secs(1),
            &mut sink,
        );
        let update = sink.0.last().unwrap();
        if accepted {
            assert_eq!(stage, 0, "admission must precede started");
            continue;
        }
        match received.event {
            SessionEvent::Init(_)
            | SessionEvent::HookStarted(_)
            | SessionEvent::HookResponse(_) => continue,
            SessionEvent::CommandLifecycle(e) if e.state == "started" => {
                assert_eq!(stage, 0);
                assert_eq!(update.state.turn, Turn::Working);
                assert_eq!(update.phase_changed, Some((Phase::Unknown, Phase::Working)));
            }
            SessionEvent::CanUseTool(e) => {
                assert!(
                    stage == 1 || stage == 2,
                    "unexpected request at stage {stage}"
                );
                assert_eq!(e.request_id, format!("p{stage}"));
                assert_eq!(update.state.phase, Phase::NeedsHuman);
                assert_eq!(update.needs_human.len(), stage);
                if stage == 1 {
                    assert_eq!(
                        update.phase_changed,
                        Some((Phase::Working, Phase::NeedsHuman))
                    );
                }
            }
            SessionEvent::RateLimit(_) => {
                assert_eq!(stage, 3);
                assert_eq!(update.state.phase, Phase::LimitReached);
                assert_eq!(update.needs_human.len(), 2);
            }
            SessionEvent::Success(_) => {
                assert_eq!(stage, 4);
                assert_eq!(update.state.phase, Phase::LimitReached);
                assert_eq!(update.state.turn, Turn::Unknown);
                assert!(update.needs_human.is_empty());
                break;
            }
            other => panic!("unexpected state stream event: {other:?}"),
        }
        stage += 1;
    }
    assert!(sink.0.iter().all(|u| u.agent_id == "fixture-agent"));
    running.runtime.stop();
    assert_eq!(running.finish().outcome, Outcome::Stopped);
}

mod queue;
#[test]
fn accepted_stdin_then_silent_harness_reports_stall_without_started() {
    use crate::structured::claude::SessionEvent;
    use crate::structured::state::{Input, SessionState};
    let fixture = Fixture::new();
    let mut running = fixture.launch("s4-accepted-silent", "2.1.296").unwrap();
    let ready = running.receive_after_accepted();
    assert!(matches!(ready.event, SessionEvent::HookStarted(_)));
    running
        .runtime
        .try_submit(&serde_json::json!({
            "type":"user", "message":{"role":"user", "content":"hello"}
        }))
        .expect("transport accepts input");
    let received = running
        .runtime
        .receive_timeout(Duration::from_secs(10))
        .unwrap_or_else(|error| {
            panic!(
                "accepted input missing: {error}; worker: {:?}",
                running.result.try_recv()
            )
        });
    let Some(Input::Accepted { at, at_ms }) = received.state_input() else {
        panic!("expected host accepted-input fact: {received:?}");
    };
    // Confirm that the fixture consumed stdin and stays alive without stdout.
    running.wait(|| {
        fixture
            .root
            .path()
            .join("input-received")
            .exists()
            .then_some(())
    });
    assert!(running.result.try_recv().is_err());
    assert!(matches!(
        running.runtime.try_receive(),
        Err(mpsc::TryRecvError::Empty)
    ));
    let policy = Duration::from_millis(250);
    let mut reducer = SessionState::new("fixture-agent".into());
    let accepted = reducer.reduce(received.state_input().unwrap(), at, at_ms, policy);
    assert_eq!(accepted.observe_at, Some(at + policy));
    let update = reducer.reduce(Input::Observe, at + policy, at_ms + 250, policy);
    assert_eq!(update.stalled_since_ms, Some(at_ms));
    assert_eq!(update.state.turn, crate::api::agent_state::Turn::Unknown);
    running.runtime.stop();
    running.finish();
}

#[test]
fn io_completion_cancellation_does_not_turn_a_rejected_submit_into_stop() {
    let fixture = Fixture::new();
    let mut running = fixture.launch("normal", "2.1.296").unwrap();
    running.receive_after_accepted();
    let child = fixture.identity("model.pid");
    // Reproduce the cancel-without-stop window opened by the I/O time limit.
    // Closing stdin lets this fixture exit naturally, without any stop intent.
    running.runtime.control.cancel();
    let error = running
        .runtime
        .try_submit(&serde_json::json!({"type":"user"}))
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert!(
        !running.runtime.stop.requested(),
        "I/O cancellation is not a stop"
    );
    assert_eq!(running.finish().outcome, Outcome::Exited);
    assert!(!child.alive());
}

#[test]
fn registry_launch_metadata_reaches_the_child_and_events_wake_the_owner() {
    let fixture = Fixture::new();
    let cwd = fixture.root.path().join("project");
    fs::create_dir(&cwd).unwrap();
    let profile = fixture
        .profile("idle", "2.1.296")
        .for_agent(&cwd, Some("fixture-requested-model"))
        .unwrap();
    fixture.file(
        "argv.json",
        &serde_json::to_vec(
            &profile
                .args()
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
        )
        .unwrap(),
        0o600,
    );
    fixture.file(
        "case.json",
        &serde_json::to_vec(&serde_json::json!({
            "mode":"idle", "version":"2.1.296", "cwd":cwd, "model":"fixture-requested-model"
        }))
        .unwrap(),
        0o600,
    );
    let settings_index = profile
        .args()
        .iter()
        .position(|arg| arg == "--settings")
        .unwrap();
    let generated = PathBuf::from(&profile.args()[settings_index + 1])
        .parent()
        .unwrap()
        .to_owned();
    let persistent = profile.config_dir().to_owned();
    let state = fixture.root.path().to_owned();
    let location = StreamLocation {
        data_dir: state.clone(),
        storage: Default::default(),
        agent: "fixture-agent".into(),
        session: SESSION.into(),
    };
    let lease = fixture
        .leases
        .reserve("fixture-agent", SESSION, None)
        .unwrap();
    assert!(fixture.leases.is_reserved(SESSION).unwrap());
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let (wake_tx, wake_rx) = mpsc::sync_channel(1);
    let mcp = fixture.mcp();
    let worker = std::thread::spawn(move || {
        let result = run_notified(
            &state,
            profile,
            lease,
            location,
            mcp,
            |runtime| {
                ready_tx.try_send(runtime).ok();
            },
            move || {
                wake_tx.try_send(()).ok();
            },
        );
        result_tx.try_send(result).ok();
    });
    let runtime = ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let mut running = Running {
        runtime,
        result: result_rx,
        worker: Some(worker),
        finished: false,
        generated,
        persistent,
    };
    wake_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("queued stdout must wake its owner");
    let received = running
        .runtime
        .try_receive()
        .expect("notification follows event publication");
    assert!(matches!(
        received.event,
        crate::structured::claude::SessionEvent::HookStarted { .. }
    ));
    let result = running.finish();
    assert_eq!(result.outcome, Outcome::Exited, "{result:?}");
    assert!(
        !fixture.leases.is_reserved(SESSION).unwrap(),
        "reaped runtime releases the lease"
    );
}

#[test]
fn launch_metadata_refuses_invalid_cwd_or_empty_model() {
    let fixture = Fixture::new();
    for (cwd, model) in [
        (PathBuf::from("fixture-relative"), None),
        (fixture.root.path().join("missing"), None),
        (fixture.root.path().to_owned(), Some(" ")),
    ] {
        let result = fixture.profile("idle", "2.1.296").for_agent(&cwd, model);
        assert!(matches!(result, Err(LaunchError::Io(_))));
    }
}

#[tokio::test]
async fn queue_stdin_failure_requests_stop_even_without_io_cancellation() {
    use crate::structured::{
        registry::Registry,
        workers::{
            tests::{wait, Case},
            Workers,
        },
    };
    let notify = std::sync::Arc::new(tokio::sync::Notify::new());
    let mut registry = Registry::default();
    let mut workers = Workers::new(notify.clone());
    let case = Case::start(&mut registry, &mut workers, 245, "no-read", vec![]);
    case.ready(&mut registry, &mut workers, &notify, Instant::now())
        .await;
    let runtime = workers.runtime(&case.id, case.generation).unwrap();
    runtime.control.close_stdin();
    assert!(
        !runtime.control.cancelled(),
        "the transport is closed, not cancelled"
    );
    case.submit(&workers);
    let mut refused = false;
    wait(&notify, || {
        workers.poll(&mut registry, Instant::now(), 1000);
        while let Some(update) = workers.queue_update(&registry, &case.id, case.generation) {
            refused |= update
                .offer
                .as_ref()
                .err()
                .is_some_and(|e| e.code == crate::api::error_code::ErrorCode::InputWriteFailed);
        }
        workers.report(&case.id, case.generation).is_some()
    })
    .await;
    assert!(refused);
    assert_eq!(
        workers
            .report(&case.id, case.generation)
            .unwrap()
            .result
            .as_ref()
            .unwrap()
            .outcome,
        Outcome::Stopped
    );
    workers.shutdown(&mut registry).unwrap();
}
