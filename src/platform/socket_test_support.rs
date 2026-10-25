//! Socket fixtures with explicit kernel buffer sizes, independent of host defaults.
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

/// Bound the send buffer so a real frame deterministically reaches backpressure.
pub(crate) fn bound_send_buffer(socket: &UnixStream) -> io::Result<()> {
    let bytes: libc::c_int = 4096;
    // SAFETY: socket is borrowed, and the option buffer has the declared size.
    if unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            (&bytes as *const libc::c_int).cast(),
            std::mem::size_of_val(&bytes) as libc::socklen_t,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
