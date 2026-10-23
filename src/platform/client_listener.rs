//! Owned listening-socket descriptors exchanged by the live-handoff protocol.
use std::{
    io,
    net::{SocketAddr, TcpListener},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
};
/// Accept recovery classes shared by the listener owner and structured logging.
#[cfg(feature = "web")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AcceptFailure {
    Connection,
    Resource,
    Fatal,
    Unknown,
}
/// Only invalid listener descriptors are fatal; all other peer/protocol errors are recoverable.
#[cfg(feature = "web")]
pub(crate) fn accept_failure(error: &io::Error) -> AcceptFailure {
    match error.raw_os_error() {
        Some(libc::EBADF | libc::EINVAL | libc::ENOTSOCK) => AcceptFailure::Fatal,
        Some(
            libc::EMFILE
            | libc::ENFILE
            | libc::ENOBUFS
            | libc::ENOMEM
            | libc::ENOPROTOOPT
            | libc::EOPNOTSUPP,
        ) => AcceptFailure::Resource,
        Some(
            libc::ECONNABORTED
            | libc::ECONNRESET
            | libc::EINTR
            | libc::EPROTO
            | libc::ENETDOWN
            | libc::ENETUNREACH
            | libc::EHOSTUNREACH
            | libc::ETIMEDOUT
            | libc::ENETRESET,
        ) => AcceptFailure::Connection,
        _ => AcceptFailure::Unknown,
    }
}
/// Duplicate without relinquishing the exporter's rollback ownership.
// Non-web builds transfer PTYs but do not adopt the optional TCP listener.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub(crate) fn duplicate(listener: &TcpListener) -> io::Result<OwnedFd> {
    Ok(listener.try_clone()?.into())
}
/// Typed refusal of a descriptor that cannot satisfy the IPv4 listener contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InvalidListener {
    Missing,
    Family,
    SocketType,
    /// A socket inspection failed; retain the operation and its original errno.
    Query(&'static str, Option<i32>),
    NotListening,
    /// The bound socket would permit exposure beyond IPv4 loopback.
    NonLoopback,
    /// Import supplied a listener after the configured binding was removed.
    #[cfg(feature = "web")]
    Unconfigured,
    /// The imported listener does not match the configured address or nonzero port.
    #[cfg(feature = "web")]
    BindMismatch {
        expected: SocketAddr,
        actual: SocketAddr,
    },
}
impl std::fmt::Display for InvalidListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Query(operation, errno) => write!(
                f,
                "structured listener {operation} failed (errno {errno:?})"
            ),
            Self::Missing => f.write_str("missing structured listener descriptor"),
            Self::Family => f.write_str("structured listener must use the IPv4 address family"),
            Self::SocketType => f.write_str("structured listener must be a stream socket"),
            Self::NotListening => f.write_str("structured listener socket is not listening"),
            Self::NonLoopback => f.write_str(
                "structured listener address is outside its allowed IPv4 loopback binding",
            ),
            #[cfg(feature = "web")]
            Self::Unconfigured => {
                f.write_str("received structured listener without a configured bind")
            }
            #[cfg(feature = "web")]
            Self::BindMismatch { expected, actual } => write!(
                f,
                "received structured listener {actual} does not match configured bind {expected}"
            ),
        }
    }
}
impl std::error::Error for InvalidListener {}
/// Preserve syscall diagnostics inside the typed descriptor refusal.
fn query_failure(operation: &'static str, error: io::Error) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        InvalidListener::Query(operation, error.raw_os_error()),
    )
}
/// Refuse any address outside IPv4's loopback block, including wildcard binds.
pub(crate) fn require_loopback(address: SocketAddr) -> io::Result<()> {
    if !matches!(address,SocketAddr::V4(address) if address.ip().is_loopback()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            InvalidListener::NonLoopback,
        ));
    }
    Ok(())
}
/// Consume the final owned descriptor, requiring IPv4, stream type and listening state.
// Non-web builds transfer PTYs but do not adopt the optional TCP listener.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub(crate) fn take(fds: &mut Vec<RawFd>) -> io::Result<TcpListener> {
    let fd = fds
        .pop()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, InvalidListener::Missing))?;
    // SCM_RIGHTS transferred exclusive ownership; every return drops this owner exactly once.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    crate::platform::received_fd::close_on_exec(owned.as_raw_fd())?;
    // Zeroed storage is a valid initialized output buffer for getsockname.
    let mut address: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of_val(&address) as libc::socklen_t;
    // The output pointer references the full initialized storage and its capacity.
    if unsafe {
        libc::getsockname(
            fd,
            (&mut address as *mut libc::sockaddr_storage).cast(),
            &mut length,
        )
    } < 0
    {
        return Err(query_failure("getsockname", io::Error::last_os_error()));
    }
    let mut socket_type: libc::c_int = 0;
    // SO_TYPE returns an initialized C integer.
    unsafe {
        option(fd, libc::SOL_SOCKET, libc::SO_TYPE, &mut socket_type)
            .map_err(|error| query_failure("SO_TYPE", error))?;
    }
    let refusal = if address.ss_family as libc::c_int != libc::AF_INET {
        Some(InvalidListener::Family)
    } else if socket_type != libc::SOCK_STREAM {
        Some(InvalidListener::SocketType)
    } else if !listening(fd)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, InvalidListener::NotListening))?
    {
        Some(InvalidListener::NotListening)
    } else {
        None
    };
    if let Some(reason) = refusal {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, reason));
    }
    let listener = TcpListener::from(owned);
    require_loopback(listener.local_addr()?)?;
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
    use std::os::fd::IntoRawFd;
    #[tokio::test]
    async fn transferred_listener_keeps_the_port_and_is_close_on_exec() {
        let original = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = original.local_addr().unwrap();
        let mut descriptors = vec![duplicate(&original).unwrap().into_raw_fd()];
        // Simulate the absence of CLOEXEC on the received SCM_RIGHTS descriptor.
        assert_eq!(unsafe { libc::fcntl(descriptors[0], libc::F_SETFD, 0) }, 0);
        let inherited = take(&mut descriptors).unwrap();
        assert!(descriptors.is_empty());
        let flags = unsafe { libc::fcntl(inherited.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        drop(original);
        assert!(TcpListener::bind(address).is_err());
        inherited.set_nonblocking(true).unwrap();
        let inherited = tokio::net::TcpListener::from_std(inherited).unwrap();
        let _client = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::net::TcpStream::connect(address),
        )
        .await
        .expect("connection producer stayed silent")
        .unwrap();
        let _accepted = tokio::time::timeout(std::time::Duration::from_secs(5), inherited.accept())
            .await
            .expect("connected peer never became accept-ready")
            .unwrap();
    }
    #[test]
    fn duplicated_listener_has_owned_close_on_drop_semantics() {
        let original = TcpListener::bind("127.0.0.1:0").unwrap();
        let owned: OwnedFd = duplicate(&original).unwrap();
        let address = original.local_addr().unwrap();
        drop(owned);
        assert!(original.local_addr().is_ok());
        drop(original);
        // A leaked duplicate retains the port, regardless of descriptor-number reuse.
        let _rebound = TcpListener::bind(address).expect("duplicate retained the listening port");
    }
    #[test]
    fn missing_or_non_listening_descriptor_is_refused() {
        let reason = |error: io::Error, expected| {
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert_eq!(
                error
                    .get_ref()
                    .and_then(|e| e.downcast_ref::<InvalidListener>()),
                Some(&expected)
            );
        };
        reason(take(&mut Vec::new()).unwrap_err(), InvalidListener::Missing);
        let root = crate::test_support::TempDir::socket();
        let file = std::fs::File::create(root.path().join("not-a-socket")).unwrap();
        reason(
            take(&mut vec![file.into_raw_fd()]).unwrap_err(),
            InvalidListener::Query("getsockname", Some(libc::ENOTSOCK)),
        );
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        reason(
            take(&mut vec![socket.into_raw_fd()]).unwrap_err(),
            InvalidListener::SocketType,
        );
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        reason(
            take(&mut vec![socket.into_raw_fd()]).unwrap_err(),
            InvalidListener::NotListening,
        );
    }
}

