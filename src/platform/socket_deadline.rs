//! Nonblocking Unix connect, bounded even when the listener backlog is full.
use std::{
    io, mem,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, net::UnixStream},
    },
    path::Path,
    time::{Duration, Instant},
};

/// Connect without letting the kernel wait indefinitely for a listener slot.
/// The caller owns timeout diagnosis; all descriptor cleanup is automatic.
pub(crate) fn connect(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
    connect_with_retry(path, deadline, |delay, _| std::thread::park_timeout(delay))
}

/// Admission backpressure consumes the caller's existing deadline, never a new budget.
fn connect_with_retry(
    path: &Path,
    deadline: Instant,
    mut wait: impl FnMut(Duration, &io::Error),
) -> io::Result<UnixStream> {
    let mut backoff = Duration::from_millis(1);
    loop {
        match connect_once(path, deadline) {
            Ok(stream) => return Ok(stream),
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || (cfg!(target_os = "macos")
                        && error.kind() == io::ErrorKind::ConnectionRefused) =>
            {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                // A full Darwin Unix backlog reports ECONNREFUSED, not EAGAIN.
                // A fresh descriptor on each attempt also discards failed socket state.
                wait(backoff.min(remaining), &error);
                backoff = backoff.saturating_mul(2).min(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}

fn connect_once(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
    if Instant::now() >= deadline {
        return Err(io::ErrorKind::TimedOut.into());
    }
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: an all-zero sockaddr_un is a valid starting representation.
    let mut address: libc::sockaddr_un = unsafe { mem::zeroed() };
    if bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Unix socket path",
        ));
    }
    address.sun_family = libc::AF_UNIX as _;
    for (out, byte) in address.sun_path.iter_mut().zip(bytes) {
        *out = *byte as _;
    }
    let length = mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    #[cfg(target_os = "macos")]
    {
        address.sun_len = length as _;
    }
    // SAFETY: socket has no pointer arguments and returns a fresh descriptor.
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw is a newly owned descriptor, adopted exactly once.
    let owned = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: raw is live and F_SETFD takes the integer flag directly.
    if unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = UnixStream::from(owned);
    stream.set_nonblocking(true)?;
    // SAFETY: address and its declared length remain live throughout connect.
    let result = unsafe {
        libc::connect(
            raw,
            (&address as *const libc::sockaddr_un).cast(),
            length as _,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(libc::EINPROGRESS | libc::EINTR)) {
            return Err(error);
        }
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let mut fd = libc::pollfd {
                fd: stream.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            let millis = remaining
                .as_millis()
                .saturating_add(1)
                .min(i32::MAX as u128) as i32;
            // SAFETY: fd points to one initialized pollfd for this call.
            let ready = unsafe { libc::poll(&mut fd, 1, millis) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if ready > 0 {
                if let Some(error) = stream.take_error()? {
                    return Err(error);
                }
                break;
            }
        }
    }
    stream.set_nonblocking(false)?;
    Ok(stream)
}

/// Fill an owned listener without accepting, exercising the real connect backlog.
#[cfg(test)]
pub(crate) fn saturate_backlog(
    listener: &std::os::unix::net::UnixListener,
    path: &Path,
) -> Vec<UnixStream> {
    // SAFETY: listener owns this live socket. A small queue keeps the fixture bounded.
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 1) }, 0);
    let mut holders = Vec::new();
    for _ in 0..16 {
        match connect(path, Instant::now() + std::time::Duration::from_millis(100)) {
            Ok(stream) => holders.push(stream),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::ConnectionRefused
                ) =>
            {
                assert!(
                    !holders.is_empty(),
                    "backlog must contain a real connection"
                );
                return holders;
            }
            Err(error) => panic!("unexpected backlog setup error: {error}"),
        }
    }
    panic!("listener never saturated its one-entry backlog");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Write},
        os::unix::net::UnixListener,
        sync::mpsc,
        thread,
    };

    const CEILING: Duration = Duration::from_secs(5);

    #[test]
    fn full_backlog_connect_succeeds_when_admission_opens_before_deadline() {
        let root = crate::test_support::TempDir::socket();
        let path = root.path().join("admit");
        let listener = UnixListener::bind(&path).unwrap();
        let holders = saturate_backlog(&listener, &path);
        listener.set_nonblocking(true).unwrap();
        enum Stage {
            Backpressure(i32),
            Finished(io::Result<UnixStream>),
        }
        let (send, receive) = mpsc::channel();
        let (admit, admitted) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut observed = false;
            let result = connect_with_retry(&path, Instant::now() + CEILING, |delay, error| {
                assert!(!observed, "backlog stayed full after the admission gate");
                assert!(delay <= Duration::from_millis(10));
                observed = true;
                send.send(Stage::Backpressure(error.raw_os_error().unwrap()))
                    .unwrap();
                admitted
                    .recv_timeout(CEILING)
                    .expect("admission producer failed or stayed silent");
            });
            let result = result.and_then(|mut stream| {
                stream.set_write_timeout(Some(CEILING))?;
                stream.set_read_timeout(Some(CEILING))?;
                stream.write_all(b"{\"id\":\"admission\",\"method\":\"ping\",\"params\":{}}\n")?;
                let mut reply = String::new();
                BufReader::new(&stream).read_line(&mut reply)?;
                let reply: serde_json::Value = serde_json::from_str(&reply)?;
                assert_eq!(reply["id"], "admission");
                assert_eq!(reply["result"]["type"], "pong");
                Ok(stream)
            });
            send.send(Stage::Finished(result)).unwrap();
        });
        match receive
            .recv_timeout(CEILING)
            .expect("connector failed or stayed silent")
        {
            Stage::Backpressure(code) => {
                assert!(
                    code == libc::EAGAIN || code == libc::ECONNREFUSED,
                    "unexpected native backlog error: {code}"
                );
            }
            Stage::Finished(result) => {
                panic!("connect returned before backlog admission: {result:?}")
            }
        }
        // Each holder occupies one queued connection. Drain exactly that queue,
        // then release the observed retry; no elapsed wait decides admission.
        let peers: Vec<_> = (0..holders.len())
            .map(|_| listener.accept().unwrap().0)
            .collect();
        admit.send(()).unwrap();
        let mut readiness = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: readiness borrows this owned listener throughout the bounded poll.
        assert_eq!(
            unsafe { libc::poll(&mut readiness, 1, CEILING.as_millis() as i32) },
            1,
            "client failed or stayed silent after admission opened"
        );
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(CEILING)).unwrap();
        peer.set_write_timeout(Some(CEILING)).unwrap();
        let mut request = String::new();
        BufReader::new(&peer).read_line(&mut request).unwrap();
        let request: serde_json::Value = serde_json::from_str(&request).unwrap();
        assert_eq!(request["method"], "ping");
        peer.write_all(b"{\"id\":\"admission\",\"result\":{\"type\":\"pong\"}}\n")
            .unwrap();
        match receive
            .recv_timeout(CEILING)
            .expect("connector did not finish after admission")
        {
            Stage::Finished(result) => {
                drop(result.expect("connect must succeed after admission opens"));
            }
            Stage::Backpressure(code) => panic!("unexpected second admission report: {code}"),
        }
        worker.join().unwrap();
        drop((holders, peers));
    }
}
