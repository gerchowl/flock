//! Kernel socket credentials and dedicated-user lookup (ADR-0036 §1a).
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

/// Credentials supplied by the kernel, never request fields.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Credentials {
    /// Connecting process, when the kernel exposes it.
    pub pid: Option<u32>,
    /// Effective UID at connection establishment.
    pub uid: Option<u32>,
    /// Process start identity captured before the request enters the queue.
    pub birth: Option<u64>,
}
/// Read connection credentials. Missing evidence remains explicitly unknown.
pub(crate) fn credentials(stream: &UnixStream) -> Credentials {
    let mut result = Credentials {
        pid: None,
        uid: None,
        birth: None,
    };
    #[cfg(target_os = "macos")]
    {
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: the socket and both output scalars are live for the call.
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == 0 {
            result.uid = Some(uid);
        }
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of_val(&pid) as libc::socklen_t;
        // SOL_LOCAL / LOCAL_PEERPID are the socket-local peer PID query.
        // SAFETY: LOCAL_PEERPID writes at most the supplied scalar buffer.
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut len,
            )
        } == 0
            && len as usize == std::mem::size_of_val(&pid)
            && pid > 0
        {
            result.pid = Some(pid as u32);
        }
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: ucred is a plain C output structure and all-zero is valid.
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of_val(&cred) as libc::socklen_t;
        // SAFETY: SO_PEERCRED writes at most the supplied initialized buffer.
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        } == 0
            && len as usize == std::mem::size_of_val(&cred)
        {
            result.uid = Some(cred.uid);
            result.pid = (cred.pid > 0).then_some(cred.pid as u32);
        }
    }
    result.birth = result.pid.and_then(super::process_start_time);
    result
}
/// Effective operator UID of this server.
pub(crate) fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions or memory arguments.
    unsafe { libc::geteuid() }
}
/// Blocking NSS lookup: only call on the lazy account worker. Missing accounts
/// differ from directory errors, which must fail closed. No global passwd buffer.
pub(crate) fn dedicated_uid() -> io::Result<Option<u32>> {
    let mut storage = vec![0u8; 65536];
    // SAFETY: passwd is an output-only C structure and all-zero is valid.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found = std::ptr::null_mut();
    // SAFETY: the name is NUL terminated; entry/storage/found remain live, and
    // no pointer into storage escapes this function.
    let rc = unsafe {
        libc::getpwnam_r(
            c"flk-agent".as_ptr(),
            &mut entry,
            storage.as_mut_ptr().cast(),
            storage.len(),
            &mut found,
        )
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    Ok((!found.is_null()).then_some(entry.pw_uid))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn owned_listener_credentials_match_the_kernel_process() {
        let root = crate::test_support::TempDir::socket();
        let listener =
            std::os::unix::net::UnixListener::bind(root.path().join("peer.sock")).unwrap();
        let client =
            UnixStream::connect(listener.local_addr().unwrap().as_pathname().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let captured = credentials(&server);
        drop(client);
        assert_eq!(captured.pid, Some(std::process::id()));
        assert_eq!(captured.uid, Some(effective_uid()));
        assert_eq!(
            captured.birth,
            super::super::process_start_time(std::process::id())
        );
        assert!(captured.birth.is_some());
    }
}
