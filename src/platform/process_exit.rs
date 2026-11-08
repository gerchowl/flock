//! Kernel process-exit evidence for app sessions (ADR-0038 §6).
use super::peer::Credentials;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// Owned, close-on-exec exit registration; callers still enforce LocalOperator.
/// Readiness is persistent until drop, so adapters may await this fd off the app loop.
pub(crate) struct ExitWatch(OwnedFd, Credentials);
impl ExitWatch {
    /// Register first, then compare the start identity captured at socket accept.
    /// Missing evidence, exited processes and unsupported kernels fail closed.
    pub(crate) fn arm(peer: Credentials) -> io::Result<Self> {
        Self::checked(peer, super::process_start_time)
    }
    fn checked(peer: Credentials, birth: impl FnOnce(u32) -> Option<u64>) -> io::Result<Self> {
        let pid = peer.pid.ok_or_else(changed)?;
        let watch = Self(kernel_watch(pid)?, peer);
        if peer.birth.is_none() || birth(pid) != peer.birth || watch.exited()? {
            return Err(changed());
        }
        Ok(watch)
    }
    /// Prove this registration belongs to the exact accepted connection evidence.
    pub(crate) fn matches(&self, peer: Credentials) -> bool {
        self.1.pid == peer.pid && self.1.uid == peer.uid && self.1.birth == peer.birth
    }
    /// Await persistent kernel readiness without polling or occupying a blocking worker.
    #[cfg(any(feature = "web", test))]
    pub(crate) async fn wait(self: std::sync::Arc<Self>) -> io::Result<()> {
        // Each waiter owns a distinct descriptor; reactor registrations cannot replace
        // each other, and no waiter consumes the shared persistent exit evidence.
        let descriptor = self.0.try_clone()?;
        // SAFETY: AsyncFd owns this immutable OwnedFd for its complete registration.
        let fd = unsafe {
            tokio::io::unix::AsyncFd::register_with_interest(
                descriptor,
                tokio::io::Interest::READABLE,
            )?
        };
        drop(fd.readable().await?);
        Ok(())
    }
    /// Nonblocking inspection of the kernel registration, never PID liveness polling.
    /// Errors withhold authorization; they are not treated as proof of a live peer.
    pub(crate) fn exited(&self) -> io::Result<bool> {
        let mut event = libc::pollfd {
            fd: self.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: event is a live single-element output buffer, with an owned fd.
        if unsafe { libc::poll(&mut event, 1, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if event.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(changed());
        }
        Ok(event.revents & (libc::POLLIN | libc::POLLHUP) != 0)
    }
}
impl AsRawFd for ExitWatch {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}
fn changed() -> io::Error {
    io::Error::other("peer process changed or cannot be watched")
}
fn kernel_watch(pid: u32) -> io::Result<OwnedFd> {
    let pid = libc::pid_t::try_from(pid).map_err(|_| changed())?;
    #[cfg(target_os = "linux")]
    // SAFETY: pidfd_open takes a scalar PID and zero flags, returning an owned fd.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as RawFd };
    #[cfg(target_os = "macos")]
    // SAFETY: kqueue has no input pointers and returns an owned descriptor.
    let fd = unsafe { libc::kqueue() };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd was freshly returned and has exactly one owner.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: F_SETFD consumes an integer descriptor flag, not a pointer.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    #[cfg(target_os = "macos")]
    {
        let event = libc::kevent {
            ident: pid as usize,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: the single change lives for the call; no event output is requested.
        if unsafe {
            libc::kevent(
                fd.as_raw_fd(),
                &event,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(fd)
}
#[cfg(test)]
pub(crate) mod tests;
