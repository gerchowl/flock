use super::*;
use std::os::unix::{
    fs::symlink,
    net::{UnixListener, UnixStream},
};

fn fixture() -> (crate::test_support::TempDir, std::path::PathBuf) {
    let dir = crate::test_support::TempDir::socket();
    let path = dir.path().join("flock.sock");
    (dir, path)
}
fn stale(path: &Path) {
    drop(UnixListener::bind(path).unwrap());
}
fn busy(_: &Path) -> String {
    "fixture socket busy".into()
}
#[test]
fn regular_file_is_never_removed() {
    let (_dir, path) = fixture();
    fs::write(&path, "retain").unwrap();
    assert!(prepare_socket_path(&path, busy).is_err());
    assert_eq!(fs::read_to_string(path).unwrap(), "retain");
}
#[test]
fn stale_socket_symlink_is_never_followed_or_removed() {
    let (dir, path) = fixture();
    let target = dir.path().join("target.sock");
    stale(&target);
    symlink(&target, &path).unwrap();
    assert!(prepare_socket_path(&path, busy).is_err());
    assert!(fs::symlink_metadata(path).unwrap().file_type().is_symlink());
    assert!(target.exists());
}
#[test]
fn dangling_symlink_is_not_a_missing_socket() {
    let (dir, path) = fixture();
    symlink(dir.path().join("missing.sock"), &path).unwrap();
    assert!(prepare_socket_path(&path, busy).is_err());
    assert!(fs::symlink_metadata(path).is_ok());
}
#[test]
fn another_operators_stale_socket_is_not_removed() {
    let (_dir, path) = fixture();
    stale(&path);
    let metadata = fs::symlink_metadata(&path).unwrap();
    let other = crate::platform::peer::effective_uid().wrapping_add(1);
    assert!(require_owned_socket(&metadata, other).is_err());
    assert!(path.exists());
}
#[test]
fn cleanup_never_follows_a_symlink_to_its_socket() {
    let (dir, path) = fixture();
    let target = dir.path().join("target.sock");
    let _listener = UnixListener::bind(&target).unwrap();
    let identity = socket_file_identity(&target).unwrap();
    symlink(&target, &path).unwrap();
    let _ = remove_socket_file_if_owned(&path, identity);
    assert!(fs::symlink_metadata(path).is_ok());
    assert!(UnixStream::connect(target).is_ok());
}
#[test]
fn stale_operator_socket_is_removed() {
    let (_dir, path) = fixture();
    stale(&path);
    prepare_socket_path(&path, busy).unwrap();
    assert!(!path.exists());
}
#[test]
fn live_socket_is_busy_and_preserved() {
    let (_dir, path) = fixture();
    let _owner = prepare_socket_path(&path, busy).unwrap();
    let _listener = UnixListener::bind(&path).unwrap();
    assert_eq!(
        prepare_socket_path(&path, busy).unwrap_err().kind(),
        io::ErrorKind::AddrInUse
    );
    assert!(UnixStream::connect(path).is_ok());
}
#[test]
fn cleanup_preserves_a_replacement_socket() {
    let (_dir, path) = fixture();
    let original = UnixListener::bind(&path).unwrap();
    let identity = socket_file_identity(&path).unwrap();
    fs::remove_file(&path).unwrap();
    let _replacement = UnixListener::bind(&path).unwrap();
    remove_socket_file_if_owned(&path, identity).unwrap();
    assert!(UnixStream::connect(path).is_ok());
    drop(original);
}
#[test]
fn owned_socket_cleanup_and_mode_are_preserved() {
    let (_dir, path) = fixture();
    let _listener = UnixListener::bind(&path).unwrap();
    let identity = socket_file_identity(&path).unwrap();
    restrict_socket_permissions(&path, 0o600).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    remove_socket_file_if_owned(&path, identity).unwrap();
    assert!(!path.exists());
}

#[test]
fn socket_identity_refuses_a_symlink() {
    let (dir, path) = fixture();
    let target = dir.path().join("target.sock");
    let _listener = UnixListener::bind(&target).unwrap();
    symlink(&target, &path).unwrap();
    assert!(socket_file_identity(&path).is_err());
}

#[test]
fn lock_symlink_is_refused_and_mode_is_private() {
    let (dir, path) = fixture();
    let target = dir.path().join("target");
    fs::write(&target, "retain").unwrap();
    let lock = path.with_extension("sock.lock");
    symlink(&target, &lock).unwrap();
    assert!(SocketOwner::acquire(&path).is_err());
    assert_eq!(fs::read_to_string(&target).unwrap(), "retain");
    fs::remove_file(&lock).unwrap();
    let _owner = SocketOwner::acquire(&path).unwrap();
    assert_eq!(
        fs::metadata(lock).unwrap().permissions().mode() & 0o777,
        0o600
    );
    use std::os::fd::AsRawFd;
    assert_ne!(
        unsafe { libc::fcntl(_owner.0.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
}

#[test]
fn two_concurrent_starters_have_exactly_one_winner() {
    let (_dir, path) = fixture();
    let start = std::sync::Arc::new(std::sync::Barrier::new(3));
    let finish = start.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let mut workers = Vec::new();
    for _ in 0..2 {
        let (path, start, finish, tx) = (path.clone(), start.clone(), finish.clone(), tx.clone());
        workers.push(std::thread::spawn(move || {
            start.wait();
            let owner = prepare_socket_path(&path, busy);
            tx.send(owner.is_ok()).unwrap();
            finish.wait();
            drop(owner);
        }));
    }
    start.wait();
    let results: Vec<_> = (0..2)
        .map(|_| rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap())
        .collect();
    finish.wait();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(results.iter().filter(|&&won| won).count(), 1);
}

struct OwnerChild(std::process::Child);
impl Drop for OwnerChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[test]
fn crashed_owner_releases_ownership_and_stale_socket_is_reclaimed() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    let (_dir, path) = fixture();
    let mut command = crate::process::TracedCommand::new(
        std::env::current_exe().unwrap(),
        "socket-owner-fixture",
    );
    command.args([
        "--exact",
        "ipc::tests::socket_owner_child",
        "--ignored",
        "--nocapture",
    ]);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("FLOCK_") {
            command.env_remove(key);
        }
    }
    command
        .env("FLOCK_TEST_SOCKET_OWNER_PATH", &path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = OwnerChild(command.spawn_traced().unwrap());
    let output = child.0.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            if line.unwrap().contains("OWNER_READY") {
                let _ = tx.send(());
                return;
            }
        }
    });
    rx.recv_timeout(std::time::Duration::from_secs(5))
        .expect("owner died or stayed silent before binding");
    assert_eq!(
        SocketOwner::acquire(&path).unwrap_err().kind(),
        io::ErrorKind::AddrInUse
    );
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    assert!(path.exists(), "crash must leave the pathname behind");
    let _owner = prepare_socket_path(&path, busy).unwrap();
    assert!(!path.exists());
    let _replacement = UnixListener::bind(&path).unwrap();
}
#[test]
#[ignore = "exec fixture driven by crashed_owner_releases_ownership_and_stale_socket_is_reclaimed"]
fn socket_owner_child() {
    use std::io::{Read, Write};
    let path = std::path::PathBuf::from(std::env::var_os("FLOCK_TEST_SOCKET_OWNER_PATH").unwrap());
    let _owner = prepare_socket_path(&path, busy).unwrap();
    let _listener = UnixListener::bind(&path).unwrap();
    std::io::stdout().write_all(b"OWNER_READY\n").unwrap();
    std::io::stdout().flush().unwrap();
    let _ = std::io::stdin().read(&mut [0]);
}
