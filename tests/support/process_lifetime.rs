//! Kernel process identity and bounded exit notifications for sandbox cleanup.
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    (unsafe { libc::kill(pid as i32, 0) == 0 })
        || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub fn birth(pid: u32) -> Option<u64> {
    imp::birth(pid)
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

pub struct ExitWatch(OwnedFd);
impl ExitWatch {
    pub fn new(pid: u32) -> io::Result<Self> {
        imp::watch(pid).map(Self)
    }
    pub fn raw_fd(&self) -> i32 {
        self.0.as_raw_fd()
    }
    pub fn wait(&self, ceiling: Duration) -> io::Result<()> {
        imp::wait(self.raw_fd(), ceiling)
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
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
    }
    pub fn signal(fd: i32, _pid: u32) -> io::Result<()> {
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd,
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    pub fn wait(fd: i32, ceiling: Duration) -> io::Result<()> {
        let mut event = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        match unsafe {
            libc::poll(
                &mut event,
                1,
                ceiling.as_millis().min(i32::MAX as u128) as i32,
            )
        } {
            1 => Ok(()),
            0 => Err(io::ErrorKind::TimedOut.into()),
            _ => Err(io::Error::last_os_error()),
        }
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
        let raw = unsafe { libc::kqueue() };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let event = libc::kevent {
            ident: pid as _,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        if unsafe { libc::kevent(raw, &event, 1, std::ptr::null_mut(), 0, std::ptr::null()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(fd)
    }
    pub fn signal(_fd: i32, pid: u32) -> io::Result<()> {
        if unsafe { libc::kill(pid as i32, libc::SIGKILL) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    pub fn wait(fd: i32, ceiling: Duration) -> io::Result<()> {
        let timeout = libc::timespec {
            tv_sec: ceiling.as_secs() as _,
            tv_nsec: ceiling.subsec_nanos() as _,
        };
        let mut event: libc::kevent = unsafe { std::mem::zeroed() };
        match unsafe { libc::kevent(fd, std::ptr::null(), 0, &mut event, 1, &timeout) } {
            1 => Ok(()),
            0 => Err(io::ErrorKind::TimedOut.into()),
            _ => Err(io::Error::last_os_error()),
        }
    }
}
