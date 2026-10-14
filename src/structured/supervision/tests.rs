use super::*;
use crate::process::TracedCommand;
use crate::structured::launch::ProfileConfigDir;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::process::Stdio;
use std::sync::mpsc;

fn wait<T>(reports: &mpsc::Receiver<ExitReport>, mut poll: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(value) = poll() {
            return value;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        match reports.recv_timeout(TICK.min(remaining)) {
            Ok(report) => panic!("supervisor exited before condition: {report:?}"),
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("supervisor disconnected"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                assert!(
                    !remaining.is_zero(),
                    "condition deadline with live supervisor"
                );
            }
        }
    }
}

fn command(identity: ProcessIdentity, signal: libc::c_int) {
    assert!(identity.alive(), "owned fixture died before command");
    // SAFETY: this is a birth-checked process spawned by this test, never ambient.
    assert_eq!(
        unsafe { libc::kill(identity.pid as libc::pid_t, signal) },
        0
    );
}

struct Fixture {
    root: SessionConfigDir,
    profile: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = SessionConfigDir::create(&std::env::temp_dir()).expect("fixture root");
        let profile = ProfileConfigDir::for_operator(root.path(), "fixture")
            .expect("profile")
            .path()
            .to_owned();
        fs::create_dir(profile.join("sessions")).expect("registry");
        fs::write(profile.join("transcript.jsonl"), "persistent").expect("transcript");
        Self { root, profile }
    }
    fn child(&self, mode: &str) -> Child {
        let script = self.root.path().join("child.py");
        fs::write(
            &script,
            include_str!("../../../tests/fixtures/structured-supervision/child.py"),
        )
        .expect("fixture script");
        let mut child = TracedCommand::new("python3", "structured-fixture")
            .arg(script)
            .arg(self.root.path())
            .arg(mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .process_group(0)
            .spawn_traced()
            .expect("owned child");
        let stdout = child.stdout.take().expect("fixture readiness pipe");
        let (tx, rx) = mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            let mut line = String::new();
            tx.send(BufReader::new(stdout).read_line(&mut line).map(|_| line))
                .expect("readiness receiver");
        });
        let ready = rx
            .recv_timeout(Duration::from_secs(15))
            .unwrap_or_else(|error| {
                let status = child.try_wait().expect("owned child liveness");
                child.kill().expect("kill silent owned fixture");
                child.wait().expect("reap silent owned fixture");
                panic!("fixture readiness failed: {error}; child: {status:?}");
            })
            .expect("readiness read");
        reader.join().expect("readiness reader");
        assert_eq!(ready, "ready\n", "unexpected fixture output");
        child
    }
    fn files(&self) -> SessionConfigDir {
        let mut files = prepare_session_files(self.root.path()).expect("session files");
        arm_session_files(&mut files).expect("arm before spawn");
        files
    }
}

#[test]
fn stop_intent_precedes_close_and_reaps_detached_grandchild() {
    let fixture = Fixture::new();
    let files = fixture.files();
    let path = files.path().to_owned();
    let mut child = fixture.child("tree");
    let root = ProcessIdentity::capture(child.id()).expect("root identity");
    let grandchild = fs::read_to_string(fixture.root.path().join("grandchild"))
        .expect("descendant ready before root readiness")
        .parse::<u32>()
        .expect("descendant pid");
    let descendant = ProcessIdentity::capture(grandchild).expect("grandchild identity");
    let stdin = child.stdin.take();
    let stop = StopRequest::default();
    let worker_stop = stop.clone();
    let check_stop = stop.clone();
    let profile = fixture.profile.clone();
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        tx.send(supervise(
            child,
            files,
            profile,
            worker_stop,
            || {
                assert!(check_stop.requested());
                drop(stdin);
            },
            || Ok(()),
        ))
        .expect("report")
    });
    wait(&rx, || {
        Owner::read(&path)
            .expect("atomic owner record")
            .tree
            .contains(&descendant)
            .then_some(())
    });
    assert!(path.exists());
    stop.request();
    let report = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("bounded shutdown");
    worker.join().expect("supervisor");
    report.cleanup.expect("confirmed cleanup");
    assert!(report.stop_requested);
    assert!(report.status.is_some());
    assert!(!root.alive());
    assert!(!descendant.alive());
    assert!(!path.exists());
    assert!(fixture.profile.join("transcript.jsonl").exists());
}