#[cfg(test)]
mod family_tests {
    use super::*;
    use std::os::fd::IntoRawFd;
    fn refused(fd: RawFd) {
        let mut fds = vec![fd];
        let error = take(&mut fds).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            error
                .get_ref()
                .and_then(|e| e.downcast_ref::<InvalidListener>()),
            Some(&InvalidListener::Family)
        );
        assert!(fds.is_empty());
    }
    #[test]
    fn listening_unix_socket_is_refused_with_owned_descriptor_cleanup() {
        let root = crate::test_support::TempDir::socket();
        let socket = std::os::unix::net::UnixListener::bind(root.path().join("l.sock")).unwrap();
        refused(socket.into_raw_fd());
        assert_eq!(
            std::os::unix::net::UnixStream::connect(root.path().join("l.sock"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::ConnectionRefused
        );
    }
    #[test]
    fn listening_ipv6_socket_is_refused_with_owned_descriptor_cleanup() {
        let socket = TcpListener::bind("[::1]:0").unwrap();
        let address = socket.local_addr().unwrap();
        refused(socket.into_raw_fd());
        let _rebound = TcpListener::bind(address).expect("refused IPv6 listener retained its port");
    }
}

#[cfg(test)]
mod loopback_tests {
    use super::*;
    use std::os::fd::IntoRawFd;
    #[test]
    fn received_wildcard_listener_is_refused_and_closed() {
        let listener = TcpListener::bind("0.0.0.0:0").unwrap();
        let address = listener.local_addr().unwrap();
        let fd = listener.into_raw_fd();
        let mut fds = vec![fd];
        let error = take(&mut fds).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            error
                .get_ref()
                .and_then(|e| e.downcast_ref::<InvalidListener>()),
            Some(&InvalidListener::NonLoopback)
        );
        assert!(fds.is_empty());
        let _rebound =
            TcpListener::bind(address).expect("refused wildcard listener retained its port");
    }
    #[test]
    fn loopback_exposure_boundary_is_the_entire_ipv4_loopback_block() {
        for address in ["127.0.0.1:1", "127.0.1.2:1", "127.255.255.255:1"] {
            require_loopback(address.parse().unwrap()).unwrap();
        }
        for address in ["0.0.0.0:1", "192.0.2.1:1", "[::1]:1"] {
            let error = require_loopback(address.parse().unwrap()).unwrap_err();
            assert_eq!(
                error
                    .get_ref()
                    .and_then(|e| e.downcast_ref::<InvalidListener>()),
                Some(&InvalidListener::NonLoopback)
            );
        }
    }
}

#[cfg(test)]
mod query_tests {
    use super::*;
    #[test]
    fn inspection_refusals_retain_operation_and_errno() {
        for operation in ["getsockname", "SO_TYPE"] {
            let error = query_failure(operation, io::Error::from_raw_os_error(libc::ENOTSOCK));
            assert_eq!(
                error.get_ref().unwrap().downcast_ref::<InvalidListener>(),
                Some(&InvalidListener::Query(operation, Some(libc::ENOTSOCK)))
            );
            assert!(error.to_string().contains(&libc::ENOTSOCK.to_string()));
        }
    }
}
