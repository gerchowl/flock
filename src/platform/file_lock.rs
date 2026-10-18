//! Scoped advisory file locks shared by every file-lock owner.
use std::fs::{File, TryLockError};
use std::io;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

/// Maximum interval between nonblocking attempts during a bounded lock wait.
/// The kernel offers no deadline for flock; parking avoids a busy spin.
const LOCK_RECHECK_INTERVAL: Duration = Duration::from_millis(10);

/// Own an advisory lock and explicitly issue `LOCK_UN` before closing the file.
///
/// `flock(2)` locks belong to the open file description. Closing only this file
/// leaves the lock held by a fork-inherited descriptor, even with `CLOEXEC` until
/// that child execs. Explicit unlock makes operation lifetime independent of
/// those references (ADR-0036 §5a). On-disk sidecar paths must remain stable.
///
/// Do not clone this description into another guard: either guard could unlock
/// the other's critical section. Forked children must exec or use `_exit`, not
/// run copied Rust destructors. The guard is not a cross-process ownership
/// transfer protocol; the token helper's acknowledged transfer keeps one parent
/// guard responsible for unlocking the shared description.
pub(crate) struct LockFile {
    file: File,
    unlock_on_drop: bool,
}
impl LockFile {
    /// Own the description, including while an acquisition helper is waiting.
    pub(crate) fn new(file: File) -> Self {
        Self {
            file,
            unlock_on_drop: true,
        }
    }
    /// Acquire exclusively, preserving the caller's existing blocking policy.
    pub(crate) fn lock(&self) -> io::Result<()> {
        self.file.lock()
    }
    /// Acquire exclusively within a monotonic budget. Contention at expiry is
    /// `WouldBlock`; callers retain responsibility for the operation's refusal code.
    pub(crate) fn lock_for(&self, budget: Duration) -> io::Result<()> {
        self.lock_for_with(budget, Instant::now, std::thread::park_timeout)
    }
    fn lock_for_with(
        &self,
        budget: Duration,
        now: impl Fn() -> Instant,
        mut wait: impl FnMut(Duration),
    ) -> io::Result<()> {
        let deadline = now() + budget;
        loop {
            match self.try_lock() {
                Ok(()) => return Ok(()),
                Err(TryLockError::Error(error)) => return Err(error),
                Err(TryLockError::WouldBlock) => (),
            }
            let remaining = deadline.saturating_duration_since(now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "file lock deadline exceeded",
                ));
            }
            wait(remaining.min(LOCK_RECHECK_INTERVAL));
        }
    }
    /// Acquire exclusively without waiting, preserving typed contention errors.
    pub(crate) fn try_lock(&self) -> Result<(), TryLockError> {
        self.file.try_lock()
    }
    /// Close this duplicate after an acknowledged transfer to another guard.
    /// Only the receiving owner may subsequently unlock the shared description.
    pub(super) fn close_after_transfer(mut self) {
        self.unlock_on_drop = false;
    }
}
impl AsRawFd for LockFile {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.file.as_raw_fd()
    }
}
impl Drop for LockFile {
    fn drop(&mut self) {
        if !self.unlock_on_drop {
            return;
        }
        if let Err(error) = self.file.unlock() {
            crate::logging::file_lock_unlock_failed(&error);
        }
    }
}

#[cfg(test)]
pub(crate) mod inherited_holder;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_acquisition_uses_monotonic_budget_with_live_holder() {
        let root =
            crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let path = root.path().join("live.lock");
        let file = File::create(&path).unwrap();
        let mut holder = inherited_holder::Holder::lock(&file).unwrap();
        drop(file);
        let contender = LockFile::new(File::open(&path).unwrap());
        let clock = std::cell::Cell::new(Instant::now());
        let budget = Duration::from_millis(25);
        let mut waits = Vec::new();
        let result = contender.lock_for_with(
            budget,
            || clock.get(),
            |duration| {
                waits.push(duration);
                clock.set(clock.get() + duration);
            },
        );
        holder.verify().unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(
            waits,
            [
                LOCK_RECHECK_INTERVAL,
                LOCK_RECHECK_INTERVAL,
                Duration::from_millis(5)
            ]
        );
        drop(holder);
        contender.try_lock().unwrap();
    }
}
