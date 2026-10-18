//! Cancellable advisory-lock acquisition using an owned descriptor-only helper.
use std::fs::{File, TryLockError};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::{net::UnixStream, process::CommandExt};
use std::path::Path;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

/// Internal worker argument; the worker receives no path or credential.
pub(crate) const WORKER_ARG: &str = "--client-token-lock";
use super::file_lock::LockFile;

/// A pending lock owns its helper until acquisition, cancellation or timeout.
pub(crate) struct PendingLock {
    file: Option<LockFile>,
    helper: Option<(Child, UnixStream)>,
}
impl PendingLock {
    /// Start without blocking the caller. Only a contended lock needs a helper.
    pub(crate) fn begin(file: File, executable: &Path) -> io::Result<Self> {
        let file = LockFile::new(file);
        let contended = match file.try_lock() {
            Ok(()) => false,
            Err(TryLockError::WouldBlock) => true,
            Err(TryLockError::Error(error)) => return Err(error),
        };
        let helper = if contended {
            let (parent, child) = UnixStream::pair()?;
            // Descriptor setup needs CommandExt; execution still uses the tracing funnel.
            #[allow(clippy::disallowed_methods)]
            let mut command = std::process::Command::new(executable);
            command
                .arg(WORKER_ARG)
                .stdin(Stdio::null())
                .stdout(Stdio::from(OwnedFd::from(child)))
                .stderr(Stdio::null());
            let fd = file.as_raw_fd();
            // SAFETY: pre_exec uses only async-signal-safe descriptor operations.
            unsafe {
                command.pre_exec(move || {
                    if libc::dup2(fd, 3) < 0 || libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            Some((
                crate::process::TracedCommand::from_command(command, "client-token-lock")
                    .spawn_traced()?,
                parent,
            ))
        } else {
            None
        };
        Ok(Self {
            file: Some(file),
            helper,
        })
    }
    /// Wait at most the supplied lock deadline; cleanup also runs when abandoned.
    pub(crate) fn finish(mut self, deadline: Duration) -> io::Result<LockFile> {
        if let Some((_, stream)) = &mut self.helper {
            // A completed peer may reject SO_RCVTIMEO while its ACK is buffered
            // on Darwin. Readiness waits work for both live and completed peers.
            wait_readable(stream, deadline)?;
            let mut ack = [0];
            stream.read_exact(&mut ack)?;
            if ack != [b'L'] {
                return Err(io::ErrorKind::InvalidData.into());
            }
        }
        self.file
            .take()
            .ok_or_else(|| io::ErrorKind::InvalidData.into())
    }
}
/// Wait for the sole-reader acknowledgment socket, including EOF and errors.
/// Interrupted polls remeasure the monotonic deadline; no retry extends it.
fn wait_readable(stream: &UnixStream, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let milliseconds = remaining
            .as_nanos()
            .div_ceil(1_000_000)
            .min(i32::MAX as u128) as i32;
        let mut descriptor = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: this live descriptor and the one-element output buffer are owned.
        let ready = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
        if ready > 0 {
            // Only finish() reads this endpoint, so readiness cannot be consumed
            // by another reader before the single-byte read (EOF also completes).
            return Ok(());
        }
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

impl Drop for PendingLock {
    fn drop(&mut self) {
        if let Some((child, _)) = &mut self.helper {
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}
/// Worker entry, before normal application initialization. Parent death closes IPC.
pub(crate) fn worker() -> io::Result<()> {
    // SAFETY: fcntl validates the descriptor before Rust takes ownership.
    if unsafe { libc::fcntl(3, libc::F_GETFD) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut address = std::mem::MaybeUninit::<libc::sockaddr_un>::zeroed();
    let mut length = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
    // SAFETY: writable address buffer and length; this also rejects ordinary CLI stdout.
    if unsafe { libc::getsockname(1, address.as_mut_ptr().cast(), &mut length) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful getsockname initialized the address family.
    if unsafe { address.assume_init() }.sun_family as i32 != libc::AF_UNIX {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    // SAFETY: this internal invocation owns descriptor 3, inherited from its parent.
    let file = unsafe { File::from_raw_fd(3) };
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.uid() != super::peer::effective_uid()
        || meta.mode() & 0o7777 != 0o600
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    // SAFETY: dup creates an independent owned reference to the inherited IPC socket.
    let fd = unsafe { libc::dup(1) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful dup transfers this fresh descriptor to UnixStream.
    let mut monitor = unsafe { UnixStream::from_raw_fd(fd) };
    std::thread::Builder::new()
        .name("lock-owner".into())
        .spawn(move || {
            let _ = monitor.read(&mut [0]);
            std::process::exit(crate::cli::exit::SUCCESS);
        })?;
    let file = LockFile::new(file);
    file.lock()?;
    std::io::stdout().write_all(b"L")?;
    // The acknowledged parent retains the same description and its unlock guard.
    file.close_after_transfer();
    Ok(())
}

#[cfg(test)]
use super::file_lock::inherited_holder;

#[cfg(test)]
mod tests;
