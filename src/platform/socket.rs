//! Deadline-based Unix socket reads without changing options after peer close.
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Wait for one readable chunk, including data queued before a peer's hangup.
/// The caller recomputes its absolute deadline after interrupted/spurious
/// readiness or a bounded poll timeout. Receive is nonblocking so readiness
/// cannot accidentally restart a full socket timeout. No request is replayed.
pub(crate) fn read_ready(
    stream: &UnixStream,
    buffer: &mut [u8],
    remaining: Duration,
) -> io::Result<usize> {
    let mut descriptor = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let millis =
        remaining.as_millis() + u128::from(!remaining.subsec_nanos().is_multiple_of(1_000_000));
    let timeout = millis.min(i32::MAX as u128) as i32;
    // SAFETY: descriptor refers to this live borrowed stream for the entire poll.
    match unsafe { libc::poll(&mut descriptor, 1, timeout) } {
        -1 => return Err(io::Error::last_os_error()),
        0 => return Err(io::ErrorKind::TimedOut.into()),
        _ => {}
    }
    // POLLHUP/POLLERR can accompany a complete reply. Drain it before interpreting EOF.
    // SAFETY: buffer is writable for its length; the borrowed socket remains open.
    let read = unsafe {
        libc::recv(
            descriptor.fd,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            libc::MSG_DONTWAIT,
        )
    };
    if read < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(read as usize)
    }
}

/// Open a permanent, private ownership file without following symbolic links.
/// Repair umask-restricted permissions through the new descriptor only. Existing
/// files remain unchanged and must pass the caller's ownership/mode validation.
pub(crate) fn open_owner(path: &std::path::Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    match options.open(path) {
        Ok(file) => {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            Ok(file)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            options.create_new(false).open(path)
        }
        Err(error) => Err(error),
    }
}

/// Nonblocking exclusive ownership, shared across duplicated handoff descriptors.
/// Close the last descriptor to release it: explicit LOCK_UN would unlock successors.
pub(crate) fn lock_owner(file: &std::fs::File) -> io::Result<()> {
    // SAFETY: flock borrows this open file and does not take descriptor ownership.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Nonblocking liveness probe for an existing pathname socket.
/// Success is live; errors alone do not prove staleness (#2531).
/// Return kernel evidence without waiting or retrying; a full live backlog can refuse.
pub(crate) fn probe_listener(path: &std::path::Path) -> io::Result<()> {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: sockaddr_un is a plain C structure with a valid zero representation.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() || bytes.contains(&0) || bytes.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Unix socket path",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    #[cfg(target_os = "macos")]
    {
        address.sun_len = std::mem::size_of_val(&address) as u8;
    }
    for (to, from) in address.sun_path.iter_mut().zip(bytes) {
        *to = *from as libc::c_char;
    }
    let flags = libc::SOCK_STREAM;
    #[cfg(target_os = "linux")]
    let flags = flags | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK;
    // SAFETY: socket takes scalar protocol arguments and returns a new descriptor.
    let raw = unsafe { libc::socket(libc::AF_UNIX, flags, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw is a fresh, exclusively owned descriptor from socket.
    let socket = unsafe { OwnedFd::from_raw_fd(raw) };
    #[cfg(target_os = "macos")]
    {
        // SAFETY: both fcntl calls operate on this owned, not-yet-shared descriptor.
        if unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) } < 0
            || unsafe { libc::fcntl(raw, libc::F_SETFL, libc::O_NONBLOCK) } < 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: address is initialized, NUL terminated and borrowed for this call only.
    let result = unsafe {
        libc::connect(
            socket.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            std::mem::size_of_val(&address) as libc::socklen_t,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::{os::unix::net::UnixListener, sync::mpsc};

    #[test]
    fn a_full_listener_backlog_preserves_the_live_socket_and_cannot_block() {
        let (done, received) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let root = crate::test_support::TempDir::socket();
            let path = root.path().join("backlog.sock");
            let _owner = crate::ipc::prepare_socket_path(&path, |_| "busy".into()).unwrap();
            let listener = UnixListener::bind(&path).unwrap();
            // SAFETY: the fixture owns this live listener; shrinking its queue is local.
            assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 1) }, 0);
            for admitted in 0..128 {
                match probe_listener(&path) {
                    Ok(()) => (),
                    Err(error) => {
                        assert!(
                            admitted > 0,
                            "the backlog first admitted a connection: {error}"
                        );
                        assert!(
                            matches!(
                                error.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::ConnectionRefused
                            ) || error.raw_os_error() == Some(libc::EINPROGRESS),
                            "unexpected backlog error: {error}"
                        );
                        assert_eq!(
                            crate::ipc::prepare_socket_path(&path, |_| "busy".into())
                                .unwrap_err()
                                .kind(),
                            io::ErrorKind::AddrInUse
                        );
                        assert!(
                            path.exists(),
                            "full backlog must preserve the live listener"
                        );
                        done.send(()).unwrap();
                        return;
                    }
                }
            }
            panic!("fixture backlog did not fill within its bounded admission count");
        });
        // A blocked worker stays alive and silent; the test must fail rather than join it.
        received
            .recv_timeout(Duration::from_secs(5))
            .expect("preparation worker failed or stalled on a full backlog");
        worker.join().unwrap();
    }
}
