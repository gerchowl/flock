//! An exec-isolated descriptor holder for lock-lifetime regression tests.
use super::*;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::{net::UnixStream, process::CommandExt};
use std::process::{Child, Stdio};
use std::time::Duration;

const CEILING: Duration = Duration::from_secs(5);
const FILE_FD: i32 = 3;
const CONTROL_FD: i32 = 4;
const LOCK_ENV: &str = "FLOCK_TEST_HOLDER_LOCK";
const INODE_ENV: &str = "FLOCK_TEST_HOLDER_INODE";

/// Own exactly one exec'd helper; retain no parent copy of the lock description.
pub(crate) struct Holder {
    /// PID kept owned and unreaped until this fixture drops.
    pub(crate) pid: libc::pid_t,
    /// Child descriptor, unaffected by descriptor reuse in the parent.
    pub(crate) descriptor: i32,
    /// Inode verified by the helper before every acknowledgment.
    pub(crate) inode: u64,
    child: Child,
    control: UnixStream,
}
impl Holder {
    /// Pass only the target description and IPC through exec to a single fixture.
    /// Ordinary inherited CLOEXEC descriptors close before the readiness reply.
    pub(crate) fn spawn(file: &(impl AsRawFd + ?Sized)) -> io::Result<Self> {
        Self::spawn_with_lock(file, false)
    }
    /// Acquire in the exec'd helper, proving contention from a live lock owner.
    pub(crate) fn lock(file: &(impl AsRawFd + ?Sized)) -> io::Result<Self> {
        Self::spawn_with_lock(file, true)
    }
    fn spawn_with_lock(file: &(impl AsRawFd + ?Sized), acquire: bool) -> io::Result<Self> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
        // SAFETY: descriptor is borrowed and the output buffer has the right size.
        if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful fstat initialized the buffer.
        let inode = unsafe { stat.assume_init() }.st_ino;
        let (parent, child) = UnixStream::pair()?;
        // Allocate sources above both destinations so dup2 cannot overwrite IPC.
        let pass_file = pass_descriptor(file.as_raw_fd())?;
        let pass_control = pass_descriptor(child.as_raw_fd())?;
        let file_fd = pass_file.as_raw_fd();
        let control_fd = pass_control.as_raw_fd();
        // Descriptor remapping requires CommandExt; spawn still uses tracing.
        #[allow(clippy::disallowed_methods)]
        let mut command = std::process::Command::new(std::env::current_exe()?);
        let module = module_path!()
            .split_once("::")
            .expect("crate module path")
            .1;
        command
            .args([
                "--exact",
                &format!("{module}::holder_worker"),
                "--nocapture",
                "--ignored",
                "--test-threads=1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (key, _) in std::env::vars_os() {
            if key.to_str().is_some_and(|key| key.starts_with("FLOCK_")) {
                command.env_remove(key);
            }
        }
        command.env(INODE_ENV, inode.to_string());
        if acquire {
            command.env(LOCK_ENV, "1");
        }
        // SAFETY: only async-signal-safe descriptor operations run before exec.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(file_fd, FILE_FD) < 0
                    || libc::dup2(control_fd, CONTROL_FD) < 0
                    || libc::fcntl(FILE_FD, libc::F_SETFD, 0) < 0
                    || libc::fcntl(CONTROL_FD, libc::F_SETFD, 0) < 0
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child_process =
            crate::process::TracedCommand::from_command(command, "lock-inherited-holder")
                .spawn_traced()?;
        drop((pass_file, pass_control, child));
        let mut holder = Self {
            pid: child_process.id() as libc::pid_t,
            descriptor: FILE_FD,
            inode,
            child: child_process,
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
    /// Verify the same child still holds the same description after the operation.
    pub(crate) fn verify(&mut self) -> io::Result<()> {
        self.control.write_all(b"C")?;
        self.acknowledgment()
    }
}
impl Drop for Holder {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}
fn pass_descriptor(fd: i32) -> io::Result<OwnedFd> {
    // SAFETY: fcntl duplicates the borrowed descriptor to a fresh owned one.
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful F_DUPFD_CLOEXEC returned an unowned fresh descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}

#[test]
#[ignore = "exec-only descriptor holder launched with explicit fixture descriptors"]
fn holder_worker() {
    let inode = std::env::var_os(INODE_ENV).expect("only the holder fixture launches this worker");
    let inode: u64 = inode.to_str().unwrap().parse().unwrap();
    // SAFETY: only Holder::spawn invokes this fixture, explicitly owning 3 and 4.
    let (file, mut control) = unsafe {
        (
            File::from_raw_fd(FILE_FD),
            UnixStream::from_raw_fd(CONTROL_FD),
        )
    };
    // The deliberate exec transfer cleared CLOEXEC; restore it in the new process.
    // SAFETY: the helper owns the live file descriptor.
    assert_eq!(
        unsafe { libc::fcntl(FILE_FD, libc::F_SETFD, libc::FD_CLOEXEC) },
        0
    );
    let _lock = std::env::var_os(LOCK_ENV).map(|_| {
        let lock = LockFile::new(file.try_clone().unwrap());
        lock.try_lock()
            .expect("helper must acquire before reporting ready");
        lock
    });
    control.set_read_timeout(Some(CEILING)).unwrap();
    loop {
        assert_eq!(file.metadata().unwrap().ino(), inode);
        // SAFETY: the helper retains this descriptor for its whole lifetime.
        let flags = unsafe { libc::fcntl(FILE_FD, libc::F_GETFD) };
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0);
        control.write_all(b"R").unwrap();
        let mut byte = [0];
        match control.read_exact(&mut byte) {
            Ok(()) => assert_eq!(byte, [b'C']),
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return,
            Err(err) => panic!("holder control failed: {err}"),
        }
    }
}

#[test]
fn inherited_holder_keeps_no_unrelated_descriptor() {
    let root = crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let file = File::create(root.path().join("target")).unwrap();
    let (mut observer, unrelated) = UnixStream::pair().unwrap();
    observer
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut holder = Holder::spawn(&file).unwrap();
    drop(unrelated);
    let end = observer.read(&mut [0]);
    let alive = holder.verify();
    drop(holder);
    alive.unwrap();
    assert_eq!(
        end.unwrap(),
        0,
        "helper retained an unrelated socket endpoint"
    );
}
