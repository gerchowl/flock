//! Bounded pre-exec descriptor holder for lock-lifetime regression tests.
use super::*;

const CEILING: Duration = Duration::from_secs(5);

/// Own exactly the child forked by this fixture; retain no parent lock descriptor.
pub(crate) struct Holder {
    /// Owned child process, never reaped until Drop.
    pub(crate) pid: libc::pid_t,
    /// Descriptor number in that child, independent of reuse in the parent.
    pub(crate) descriptor: i32,
    /// Inode checked by the child before every acknowledgment.
    pub(crate) inode: u64,
    control: UnixStream,
}
impl Holder {
    /// Pause the child before exec, preserving even CLOEXEC descriptors.
    pub(crate) fn spawn(file: &impl AsRawFd) -> io::Result<Self> {
        let descriptor = file.as_raw_fd();
        let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
        // SAFETY: the borrowed descriptor and output stat are live.
        if unsafe { libc::fstat(descriptor, stat.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful fstat initialized stat.
        let stat = unsafe { stat.assume_init() };
        let (parent, child) = UnixStream::pair()?;
        let (parent_fd, child_fd) = (parent.as_raw_fd(), child.as_raw_fd());
        // SAFETY: the child executes only async-signal-safe syscalls and _exit.
        // All allocation and Rust I/O are confined to the parent.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }
        if pid == 0 {
            // SAFETY: the child owns its copied descriptors; no Rust code returns.
            unsafe { child_main(parent_fd, child_fd, descriptor, stat.st_ino) }
        }
        drop(child);
        let mut holder = Self {
            pid,
            descriptor,
            inode: stat.st_ino,
            control: parent,
        };
        holder.control.set_read_timeout(Some(CEILING))?;
        holder.acknowledgment()?;
        Ok(holder)
    }
    fn acknowledgment(&mut self) -> io::Result<()> {
        let mut byte = [0];
        self.control.read_exact(&mut byte)?;
        if byte != [b'R'] {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(())
    }
    /// The same child still has the same descriptor open after the operation.
    pub(crate) fn verify(&mut self) -> io::Result<()> {
        self.control.write_all(b"C")?;
        self.acknowledgment()
    }
}
impl Drop for Holder {
    fn drop(&mut self) {
        // SAFETY: this positive pid is the unreaped child created above. It cannot
        // be reused, and killing it never targets a process owned by another test.
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
        }
        loop {
            // SAFETY: reap only our own child after killing its bounded poll loop.
            let result = unsafe { libc::waitpid(self.pid, std::ptr::null_mut(), 0) };
            if result >= 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                break;
            }
        }
    }
}
unsafe fn child_main(parent: i32, socket: i32, file: i32, inode: u64) -> ! {
    // SAFETY: the forked child has its own descriptor table. Only syscalls touch
    // these buffers and descriptors, and every wait is bounded by CEILING.
    unsafe {
        libc::close(parent);
        loop {
            let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
            let flags = libc::fcntl(file, libc::F_GETFD);
            if libc::fstat(file, stat.as_mut_ptr()) < 0
                || stat.assume_init().st_ino != inode
                || flags < 0
                || flags & libc::FD_CLOEXEC == 0
                || libc::write(socket, b"R".as_ptr().cast(), 1) != 1
            {
                libc::_exit(crate::cli::exit::FAILURE);
            }
            let mut poll = libc::pollfd {
                fd: socket,
                events: libc::POLLIN,
                revents: 0,
            };
            if libc::poll(&mut poll, 1, CEILING.as_millis() as i32) != 1 {
                libc::_exit(crate::cli::exit::FAILURE);
            }
            let mut byte = [0u8];
            if libc::read(socket, byte.as_mut_ptr().cast(), 1) != 1 || byte != [b'C'] {
                libc::_exit(crate::cli::exit::FAILURE);
            }
        }
    }
}
