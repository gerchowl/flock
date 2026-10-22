//! Descriptor inheritance protection at the SCM_RIGHTS receive boundary.
use std::{io, os::fd::RawFd};
/// Linux installs CLOEXEC atomically with SCM_RIGHTS descriptor allocation.
#[cfg(target_os = "linux")]
pub(crate) const RECV_FLAGS: libc::c_int = libc::MSG_CMSG_CLOEXEC;
/// Darwin has no atomic receive flag, leaving a receive-to-fcntl inheritance window.
#[cfg(not(target_os = "linux"))]
pub(crate) const RECV_FLAGS: libc::c_int = 0;
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
    #[test]
    fn received_rights_install_close_on_exec_at_the_platform_boundary() {
        use std::os::fd::{FromRawFd, OwnedFd};
        use std::os::unix::net::UnixStream;
        let (sender, receiver) = UnixStream::pair().unwrap();
        let mut byte = 1u8;
        let mut iov = libc::iovec {
            iov_base: (&mut byte as *mut u8).cast(),
            iov_len: 1,
        };
        // Word storage provides the required control-message alignment.
        let bytes = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as _) as usize };
        let mut control = vec![0usize; bytes.div_ceil(std::mem::size_of::<usize>())];
        // Initialized POD header points to owned iovec and aligned control storage.
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = bytes as _;
        // The initialized header has capacity for exactly one rights descriptor.
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&message);
            assert!(!header.is_null());
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as _) as _;
            std::ptr::write(libc::CMSG_DATA(header).cast::<RawFd>(), sender.as_raw_fd());
            assert_eq!(libc::sendmsg(sender.as_raw_fd(), &message, 0), 1);
        }
        control.fill(0);
        // Send completed, so the receiver cannot wait on a missing producer.
        assert_eq!(
            unsafe { libc::recvmsg(receiver.as_raw_fd(), &mut message, RECV_FLAGS) },
            1
        );
        // The single received rights entry transfers exclusive fd ownership.
        let owned = unsafe {
            let header = libc::CMSG_FIRSTHDR(&message);
            assert!(!header.is_null());
            assert_eq!((*header).cmsg_type, libc::SCM_RIGHTS);
            OwnedFd::from_raw_fd(std::ptr::read(libc::CMSG_DATA(header).cast::<RawFd>()))
        };
        if cfg!(target_os = "linux") {
            assert_ne!(RECV_FLAGS, 0, "Linux must use its atomic rights flag");
            assert_ne!(
                unsafe { libc::fcntl(owned.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                0,
                "CLOEXEC must already be set before fcntl"
            );
        } else {
            assert_eq!(RECV_FLAGS, 0);
            close_on_exec(owned.as_raw_fd()).unwrap();
        }
        assert_ne!(
            unsafe { libc::fcntl(owned.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
    }
}
