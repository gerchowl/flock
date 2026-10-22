//! Kernel process identity and bounded exit notifications for sandbox cleanup.
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

/// Treat permission denial as alive, and never interpret a PID as a process group.
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    (unsafe { libc::kill(pid as i32, 0) == 0 })
        || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Native process birth identity, or no identity when inspection fails.
pub fn birth(pid: u32) -> Option<u64> {
    imp::birth(pid)
}

fn checked(result: i32) -> io::Result<i32> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

/// Register before rechecking identity, then signal only the verified process.
/// A timeout retains the sandbox instead of removing a running server's files.
pub fn stop(pid: u32, verify: impl FnOnce() -> bool) -> io::Result<()> {
    let exit = ExitWatch::new(pid)?;
    if !verify() {
        return Err(io::Error::other("server identity changed"));
    }
    imp::signal(exit.0.as_raw_fd(), pid)?;
    exit.wait(Duration::from_secs(5))
}

/// A kernel exit subscription that owns its descriptor.
pub struct ExitWatch(OwnedFd);
impl ExitWatch {
    /// Subscribe to one positive PID before its identity is checked again.
    pub fn new(pid: u32) -> io::Result<Self> {
        if pid == 0 || pid > i32::MAX as u32 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        imp::watch(pid).map(Self)
    }
    /// Borrow the subscription for a fixture's bounded stage poll.
    pub fn raw_fd(&self) -> i32 {
        self.0.as_raw_fd()
    }
    /// Wait for exit; an alive or silent producer returns a timeout at the ceiling.
    pub fn wait(&self, ceiling: Duration) -> io::Result<()> {
        let mut event = libc::pollfd {
            fd: self.raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = checked(unsafe {
            libc::poll(
                &mut event,
                1,
                ceiling.as_millis().min(i32::MAX as u128) as i32,
            )
        })?;
        if result == 0 {
            Err(io::ErrorKind::TimedOut.into())
        } else if event.revents & libc::POLLIN != 0 {
            Ok(())
        } else {
            Err(io::Error::other("invalid exit notification"))
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    pub fn birth(pid: u32) -> Option<u64> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat.get(stat.rfind(')')? + 2..)?
            .split_whitespace()
            .nth(19)?
            .parse()
            .ok()
    }
    pub fn watch(pid: u32) -> io::Result<OwnedFd> {
        let fd = checked(unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32)?;
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
    pub fn signal(fd: i32, _pid: u32) -> io::Result<()> {
        checked(unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd,
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        } as i32)
        .map(|_| ())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    pub fn birth(pid: u32) -> Option<u64> {
        unsafe extern "C" {
            fn flock_process_facts(pid: i32, parent: *mut u32, birth: *mut u64) -> i32;
        }
        let (mut parent, mut start) = (0, 0);
        (unsafe { flock_process_facts(i32::try_from(pid).ok()?, &mut parent, &mut start) } == 1)
            .then_some(start)
    }
    pub fn watch(pid: u32) -> io::Result<OwnedFd> {
        let raw = checked(unsafe { libc::kqueue() })?;
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let event = libc::kevent {
            ident: pid as _,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        checked(unsafe {
            libc::kevent(raw, &event, 1, std::ptr::null_mut(), 0, std::ptr::null())
        })?;
        Ok(fd)
    }
    pub fn signal(_fd: i32, pid: u32) -> io::Result<()> {
        checked(unsafe { libc::kill(pid as i32, libc::SIGKILL) }).map(|_| ())
    }
}
