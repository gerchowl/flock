//! Descriptor inheritance protection at the SCM_RIGHTS receive boundary.
use std::{io, os::fd::RawFd};
/// Prevent an owned received descriptor from leaking into any subsequently spawned child.
pub(crate) fn close_on_exec(fd: RawFd) -> io::Result<()> {
    // F_GETFD preserves any descriptor flags beyond close-on-exec.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    #[test]
    fn close_on_exec_refuses_invalid_descriptors() {
        assert_eq!(
            close_on_exec(-1).unwrap_err().raw_os_error(),
            Some(libc::EBADF)
        );
    }
    #[test]
    fn close_on_exec_protects_an_owned_descriptor() {
        let (reader, _writer) = std::os::unix::net::UnixStream::pair().unwrap();
        let fd = reader.as_raw_fd();
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, 0) }, 0);
        close_on_exec(fd).unwrap();
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
    }
}
