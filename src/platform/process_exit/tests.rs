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
        Self::request_to(request, None)
    }
    #[cfg(feature = "web")]
    pub(crate) fn bridge(request: &str, path: &std::path::Path) -> Self {
        Self::request_to(Some(request), Some(path))
    }
    fn request_to(request: Option<&str>, bridge: Option<&std::path::Path>) -> Self {
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
            "--ignored",
        ]);
        command.env("FLOCK_TEST_PROCESS_EXIT_SOCKET", &path);
        if let Some(request) = request {
            command.env("FLOCK_TEST_PROCESS_EXIT_REQUEST", request);
        }
        if let Some(path) = bridge {
            command.env("FLOCK_TEST_NATIVE_BRIDGE", path);
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
// Peer launches this helper with --ignored; it is not a standalone acceptance test.
#[test]
#[ignore]
fn peer_child() {
    let Some(path) = std::env::var_os("FLOCK_TEST_PROCESS_EXIT_SOCKET") else {
        return;
    };
    let mut socket = UnixStream::connect(path).unwrap();
    if let Ok(request) = std::env::var("FLOCK_TEST_PROCESS_EXIT_REQUEST") {
        writeln!(socket, "{request}").unwrap();
        let mut reply = String::new();
        BufReader::new(&socket).read_line(&mut reply).unwrap();
        #[cfg(feature = "web")]
        if let Some(path) = std::env::var_os("FLOCK_TEST_NATIVE_BRIDGE") {
            bridge_sockets(&socket, std::path::Path::new(&path), &reply);
        }
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

#[cfg(feature = "web")]
fn bridge_sockets(control: &UnixStream, path: &std::path::Path, reply: &str) {
    use std::os::fd::AsRawFd;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let value: serde_json::Value = serde_json::from_str(reply).unwrap();
    let token = value["result"]["issued"]["token"]
        .as_str()
        .expect("mint must succeed");
    crate::web::wire::tests::runtime(async {
        let mut sockets = Vec::new();
        for _ in 0..2 {
            let mut socket = tokio::net::UnixStream::connect(path).await.unwrap();
            socket.write_all(b"GET /v1/ws HTTP/1.1\r\nHost: fixture.example\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n").await.unwrap(); // gitleaks:allow -- RFC 6455 public handshake example nonce
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                header.push(socket.read_u8().await.unwrap());
                assert!(header.len() < 8192);
            }
            assert!(String::from_utf8(header)
                .unwrap()
                .starts_with("HTTP/1.1 101"));
            let mut ws = crate::web::wire::tests::Socket(socket);
            ws.hello(crate::structured_wire::WIRE_JSON_VERSION, token)
                .await;
            sockets.push(ws.0.into_std().unwrap());
        }
        // Transfer client descriptors only after the child's own hellos receive welcome.
        // Keeping them in the parent makes the server's 4403 observable after child exit.
        crate::server::handoff::send_fds(
            control,
            &sockets.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>(),
        )
        .unwrap();
    });
}
