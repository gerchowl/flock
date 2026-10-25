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
