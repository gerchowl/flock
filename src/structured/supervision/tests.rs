use super::*;
use crate::process::TracedCommand;
use crate::structured::launch::ProfileConfigDir;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::process::Stdio;
use std::sync::mpsc;

fn wait<T>(mut poll: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(value) = poll() {
            return value;
        }
        assert!(Instant::now() < deadline, "owned fixture exceeded deadline");
        std::thread::sleep(TICK);
    }
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
        let child = TracedCommand::new("python3", "structured-fixture")
            .arg(script)
            .arg(self.root.path())
            .arg(mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn_traced()
            .expect("owned child");
        wait(|| self.root.path().join("ready").exists().then_some(()));
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
    let grandchild = wait(|| {
        fs::read_to_string(fixture.root.path().join("grandchild"))
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
    });
    let descendant = ProcessIdentity::capture(grandchild).expect("grandchild identity");
    let stdin = child.stdin.take();
    let stop = StopRequest::default();
    let worker_stop = stop.clone();
    let check_stop = stop.clone();
    let profile = fixture.profile.clone();
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        tx.send(supervise(child, files, profile, worker_stop, || {
            assert!(check_stop.requested());
            drop(stdin);
        }))
        .expect("report")
    });
    wait(|| {
        Owner::read(&path)
            .ok()
            .filter(|o| o.tree.contains(&descendant))
            .map(|_| ())
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
    let grandchild = wait(|| {
        fs::read_to_string(fixture.root.path().join("grandchild"))
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
    });
    let descendant = ProcessIdentity::capture(grandchild).expect("grandchild");
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
        ))
        .expect("report")
    });
    wait(|| {
        Owner::read(&path)
            .ok()
            .filter(|o| o.tree.contains(&descendant))
            .map(|_| ())
    });
    fs::write(fixture.root.path().join("crash"), b"crash").expect("crash only owned fixture");
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
    let report = supervise(child, files, fixture.profile.clone(), stop, || drop(stdin));
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
    fs::write(fixture.root.path().join("finish"), b"finish").expect("finish owned fixture");
    let report = supervise(
        child,
        files,
        fixture.profile.clone(),
        StopRequest::default(),
        || drop(stdin),
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
    fs::write(fixture.root.path().join("finish"), b"finish").expect("finish");
    wait(|| child.try_wait().expect("owned wait").map(|_| ()));
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
        uncertain: false,
        profile: Some(fixture.profile.clone()),
        tree: vec![identity],
    };
    owner.write(&dir).expect("dead owner record");
    files.retain();
    drop(files);
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