#[test]
fn crash_path_uses_retained_snapshot_after_reparenting() {
    let fixture = Fixture::new();
    let files = fixture.files();
    let path = files.path().to_owned();
    let mut child = fixture.child("tree");
    let grandchild = fs::read_to_string(fixture.root.path().join("grandchild"))
        .expect("descendant ready before root readiness")
        .parse::<u32>()
        .expect("descendant pid");
    let descendant = ProcessIdentity::capture(grandchild).expect("grandchild");
    let root = ProcessIdentity::capture(child.id()).expect("root identity");
    let stdin = child.stdin.take();
    let profile = fixture.profile.clone();
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        tx.send(supervise(
            child,
            files,
            profile,
            StopRequest::default(),
            || drop(stdin),
            || Ok(()),
        ))
        .expect("report")
    });
    wait(&rx, || {
        Owner::read(&path)
            .expect("atomic owner record")
            .tree
            .contains(&descendant)
            .then_some(())
    });
    command(root, libc::SIGUSR2);
    let report = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("bounded crash cleanup");
    worker.join().expect("supervisor");
    report.cleanup.expect("reparented descendant gone");
    assert_eq!(report.status.expect("exit").code(), Some(7));
    assert!(!report.stop_requested);
    assert!(!descendant.alive());
    assert!(!path.exists());
}

#[test]
fn term_resistant_owned_child_escalates_to_kill_after_grace() {
    let fixture = Fixture::new();
    let files = fixture.files();
    let mut child = fixture.child("single");
    let identity = ProcessIdentity::capture(child.id()).expect("child identity");
    let stdin = child.stdin.take();
    let stop = StopRequest::default();
    stop.request();
    let begin = Instant::now();
    let report = supervise(
        child,
        files,
        fixture.profile.clone(),
        stop,
        || drop(stdin),
        || Ok(()),
    );
    report.cleanup.expect("KILL confirmed");
    assert!(report.status.is_some());
    assert!(begin.elapsed() >= EOF_GRACE + TERM_GRACE);
    assert!(begin.elapsed() < Duration::from_secs(5));
    assert!(fixture.root.path().join("term").exists());
    assert!(!identity.alive());
}

#[test]
fn natural_idle_exit_has_no_stop_intent_and_keeps_profile() {
    let fixture = Fixture::new();
    let files = fixture.files();
    let path = files.path().to_owned();
    let mut child = fixture.child("single");
    let stdin = child.stdin.take();
    command(
        ProcessIdentity::capture(child.id()).expect("owned child"),
        libc::SIGUSR1,
    );
    let report = supervise(
        child,
        files,
        fixture.profile.clone(),
        StopRequest::default(),
        || drop(stdin),
        || Ok(()),
    );
    report.cleanup.expect("cleanup");
    assert!(report.status.expect("exit").success());
    assert!(!report.stop_requested);
    assert!(!path.exists());
    assert!(fixture.profile.join("transcript.jsonl").exists());
}

#[test]
fn armed_guard_retains_files_on_unwind_and_sweep_preserves_live_owner() {
    let fixture = Fixture::new();
    let files = fixture.files();
    let path = files.path().to_owned();
    let result = std::panic::catch_unwind(|| {
        let _files = files;
        panic!("owned unwind fixture");
    });
    assert!(result.is_err());
    assert!(path.exists());
    assert_eq!(
        sweep_stale_sessions(fixture.root.path()).expect("live owner retained"),
        0
    );
    assert!(path.exists());
}

#[test]
fn stale_sweep_validates_birth_and_removes_only_dead_known_sessions() {
    let fixture = Fixture::new();
    let mut files = prepare_session_files(fixture.root.path()).expect("files");
    let path = files.path().to_owned();
    let current = ProcessIdentity::capture(std::process::id()).expect("fixture identity");
    let reused = ProcessIdentity {
        birth: current.birth.wrapping_add(1),
        ..current
    };
    assert!(!reused.alive());
    let owner = Owner {
        owner: reused,
        supervisor: reused,
        uncertain: false,
        profile: None,
        tree: vec![reused],
    };
    owner.write(&path).expect("fictional stale birth");
    files.retain();
    drop(files);
    assert_eq!(
        sweep_stale_sessions(fixture.root.path()).expect("stale sweep"),
        1
    );
    assert!(!path.exists());
    assert!(
        current.alive(),
        "same pid with different birth was never signalled"
    );
}

