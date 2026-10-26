use super::*;
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    process::{Child, Stdio},
    time::Duration,
};

pub(crate) fn readable(fd: RawFd, timeout: Duration) -> bool {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: the single output buffer and borrowed fd live for this call.
    let result = unsafe { libc::poll(&mut p, 1, timeout.as_millis().try_into().unwrap()) };
    assert!(
        result >= 0,
        "kernel event wait failed: {}",
        io::Error::last_os_error()
    );
    result > 0
}
pub(crate) struct Peer {
    child: Child,
    _root: crate::test_support::TempDir,
    _stream: Option<UnixStream>,
    pub(crate) credentials: Credentials,
}
impl Peer {
    pub(crate) fn spawn() -> Self {
        Self::request(None)
    }
    pub(crate) fn request(request: Option<&str>) -> Self {
        let root = crate::test_support::TempDir::socket();
        let path = root.path().join("exit.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let mut command = crate::process::TracedCommand::new(
            std::env::current_exe().unwrap(),
            "app-peer-fixture",
        );
        command.args([
            "--exact",
            "platform::process_exit::tests::peer_child",
            "--nocapture",
        ]);
        command.env("FLOCK_TEST_PROCESS_EXIT_SOCKET", &path);
        if let Some(request) = request {
            command.env("FLOCK_TEST_PROCESS_EXIT_REQUEST", request);
        }
        for key in ["FLOCK_SOCKET_PATH", "FLOCK_PANE_ID", "FLOCK_AGENT_ID"] {
            command.env_remove(key);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut peer = Self {
            child: command.spawn_traced().unwrap(),
            _root: root,
            _stream: None,
            credentials: Credentials::default(),
        };
        assert!(
            readable(listener.as_raw_fd(), Duration::from_secs(5)),
            "peer failed to connect; child status {:?}",
            peer.child.try_wait()
        );
        let (stream, _) = listener.accept().unwrap();
        peer.credentials = crate::platform::peer::credentials(&stream);
        assert_eq!(peer.credentials.pid, Some(peer.child.id()));
        peer._stream = Some(stream);
        peer
    }
    #[cfg(feature = "web")]
    pub(crate) fn take_stream(&mut self) -> UnixStream {
        self._stream.take().unwrap()
    }
    pub(crate) fn exit(&mut self) {
        drop(self.child.stdin.take());
    }
    /// Observe this child's exit without reaping, preserving readable birth evidence.
    /// Kill only this child if the producer misses the five-second failure ceiling.
    pub(crate) fn exit_unreaped(&mut self) {
        self.exit();
        let pid = self.child.id();
        let (done, received) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            // SAFETY: this fixture owns pid; WNOWAIT observes exit without reaping.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let result =
                unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT) };
            let result = if result == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            };
            let _ = done.send(result);
        });
        let result = received.recv_timeout(Duration::from_secs(5));
        if result.is_err() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        waiter.join().unwrap();
        result
            .expect("peer must exit within the fail ceiling")
            .unwrap();
        assert!(self.credentials.birth.is_some());
        assert_eq!(
            crate::platform::process_start_time(pid),
            self.credentials.birth,
            "exit must leave the original birth readable until this fixture reaps it"
        );
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
#[test]
fn peer_child() {
    let Some(path) = std::env::var_os("FLOCK_TEST_PROCESS_EXIT_SOCKET") else {
        return;
    };
    let mut socket = UnixStream::connect(path).unwrap();
    if let Ok(request) = std::env::var("FLOCK_TEST_PROCESS_EXIT_REQUEST") {
        writeln!(socket, "{request}").unwrap();
        let mut reply = String::new();
        BufReader::new(&socket).read_line(&mut reply).unwrap();
    }
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input).unwrap();
}
#[test]
fn real_socket_peer_exit_is_kernel_ready_without_liveness_polling() {
    let mut peer = Peer::spawn();
    let watch = ExitWatch::arm(peer.credentials).unwrap();
    assert!(!watch.exited().unwrap());
    // SAFETY: F_GETFD reads flags for this owned descriptor.
    assert_ne!(
        unsafe { libc::fcntl(watch.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    peer.exit();
    assert!(
        readable(watch.as_raw_fd(), Duration::from_secs(5)),
        "live/silent peer did not exit within the fail ceiling"
    );
    assert!(watch.exited().unwrap());
    assert!(
        watch.exited().unwrap(),
        "exit evidence must stay readable for later consumers"
    );
}
#[test]
fn missing_mismatched_and_already_exited_identities_refuse() {
    let mut peer = Peer::spawn();
    assert!(ExitWatch::arm(Credentials {
        birth: None,
        ..peer.credentials
    })
    .is_err());
    assert!(ExitWatch::arm(Credentials {
        pid: None,
        ..peer.credentials
    })
    .is_err());
    assert!(ExitWatch::arm(Credentials {
        birth: Some(peer.credentials.birth.unwrap() ^ 1),
        ..peer.credentials
    })
    .is_err());
    let watch = ExitWatch::arm(peer.credentials).unwrap();
    peer.exit();
    assert!(readable(watch.as_raw_fd(), Duration::from_secs(5)));
    assert!(ExitWatch::arm(peer.credentials).is_err());
}
#[test]
fn failed_kernel_arm_never_reads_a_later_process_identity() {
    let peer = Credentials {
        pid: Some(u32::MAX),
        birth: Some(1),
        uid: None,
    };
    assert!(ExitWatch::checked(peer, |_| panic!("identity read before kernel arm")).is_err());
}

#[test]
fn async_kernel_exit_wait_is_observable_by_exited() {
    let mut peer = Peer::spawn();
    let watch = std::sync::Arc::new(ExitWatch::arm(peer.credentials).unwrap());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let waiting = watch.clone().wait();
            tokio::pin!(waiting);
            std::future::poll_fn(|cx| {
                assert!(std::future::Future::poll(waiting.as_mut(), cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            assert!(!watch.exited().unwrap());
            peer.exit();
            tokio::time::timeout(Duration::from_secs(5), &mut waiting)
                .await
                .unwrap()
                .unwrap();
            assert!(watch.exited().unwrap());
            assert!(watch.exited().unwrap());
        });
}

#[test]
fn exit_watch_refuses_unreaped_exit_during_birth_read() {
    let mut child = Peer::spawn();
    let credentials = child.credentials;
    assert!(ExitWatch::checked(credentials, |pid| {
        child.exit_unreaped();
        crate::platform::process_start_time(pid)
    })
    .is_err());
}
