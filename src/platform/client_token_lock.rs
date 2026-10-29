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
/// Shared admission worker, with the same descriptor-only ownership checks.
pub(crate) const SHARED_WORKER_ARG: &str = "--client-token-lock-shared";
/// Ceiling for cold exec and descriptor validation before the started byte.
/// Separate from lock contention: a silent/dead helper still fails boundedly.
/// Absolute caller budgets (hello) additionally cap this phase.
const HELPER_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// A management wait budgets only contention; hello retains its absolute ceiling.
enum WaitBudget {
    Lock(Duration),
    Until(Instant),
}
impl WaitBudget {
    fn remaining(&self) -> io::Result<Duration> {
        match self {
            Self::Lock(duration) => Ok(*duration),
            Self::Until(deadline) => deadline
                .checked_duration_since(Instant::now())
                .filter(|left| !left.is_zero())
                .ok_or_else(|| io::ErrorKind::TimedOut.into()),
        }
    }
    fn startup(&self) -> io::Result<Duration> {
        match self {
            Self::Lock(_) => Ok(HELPER_STARTUP_TIMEOUT),
            Self::Until(_) => Ok(self.remaining()?.min(HELPER_STARTUP_TIMEOUT)),
        }
    }
}
/// Owned advisory-lock description whose scope ends independently of child fds.
///
/// Close alone waits for every fork/dup reference to disappear. Explicit unlock
/// keeps admission and revocation bounded to their operation (ADR-0036 §5a).
/// An unlocked instance is also valid while an owned helper waits to acquire it.
/// Do not clone the descriptor; [`PendingLock`] is the sole transfer path.
pub(crate) struct LockFile(File);
impl LockFile {
    /// Own the description and release any lock on it when this guard drops.
    pub(crate) fn new(file: File) -> Self {
        Self(file)
    }
    /// Attempt admission without waiting; active contention retains its refusal.
    pub(crate) fn try_lock(&self) -> Result<(), TryLockError> {
        self.0.try_lock()
    }
    /// Share admission ownership without waiting; token mutation remains exclusive.
    pub(crate) fn try_lock_shared(&self) -> Result<(), TryLockError> {
        self.0.try_lock_shared()
    }
}
impl AsRawFd for LockFile {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.0.as_raw_fd()
    }
}
impl Drop for LockFile {
    fn drop(&mut self) {
        if let Err(error) = self.0.unlock() {
            crate::logging::client_token_unlock_failed(&error);
        }
    }
}

/// A pending lock owns its helper until acquisition, cancellation or timeout.
pub(crate) struct PendingLock {
    file: Option<LockFile>,
    helper: Option<(Child, UnixStream)>,
    #[cfg(test)]
    waiting: Option<std::sync::mpsc::Sender<()>>,
}
impl PendingLock {
    /// Start without blocking the caller. Only a contended lock needs a helper.
    pub(crate) fn begin(file: File, executable: &Path) -> io::Result<Self> {
        Self::begin_with(file, executable, false)
    }
    /// Wait for shared admission without changing exclusive mutation ownership.
    pub(crate) fn begin_shared(file: File, executable: &Path) -> io::Result<Self> {
        Self::begin_with(file, executable, true)
    }
    fn begin_with(file: File, executable: &Path, shared: bool) -> io::Result<Self> {
        let file = LockFile::new(file);
        let attempt = if shared {
            file.try_lock_shared()
        } else {
            file.try_lock()
        };
        let contended = match attempt {
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
                .arg(if shared {
                    SHARED_WORKER_ARG
                } else {
                    WORKER_ARG
                })
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
            #[cfg(test)]
            waiting: None,
        })
    }
    /// Observe entry into a contended lock's bounded IPC wait in fence tests.
    #[cfg(test)]
    pub(crate) fn notify_waiting(&mut self, ready: std::sync::mpsc::Sender<()>) {
        self.waiting = Some(ready);
    }
    /// Bound startup separately, then wait at most the supplied lock deadline.
    /// Cleanup also runs when abandoned.
    pub(crate) fn finish(self, deadline: Duration) -> io::Result<LockFile> {
        self.finish_notifying(deadline, || {})
    }
    /// Observe a started helper before reading its bounded lock acknowledgement.
    /// Startup has its own ceiling; the callback precedes the lock-read timeout.
    /// It must not wait for the helper or manage the guarded token store. Fixtures may pause here
    /// with a sender-owned gate, keeping unrelated test work outside the read budget.
    pub(crate) fn finish_notifying(
        self,
        deadline: Duration,
        on_wait: impl FnOnce(),
    ) -> io::Result<LockFile> {
        self.finish_with(WaitBudget::Lock(deadline), on_wait)
    }
    /// Keep an overall caller deadline across startup, callbacks and lock reads.
    pub(crate) fn finish_until(
        self,
        deadline: Instant,
        on_wait: impl FnOnce(),
    ) -> io::Result<LockFile> {
        self.finish_with(WaitBudget::Until(deadline), on_wait)
    }
    fn finish_with(mut self, budget: WaitBudget, on_wait: impl FnOnce()) -> io::Result<LockFile> {
        if let WaitBudget::Until(_) = &budget {
            budget.remaining()?;
        }
        if let Some((_, stream)) = &mut self.helper {
            #[cfg(test)]
            let waiting = self.waiting.take();
            wait_for_lock(stream, budget, || {
                on_wait();
                #[cfg(test)]
                if let Some(ready) = waiting {
                    ready.send(()).map_err(|_| io::ErrorKind::BrokenPipe)?;
                }
                Ok(())
            })?;
        }
        self.file
            .take()
            .ok_or_else(|| io::ErrorKind::InvalidData.into())
    }
}
/// IPC timeout seam; tests inject helper startup latency without wall-clock waits.
trait HelperStream: Read {
    fn read_timeout(&self, timeout: Duration) -> io::Result<()>;
}
impl HelperStream for UnixStream {
    fn read_timeout(&self, timeout: Duration) -> io::Result<()> {
        self.set_read_timeout(Some(timeout))
    }
}
fn wait_for_lock(
    stream: &mut impl HelperStream,
    budget: WaitBudget,
    on_wait: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    read_ack(stream, budget.startup()?, b'S')?;
    on_wait()?;
    read_ack(stream, budget.remaining()?, b'L')
}
fn read_ack(stream: &mut impl HelperStream, timeout: Duration, expected: u8) -> io::Result<()> {
    stream.read_timeout(timeout)?;
    let mut ack = [0];
    stream.read_exact(&mut ack).map_err(|e| {
        if e.kind() == io::ErrorKind::WouldBlock {
            io::ErrorKind::TimedOut.into()
        } else {
            e
        }
    })?;
    if ack != [expected] {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(())
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
    {
        let mut output = std::io::stdout().lock();
        output.write_all(b"S")?;
        output.flush()?;
    }
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == SHARED_WORKER_ARG)
    {
        file.lock_shared()?;
    } else {
        file.lock()?;
    }
    // The parent retains this same description through the operation. Unlocking
    // here would break the transfer; the parent guard releases it after admission.
    let mut output = std::io::stdout().lock();
    output.write_all(b"L")?;
    output.flush()
}

#[cfg(test)]
pub(crate) mod inherited_holder;

#[cfg(test)]
mod tests;
