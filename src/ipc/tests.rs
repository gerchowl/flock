use super::*;
use std::cell::RefCell;
use std::os::unix::{
    fs::symlink,
    net::{UnixListener, UnixStream},
};

fn fixture() -> (crate::test_support::TempDir, std::path::PathBuf) {
    let dir = crate::test_support::TempDir::socket();
    let path = dir.path().join("ipc.sock");
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
    // A platform may report ECONNREFUSED even when the path is not a socket.
    assert!(prepare_socket_path_with(
        &path,
        busy,
        |_| Err(io::ErrorKind::ConnectionRefused.into()),
        crate::platform::peer::effective_uid()
    )
    .is_err());
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
fn timeout_is_not_evidence_of_staleness() {
    let (_dir, path) = fixture();
    stale(&path);
    let result = prepare_socket_path_with(
        &path,
        busy,
        |_| Err(io::ErrorKind::TimedOut.into()),
        crate::platform::peer::effective_uid(),
    );
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    assert!(path.exists());
}
#[test]
fn another_operators_stale_socket_is_not_removed() {
    let (_dir, path) = fixture();
    stale(&path);
    // Inject the expected operator, not an ambient user or a privileged chown.
    let other = crate::platform::peer::effective_uid().wrapping_add(1);
    assert!(prepare_socket_path_with(
        &path,
        busy,
        |_| panic!("foreign socket must not be probed"),
        other
    )
    .is_err());
    assert!(path.exists());
}
#[test]
fn replacement_during_probe_is_preserved_and_refused() {
    let (_dir, path) = fixture();
    // Keep the original inode allocated so the fixture cannot reuse its number.
    let original = UnixListener::bind(&path).unwrap();
    let replacement = RefCell::new(None);
    let result = prepare_socket_path_with(
        &path,
        busy,
        |path| {
            fs::remove_file(path).unwrap();
            replacement.replace(Some(UnixListener::bind(path).unwrap()));
            Err(io::ErrorKind::ConnectionRefused.into())
        },
        crate::platform::peer::effective_uid(),
    );
    assert!(
        result.is_err(),
        "a changed socket cannot be declared prepared"
    );
    assert!(UnixStream::connect(&path).is_ok());
    drop(original);
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
