//! Scoped advisory file locks shared by every file-lock owner.
use std::fs::{File, TryLockError};
use std::io;
use std::os::fd::AsRawFd;

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
