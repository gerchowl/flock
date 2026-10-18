//! Owned listening-socket descriptors exchanged by the live-handoff protocol.
use std::{
    io,
    net::TcpListener,
    os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd},
};
/// Accept recovery classes shared by the listener owner and structured logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AcceptFailure {
    Connection,
    Resource,
    Fatal,
}
/// Only invalid listener descriptors are fatal; all other peer/protocol errors are recoverable.
pub(crate) fn accept_failure(error: &io::Error) -> AcceptFailure {
    match error.raw_os_error() {
        Some(libc::EBADF | libc::EINVAL | libc::ENOTSOCK) => AcceptFailure::Fatal,
        Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM) => AcceptFailure::Resource,
        _ => AcceptFailure::Connection,
    }
}
/// Duplicate without relinquishing the exporter's rollback ownership.
pub(crate) fn duplicate(listener: &TcpListener) -> io::Result<RawFd> {
    Ok(listener.try_clone()?.into_raw_fd())
}
/// Consume the final transferred descriptor and require an actual listening socket.
pub(crate) fn take(fds: &mut Vec<RawFd>) -> io::Result<TcpListener> {
    let fd = fds
        .pop()
        .ok_or_else(|| io::Error::other("missing structured listener descriptor"))?;
    // The authenticated SCM_RIGHTS receiver transferred exclusive ownership of this fd.
    let listener = unsafe { TcpListener::from_raw_fd(fd) };
    // SCM_RIGHTS does not promise close-on-exec on both supported platforms.
    if unsafe { libc::fcntl(listener.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if !listening(listener.as_raw_fd())? {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(listener)
}
/// # Safety
/// The initialized output must be the POD layout for this socket option.
unsafe fn option<T>(
    fd: RawFd,
    level: libc::c_int,
    name: libc::c_int,
    value: &mut T,
) -> io::Result<()> {
    let mut len = std::mem::size_of_val(value) as libc::socklen_t;
    // The caller supplies a live POD output pointer and its exact buffer capacity.
    if unsafe { libc::getsockopt(fd, level, name, (value as *mut T).cast(), &mut len) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
#[cfg(target_os = "macos")]
fn listening(fd: RawFd) -> io::Result<bool> {
    /// Darwin's listener state in [tcp_fsm.h](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/netinet/tcp_fsm.h).
    const TCPS_LISTEN: u8 = 1;
    // All-zero scalar fields are valid, and state is the first ABI byte.
    let mut info: libc::tcp_connection_info = unsafe { std::mem::zeroed() };
    // TCP_CONNECTION_INFO accepts this platform's initialized C layout.
    unsafe {
        option(fd, libc::IPPROTO_TCP, libc::TCP_CONNECTION_INFO, &mut info)?;
    }
    Ok(info.tcpi_state == TCPS_LISTEN)
}
#[cfg(not(target_os = "macos"))]
fn listening(fd: RawFd) -> io::Result<bool> {
    let mut accept: libc::c_int = 0;
    // SO_ACCEPTCONN returns an initialized C integer on this platform.
    unsafe {
        option(fd, libc::SOL_SOCKET, libc::SO_ACCEPTCONN, &mut accept)?;
    }
    Ok(accept != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transferred_listener_keeps_the_port_and_is_close_on_exec() {
        let original = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = original.local_addr().unwrap();
        let mut descriptors = vec![duplicate(&original).unwrap()];
        // Simulate the absence of CLOEXEC on the received SCM_RIGHTS descriptor.
        assert_eq!(unsafe { libc::fcntl(descriptors[0], libc::F_SETFD, 0) }, 0);
        let inherited = take(&mut descriptors).unwrap();
        assert!(descriptors.is_empty());
        let flags = unsafe { libc::fcntl(inherited.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        drop(original);
        assert!(TcpListener::bind(address).is_err());
        let _client = std::net::TcpStream::connect(address).unwrap();
        inherited.set_nonblocking(true).unwrap();
        let _accepted = inherited.accept().unwrap();
    }
    #[test]
    fn missing_or_non_listening_descriptor_is_refused() {
        assert!(take(&mut Vec::new()).is_err());
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        assert!(take(&mut vec![socket.into_raw_fd()]).is_err());
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        assert!(take(&mut vec![socket.into_raw_fd()]).is_err());
    }
}
