//! Nonblocking Unix connect, bounded even when the listener backlog is full.
use std::{
    io, mem,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, net::UnixStream},
    },
    path::Path,
    time::Instant,
};

/// Connect without letting the kernel wait indefinitely for a listener slot.
/// The caller owns timeout diagnosis; all descriptor cleanup is automatic.
pub(crate) fn connect(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
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