#[test]
fn stale_sweep_refuses_uncertain_malformed_and_linked_owners() {
    let fixture = Fixture::new();
    let mut files = fixture.files();
    let path = files.path().to_owned();
    let mut owner = Owner::read(&path).expect("owner");
    owner.owner.birth = owner.owner.birth.wrapping_add(1);
    owner.supervisor.birth = owner.supervisor.birth.wrapping_add(1);
    owner.write(&path).expect("dead uncertain owner");
    files.retain();
    drop(files);
    assert!(sweep_stale_sessions(fixture.root.path()).is_err());
    assert!(path.exists());
    fs::write(path.join("owner.json"), b"malformed").expect("malformed fixture");
    assert!(sweep_stale_sessions(fixture.root.path()).is_err());
    assert!(path.exists());
    fs::remove_file(path.join("owner.json")).expect("remove fixture record");
    symlink(
        fixture.root.path().join("child.py"),
        path.join("owner.json"),
    )
    .expect("fixture link");
    assert!(sweep_stale_sessions(fixture.root.path()).is_err());
    assert!(path.exists());
}

#[test]
fn transient_cleanup_is_scoped_and_preserves_persistent_profile() {
    let fixture = Fixture::new();
    let mut files = fixture.files();
    let dir = files.path().to_owned();
    let mut child = fixture.child("single");
    let identity = ProcessIdentity::capture(child.id()).expect("identity");
    command(identity, libc::SIGUSR1);
    child.wait().expect("owned child exited");
    let sockets = fixture.root.path().join("cc-socks");
    private_fs::create_private_dir(&sockets).expect("private sockets");
    let socket = sockets.join(format!("{}.sock", identity.pid));
    let listener = UnixListener::bind(&socket).expect("fixture socket");
    let registry = fixture
        .profile
        .join("sessions")
        .join(format!("{}.json", identity.pid));
    fs::write(
        &registry,
        serde_json::json!({"messagingSocketPath":socket}).to_string(),
    )
    .expect("registry");
    let owner = Owner {
        owner: identity,
        supervisor: identity,
        uncertain: false,
        profile: Some(fixture.profile.clone()),
        tree: vec![identity],
    };
    owner.write(&dir).expect("dead owner record");
    files.retain();
    drop(files);
    let original = fixture.profile.join("original-sessions");
    fs::rename(fixture.profile.join("sessions"), &original).expect("move registry fixture");
    symlink(&original, fixture.profile.join("sessions")).expect("linked registry parent");
    assert!(
        sweep_stale_sessions(fixture.root.path()).is_err(),
        "redirected registry refused"
    );
    assert!(original.join(format!("{}.json", identity.pid)).exists());
    assert!(
        socket.exists(),
        "no socket unlink through redirected registry"
    );
    assert!(dir.exists());
    fs::remove_file(fixture.profile.join("sessions")).expect("remove fixture link");
    fs::rename(original, fixture.profile.join("sessions")).expect("restore registry parent");
    assert_eq!(sweep_stale_sessions(fixture.root.path()).expect("sweep"), 1);
    assert!(!registry.exists());
    assert!(!socket.exists());
    assert!(!dir.exists());
    drop(listener);
    assert!(fixture.profile.join("transcript.jsonl").exists());
}

#[test]
fn unsafe_transients_and_reused_live_pids_are_never_unlinked() {
    let fixture = Fixture::new();
    let files = fixture.files();
    let identity = ProcessIdentity::capture(std::process::id()).expect("fixture identity");
    let registry = fixture
        .profile
        .join("sessions")
        .join(format!("{}.json", identity.pid));
    fs::write(&registry, b"{}").expect("registry");
    let reused = Owner {
        owner: ProcessIdentity {
            birth: identity.birth.wrapping_add(1),
            ..identity
        },
        supervisor: identity,
        uncertain: false,
        profile: Some(fixture.profile.clone()),
        tree: vec![],
    };
    assert!(cleanup_transient(&reused, files.path()).is_err());
    assert!(registry.exists());
    let unsafe_file = fixture.root.path().join("unrelated.sock");
    fs::write(&unsafe_file, b"untouched").expect("regular file");
    assert!(private_fs::remove_transient(&unsafe_file, Some(identity.pid)).is_err());
    assert_eq!(fs::read(unsafe_file).expect("preserved"), b"untouched");
    fs::set_permissions(files.path(), fs::Permissions::from_mode(0o755))
        .expect("unsafe session mode");
    assert!(sweep_stale_sessions(fixture.root.path()).is_err());
}

