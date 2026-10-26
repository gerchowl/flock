use super::*;
use crate::test_support::TempDir;
use axum::{extract::ConnectInfo, routing::get};
use std::{future::Future, os::fd::AsRawFd, os::unix::fs::PermissionsExt, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn bounded<T>(work: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), work)
        .await
        .expect("Unix HTTP producer stayed silent")
}
fn prepare(root: &TempDir, router: Router) -> Listener {
    Listener::prepare(socket_path(&root.path().join("a.sock")), None, router).unwrap()
}
async fn request(path: &Path) -> UnixStream {
    let mut socket = bounded(UnixStream::connect(path)).await.unwrap();
    bounded(socket.write_all(
        b"GET / HTTP/1.1\r\nHost: fixture.example\r\nX-Peer-Pid: 1\r\nConnection: close\r\n\r\n",
    ))
    .await
    .unwrap();
    socket
}
async fn response(mut socket: UnixStream) -> String {
    let mut text = String::new();
    bounded(socket.read_to_string(&mut text)).await.unwrap();
    text
}
#[test]
fn companion_names_preserve_distinct_api_overrides() {
    let root = TempDir::socket();
    assert_eq!(
        socket_path(&root.path().join("flock.sock")),
        root.path().join("flock-ws.sock")
    );
    assert_ne!(
        socket_path(&root.path().join("a.sock")),
        socket_path(&root.path().join("b.sock"))
    );
}
#[tokio::test]
async fn prepares_private_socket_without_serving_until_explicit_activation() {
    let root = TempDir::socket();
    let mut listener = prepare(
        &root,
        Router::new().route("/", get(|| async { "native-http" })),
    );
    assert_eq!(
        std::fs::metadata(&listener.path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let socket = request(&listener.path).await;
    assert!(
        listener.task.is_none(),
        "preparation must not start a serve task before handoff commit"
    );
    assert!(
        listener.prepared.is_some(),
        "the acceptor stays parked until explicit activation"
    );
    listener.activate();
    let text = response(socket).await;
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert!(text.ends_with("native-http"), "{text}");
    listener.stop().await;
}
#[tokio::test]
async fn http_receives_kernel_credentials_instead_of_request_headers() {
    let root = TempDir::socket();
    let mut listener = prepare(
        &root,
        Router::new().route(
            "/",
            get(|ConnectInfo(peer): ConnectInfo<Credentials>| async move {
                axum::Json(serde_json::json!({"pid":peer.pid,"uid":peer.uid,"birth":peer.birth}))
            }),
        ),
    );
    listener.activate();
    let text = response(request(&listener.path).await).await;
    let (_, body) = text.split_once("\r\n\r\n").unwrap();
    let peer: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(peer["pid"], std::process::id());
    assert_eq!(peer["uid"], crate::platform::peer::effective_uid());
    assert_eq!(
        peer["birth"],
        crate::platform::process_start_time(std::process::id()).unwrap()
    );
    listener.stop().await;
}
#[tokio::test]
async fn ownership_excludes_second_listener_and_cleanup_releases_only_its_binding() {
    let root = TempDir::socket();
    let listener = prepare(&root, Router::new());
    let path = listener.path.clone();
    let identity = crate::ipc::socket_file_identity(&path).unwrap();
    let error = Listener::prepare(path.clone(), None, Router::new())
        .err()
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
    assert_eq!(crate::ipc::socket_file_identity(&path).unwrap(), identity);
    drop(listener);
    assert!(!path.exists());
    assert!(path.with_extension("sock.lock").exists());
    let _replacement = Listener::prepare(path, None, Router::new()).unwrap();
}
#[tokio::test]
async fn cleanup_does_not_unlink_a_replacement_socket() {
    let root = TempDir::socket();
    let listener = prepare(&root, Router::new());
    let path = listener.path.clone();
    listener.unlink().unwrap();
    let replacement = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let identity = crate::ipc::socket_file_identity(&path).unwrap();
    drop(listener);
    assert_eq!(crate::ipc::socket_file_identity(&path).unwrap(), identity);
    drop(replacement);
}
#[tokio::test]
async fn unsafe_path_entries_are_refused_without_modification() {
    let root = TempDir::socket();
    let path = socket_path(&root.path().join("a.sock"));
    let target = root.path().join("keep");
    std::fs::write(&target, "preserved").unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert_eq!(
        Listener::prepare(path.clone(), None, Router::new())
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    assert!(std::fs::symlink_metadata(&path)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(std::fs::read_to_string(target).unwrap(), "preserved");
}
#[tokio::test]
async fn transferred_owner_retains_exclusivity_through_import_and_rollback() {
    let root = TempDir::socket();
    let mut exporter = prepare(&root, Router::new());
    exporter.activate();
    let path = exporter.path.clone();
    let retained = exporter.owner();
    let descriptor = retained.duplicate().unwrap();
    let (send, receive) = std::os::unix::net::UnixStream::pair().unwrap();
    receive
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    crate::server::handoff::send_fds(&send, &[descriptor.as_raw_fd()]).unwrap();
    let mut descriptors = crate::server::handoff::recv_fds(&receive, 1).unwrap();
    let imported = SocketOwner::take(&path, &mut descriptors).unwrap();
    assert!(descriptors.is_empty());
    drop(descriptor);
    exporter.unlink().unwrap();
    let importer = Listener::prepare(path.clone(), Some(imported), Router::new()).unwrap();
    exporter.stop().await;
    drop(exporter);
    assert!(
        path.exists(),
        "exporter must not remove the importer's new binding"
    );
    assert_eq!(
        SocketOwner::acquire(&path).unwrap_err().kind(),
        io::ErrorKind::AddrInUse
    );
    drop(importer);
    assert_eq!(
        SocketOwner::acquire(&path).unwrap_err().kind(),
        io::ErrorKind::AddrInUse
    );
    let rollback = Listener::prepare(path.clone(), Some(retained), Router::new()).unwrap();
    drop(rollback);
    assert!(!path.exists());
    let _next = SocketOwner::acquire(&path).unwrap();
}
#[tokio::test]
async fn stopping_acceptance_cancels_an_inflight_http_handler() {
    struct Finished(Arc<tokio::sync::Notify>);
    impl Drop for Finished {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }
    let root = TempDir::socket();
    let entered = Arc::new(tokio::sync::Notify::new());
    let finished = Arc::new(tokio::sync::Notify::new());
    let mut listener = prepare(
        &root,
        Router::new().route(
            "/",
            get({
                let entered = entered.clone();
                let finished = finished.clone();
                move || {
                    let entered = entered.clone();
                    let finished = finished.clone();
                    async move {
                        let _finished = Finished(finished);
                        entered.notify_one();
                        std::future::pending::<&'static str>().await
                    }
                }
            }),
        ),
    );
    listener.activate();
    let socket = request(&listener.path).await;
    bounded(entered.notified()).await;
    listener.stop().await;
    // Cancellation must finish before the independent five-second HTTP deadline.
    // Two seconds gives executor cleanup room while detecting a detached live handler.
    tokio::time::timeout(Duration::from_secs(2), finished.notified())
        .await
        .expect("stopped listener left its HTTP handler alive");
    assert!(response(socket).await.is_empty());
    assert_eq!(
        SocketOwner::acquire(&listener.path).unwrap_err().kind(),
        io::ErrorKind::AddrInUse
    );
}

#[tokio::test]
async fn unsafe_parent_is_refused_before_creating_any_path() {
    let root = TempDir::socket();
    let directory = root.path().join("unsafe");
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o777)).unwrap();
    let path = directory.join("native.sock");
    assert_eq!(
        Listener::prepare(path.clone(), None, Router::new())
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert!(!path.exists());
    assert!(std::fs::read_dir(&directory).unwrap().next().is_none());
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let alias = root.path().join("alias");
    std::os::unix::fs::symlink(&directory, &alias).unwrap();
    assert!(Listener::prepare(alias.join("native.sock"), None, Router::new()).is_err());
    assert!(std::fs::read_dir(directory).unwrap().next().is_none());
}

#[tokio::test]
async fn inherited_owner_reclaims_a_stale_socket_before_binding() {
    let root = TempDir::socket();
    let path = root.path().join("native.sock");
    let owner = SocketOwner::acquire(&path).unwrap();
    let abandoned = std::os::unix::net::UnixListener::bind(&path).unwrap();
    drop(abandoned);
    assert!(
        path.exists(),
        "the abandoned binding is the real input to prepare"
    );
    let mut importer = Listener::prepare(
        path.clone(),
        Some(owner),
        Router::new().route("/", get(|| async { "imported" })),
    )
    .unwrap();
    assert!(importer.task.is_none());
    assert_eq!(
        SocketOwner::acquire(&path).unwrap_err().kind(),
        io::ErrorKind::AddrInUse
    );
    importer.activate();
    assert!(response(request(&path).await).await.ends_with("imported"));
    importer.stop().await;
    drop(importer);
    assert!(!path.exists());
}

#[tokio::test]
async fn inherited_owner_refuses_a_non_socket_without_changing_it() {
    let root = TempDir::socket();
    let path = root.path().join("native.sock");
    let owner = SocketOwner::acquire(&path).unwrap();
    std::fs::write(&path, "preserved stale file").unwrap();
    let error = Listener::prepare(path.clone(), Some(owner), Router::new())
        .err()
        .expect("inherited ownership does not authorize removing a regular file");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "preserved stale file"
    );
}

#[tokio::test]
async fn inherited_owner_refuses_a_different_path_before_creating_it() {
    let root = TempDir::socket();
    let owned_path = root.path().join("owned.sock");
    let requested_path = root.path().join("other.sock");
    let owner = SocketOwner::acquire(&owned_path).unwrap();
    let error = Listener::prepare(requested_path.clone(), Some(owner.clone()), Router::new())
        .err()
        .expect("a guard for another pathname cannot authorize this listener");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(!requested_path.exists());
    assert!(!requested_path.with_extension("sock.lock").exists());
    assert_eq!(
        SocketOwner::acquire(&owned_path).unwrap_err().kind(),
        io::ErrorKind::AddrInUse
    );
}
