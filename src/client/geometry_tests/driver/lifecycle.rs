//! Cleanup ownership and deterministic select regressions for the test driver.
use super::{
    accept_or_stop, finish_pty_child_with_timeout, geometry_fixture_dir, DRIVER_FINISH_CAP,
};

type Child = Box<dyn portable_pty::Child + Send + Sync>;

/// Arm before spawning. Every successful spawn is adopted before reader setup,
/// so an early error or panic follows the normal bounded kill/reap path.
pub(super) struct ProbeCleanup<'a> {
    child: &'a mut Option<Child>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}
impl<'a> ProbeCleanup<'a> {
    pub(super) fn new(
        child: &'a mut Option<Child>,
        stop: tokio::sync::oneshot::Sender<()>,
    ) -> Self {
        Self {
            child,
            stop: Some(stop),
        }
    }
    pub(super) fn adopt(&mut self, child: Child) {
        *self.child = Some(child);
    }
    pub(super) fn as_mut(&mut self) -> &mut (dyn portable_pty::Child + Send + Sync + 'static) {
        self.child
            .as_deref_mut()
            .expect("fixture child was spawned")
    }
    fn stop_server(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
    pub(super) fn finish(
        &mut self,
        cancel: bool,
        cap: std::time::Duration,
    ) -> portable_pty::ExitStatus {
        let status = finish_pty_child_with_timeout(
            self.child.take().expect("fixture child was spawned"),
            cancel,
            cap,
        );
        self.stop_server();
        status
    }
}
impl Drop for ProbeCleanup<'_> {
    fn drop(&mut self) {
        self.stop_server();
        if let Some(child) = self.child.take() {
            finish_pty_child_with_timeout(child, true, DRIVER_FINISH_CAP);
        }
    }
}

#[test]
fn ready_connection_wins_a_simultaneous_stop_after_peer_exit() {
    use std::os::unix::net::{UnixListener, UnixStream};
    let root = geometry_fixture_dir();
    let socket = root.path().join("s");
    let listener = UnixListener::bind(&socket).unwrap();
    let client = UnixStream::connect(&socket).unwrap();
    let (peer, _) = listener.accept().unwrap();
    drop(client);
    let (stop, stopped) = tokio::sync::oneshot::channel();
    stop.send(()).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let mut peer = rt
        .block_on(accept_or_stop(std::future::ready(Ok(peer)), stopped))
        .unwrap();
    let mut buffer = [0; 1];
    assert_eq!(std::io::Read::read(&mut peer, &mut buffer).unwrap(), 0);
}

#[test]
fn dropped_stop_sender_leaves_pending_accept_active() {
    use std::future::{poll_fn, Future};
    use std::task::Poll;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    drop(stop);
    let (send_accept, accepted) = tokio::sync::oneshot::channel();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(async {
        let pending = accept_or_stop(async { accepted.await.unwrap() }, stopped);
        let mut pending = std::pin::pin!(pending);
        assert!(
            poll_fn(|cx| Poll::Ready(pending.as_mut().poll(cx).is_pending())).await,
            "a dropped sender must not stop the listener"
        );
        send_accept.send(Ok(())).unwrap();
        pending.await.unwrap();
    });
}

#[test]
fn setup_panic_reaps_the_owned_child_and_explicitly_stops_the_server() {
    use std::process::{Command, Stdio};
    let root = geometry_fixture_dir();
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "read line"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(|key| key.starts_with("FLOCK_")) {
            command.env_remove(key);
        }
    }
    let mut child = None;
    let (stop, mut stopped) = tokio::sync::oneshot::channel();
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut cleanup = ProbeCleanup::new(&mut child, stop);
        let spawned =
            crate::process::TracedCommand::from_command(command, "geometry-setup-failure")
                .spawn_traced()
                .unwrap();
        cleanup.adopt(Box::new(spawned));
        assert!(
            cleanup.as_mut().try_wait().unwrap().is_none(),
            "helper must be alive before setup fails"
        );
        std::fs::File::open(root.path().join("missing-reader")).unwrap();
    }));
    // Rescue through the still-owned handle if the regression control disables
    // cleanup. Even a failing control must leave no child behind.
    let reaped = child.is_none();
    if let Some(child) = child.take() {
        finish_pty_child_with_timeout(child, true, DRIVER_FINISH_CAP);
    }
    assert!(failure.is_err(), "the real reader setup must fail");
    assert!(
        reaped,
        "setup failure must consume the child through bounded cleanup"
    );
    assert!(
        matches!(stopped.try_recv(), Ok(())),
        "cleanup must explicitly stop the server"
    );
}