#[test]
fn stale_sweep_expands_a_surviving_recorded_parent_before_kill() {
    let fixture = Fixture::new();
    let mut files = fixture.files();
    let path = files.path().to_owned();
    let mut child = fixture.child("tree");
    let parent = ProcessIdentity::capture(child.id()).expect("owned parent");
    let grandchild = ProcessIdentity::capture(
        fs::read_to_string(fixture.root.path().join("grandchild"))
            .expect("ready descendant")
            .parse()
            .expect("pid"),
    )
    .expect("owned descendant");
    struct Cleanup(Owner);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.0.signal(Signal::Kill);
        }
    }
    let current = ProcessIdentity::capture(std::process::id()).expect("preparer");
    let dead = ProcessIdentity {
        birth: current.birth.wrapping_add(1),
        ..current
    };
    let _cleanup = Cleanup(Owner {
        owner: parent,
        supervisor: dead,
        uncertain: false,
        profile: None,
        tree: vec![parent, grandchild],
    });
    Owner {
        owner: parent,
        supervisor: dead,
        uncertain: false,
        profile: None,
        tree: vec![parent],
    }
    .write(&path)
    .expect("snapshot predates descendant");
    files.retain();
    drop(files);
    let (reaped_tx, reaped_rx) = mpsc::sync_channel(1);
    let reaper = std::thread::spawn(move || reaped_tx.send(child.wait()).expect("reap report"));
    let state = fixture.root.path().to_owned();
    let (tx, rx) = mpsc::sync_channel(1);
    let sweep =
        std::thread::spawn(move || tx.send(sweep_stale_sessions(&state)).expect("sweep report"));
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(10))
            .expect("sweep deadline")
            .expect("sweep"),
        1
    );
    reaped_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("reap deadline")
        .expect("owned child reaped");
    sweep.join().expect("sweep worker");
    reaper.join().expect("reaper");
    assert!(
        !grandchild.alive(),
        "sweep must discover and confirm newer descendants"
    );
    assert!(!path.exists());
}

#[test]
fn failed_io_finalization_retains_files_after_tree_is_reaped() {
    let fixture = Fixture::new();
    let files = fixture.files();
    let path = files.path().to_owned();
    let mut child = fixture.child("single");
    let identity = ProcessIdentity::capture(child.id()).expect("owned identity");
    let stdin = child.stdin.take();
    command(identity, libc::SIGUSR1);
    let report = supervise(
        child,
        files,
        fixture.profile.clone(),
        StopRequest::default(),
        || drop(stdin),
        || {
            assert!(!identity.alive(), "I/O finalization follows reaping");
            assert!(path.exists(), "generated files outlive I/O finalization");
            Err(io::Error::other("fixture reader failed"))
        },
    );
    assert!(report.status.expect("reaped").success());
    assert!(!report.stop_requested);
    assert_eq!(
        report
            .cleanup
            .expect_err("I/O failure retained")
            .to_string(),
        "fixture reader failed"
    );
    assert!(
        path.exists(),
        "failed finalization never releases generated files"
    );
    assert_eq!(
        sweep_stale_sessions(fixture.root.path()).expect("live supervisor preserved"),
        0
    );
    assert!(path.exists(), "sweep cannot bypass live I/O ownership");
}

#[test]
fn stale_sweep_refuses_a_linked_namespace_ancestor_without_deleting_files() {
    let fixture = Fixture::new();
    let mut files = prepare_session_files(fixture.root.path()).expect("files");
    let name = files.path().file_name().expect("session name").to_owned();
    let current = ProcessIdentity::capture(std::process::id()).expect("fixture identity");
    let dead = ProcessIdentity {
        birth: current.birth.wrapping_add(1),
        ..current
    };
    Owner {
        owner: dead,
        supervisor: dead,
        uncertain: false,
        profile: None,
        tree: vec![dead],
    }
    .write(files.path())
    .expect("fictional dead supervisor");
    files.retain();
    drop(files);
    let structured = fixture.root.path().join("structured");
    let moved = fixture.root.path().join("moved-structured");
    fs::rename(&structured, &moved).expect("move owned namespace");
    symlink(&moved, &structured).expect("linked namespace ancestor");
    assert!(
        sweep_stale_sessions(fixture.root.path()).is_err(),
        "redirected namespace refused"
    );
    assert!(moved
        .join("sessions")
        .join(&name)
        .join("owner.json")
        .exists());
    fs::remove_file(&structured).expect("remove namespace link");
    fs::rename(&moved, &structured).expect("restore owned namespace");
    assert_eq!(
        sweep_stale_sessions(fixture.root.path()).expect("valid sweep"),
        1
    );
}
