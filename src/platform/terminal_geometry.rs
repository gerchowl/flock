//! Selected-tty geometry queries with preserved errno and explicit loss.
use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

/// A tty query failure distinguishes terminal loss from recoverable query errors.
#[derive(Debug)]
pub(crate) enum GeometryError {
    /// The selected terminal is no longer usable.
    Gone(io::Error),
    /// No evidence of terminal loss; retain a previously validated geometry.
    Transient(io::Error),
    /// No terminal was found during initial source selection.
    Unavailable(io::Error),
}

fn classify(error: io::Error) -> GeometryError {
    match error.raw_os_error() {
        Some(libc::EIO | libc::ENXIO | libc::ENOTTY) => GeometryError::Gone(error),
        _ => GeometryError::Transient(error),
    }
}

/// Retain the selected tty so later errors refer to the tty actually in use.
/// Selection follows stdout, stdin, then the controlling tty, allowing a stdio
/// PTY in a session without a controlling tty. Selection failure is a startup
/// error, never evidence that an established observer should detach cleanly.
#[derive(Default)]
pub(crate) struct GeometryProbe {
    tty: Option<std::fs::File>,
}
impl GeometryProbe {
    /// Read columns, rows and pixel dimensions; zero cells are transient.
    pub(crate) fn query(&mut self) -> Result<(u16, u16, u16, u16), GeometryError> {
        if self.tty.is_none() {
            self.tty = Some(select_tty()?);
        }
        let Some(tty) = self.tty.as_ref() else {
            return Err(GeometryError::Unavailable(
                io::ErrorKind::NotConnected.into(),
            ));
        };
        query_fd(tty.as_raw_fd())
    }
}
fn select_tty() -> Result<std::fs::File, GeometryError> {
    select_tty_with(
        &[libc::STDOUT_FILENO, libc::STDIN_FILENO],
        tty_status,
        || {
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC)
                .open("/dev/tty")
        },
    )
}
fn tty_status(fd: std::os::fd::RawFd) -> io::Result<()> {
    // SAFETY: isatty validates the borrowed descriptor itself.
    if unsafe { libc::isatty(fd) } == 1 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
/// Continue through every candidate; retain an unexpected error only if no tty
/// can be selected. The callbacks keep errno and candidate ordering testable.
fn select_tty_with(
    candidates: &[std::os::fd::RawFd],
    mut check: impl FnMut(std::os::fd::RawFd) -> io::Result<()>,
    controlling: impl FnOnce() -> io::Result<std::fs::File>,
) -> Result<std::fs::File, GeometryError> {
    use std::os::fd::FromRawFd;
    let mut transient = None;
    for &fd in candidates {
        match check(fd) {
            Ok(()) => {
                // SAFETY: duplicate the selected tty atomically with CLOEXEC.
                let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
                if duplicate < 0 {
                    return Err(GeometryError::Transient(io::Error::last_os_error()));
                }
                // SAFETY: fcntl returned a fresh owned descriptor.
                return Ok(unsafe { std::fs::File::from_raw_fd(duplicate) });
            }
            Err(error) => {
                if !matches!(
                    error.raw_os_error(),
                    Some(libc::EIO | libc::ENXIO | libc::ENOTTY | libc::EBADF)
                ) {
                    transient = Some(error);
                }
            }
        }
    }
    controlling().map_err(|error| match error.raw_os_error() {
        Some(libc::EIO | libc::ENXIO | libc::ENOTTY | libc::ENOENT) => {
            transient.map_or(GeometryError::Unavailable(error), GeometryError::Transient)
        }
        _ => GeometryError::Transient(error),
    })
}
fn query_fd(fd: std::os::fd::RawFd) -> Result<(u16, u16, u16, u16), GeometryError> {
    let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    // SAFETY: the descriptor is borrowed and ioctl writes one winsize to the buffer.
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, size.as_mut_ptr()) } < 0 {
        return Err(classify(io::Error::last_os_error()));
    }
    // SAFETY: the successful ioctl initialized the winsize.
    let size = unsafe { size.assume_init() };
    if size.ws_col == 0 || size.ws_row == 0 {
        return Err(GeometryError::Transient(io::Error::new(
            io::ErrorKind::WouldBlock,
            "terminal dimensions are not available",
        )));
    }
    Ok((size.ws_col, size.ws_row, size.ws_xpixel, size.ws_ypixel))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    /// PENDIN is kernel pending-input state, not a configured terminal mode
    /// (the system termios header explicitly labels it state). All mode bits
    /// remain part of the restore assertion after a PTY switches canonical mode.
    pub(crate) fn configured_local_flags(flags: libc::tcflag_t) -> libc::tcflag_t {
        flags & !libc::PENDIN
    }
    /// Open the fixture's slave without acquiring it as the parent's controlling tty.
    pub(crate) fn pty_input(master: &dyn portable_pty::MasterPty) -> std::fs::File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC)
            .open(master.tty_name().expect("owned PTY has a slave path"))
            .unwrap()
    }
    /// A fresh session without a controlling tty, before the child creates threads.
    pub(crate) fn detached_command() -> std::process::Command {
        use std::os::unix::process::CommandExt;
        // Descriptor/session setup requires the standard command before tracing.
        #[allow(clippy::disallowed_methods)]
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        // SAFETY: setsid is async-signal-safe and runs only in the child before exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command
    }
    #[test]
    fn unexpected_isatty_errno_still_selects_the_next_tty() {
        let fixture =
            crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let file = std::fs::File::create(fixture.path().join("first-candidate")).unwrap();
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 40,
                cols: 132,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let candidate = pair.master.as_raw_fd().unwrap();
        let mut checked = Vec::new();
        let selected = select_tty_with(
            &[file.as_raw_fd(), candidate],
            |fd| {
                checked.push(fd);
                if fd == file.as_raw_fd() {
                    Err(io::Error::from_raw_os_error(libc::EINVAL))
                } else {
                    tty_status(fd)
                }
            },
            || panic!("must select the second tty before the controlling fallback"),
        )
        .unwrap();
        assert_eq!(checked, [file.as_raw_fd(), candidate]);
        assert_eq!(query_fd(selected.as_raw_fd()).unwrap(), (132, 40, 0, 0));
    }
    #[test]
    fn unexpected_candidate_error_is_retained_when_no_tty_is_available() {
        let result = select_tty_with(
            &[123],
            |_| Err(io::Error::from_raw_os_error(libc::EINVAL)),
            || Err(io::Error::from_raw_os_error(libc::ENXIO)),
        );
        assert!(
            matches!(result, Err(GeometryError::Transient(error)) if error.raw_os_error() == Some(libc::EINVAL))
        );
    }
    #[test]
    fn real_ioctl_notty_error_is_classified_as_loss() {
        let fixture =
            crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let file = std::fs::File::create(fixture.path().join("not-a-tty")).unwrap();
        let result = query_fd(file.as_raw_fd());
        assert!(
            matches!(&result, Err(GeometryError::Gone(error))
            if error.raw_os_error() == Some(libc::ENOTTY)),
            "{result:?}"
        );
    }
    #[test]
    fn only_terminal_loss_errnos_are_gone() {
        for errno in [libc::EIO, libc::ENXIO, libc::ENOTTY] {
            assert!(
                matches!(classify(io::Error::from_raw_os_error(errno)), GeometryError::Gone(error) if error.raw_os_error() == Some(errno))
            );
        }
        for errno in [libc::EINTR, libc::EAGAIN, libc::ENOMEM, libc::EINVAL] {
            assert!(
                matches!(classify(io::Error::from_raw_os_error(errno)), GeometryError::Transient(error) if error.raw_os_error() == Some(errno))
            );
        }
        assert!(matches!(
            classify(io::Error::other("unknown query failure")),
            GeometryError::Transient(_)
        ));
    }
}
