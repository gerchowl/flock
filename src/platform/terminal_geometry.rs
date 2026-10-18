//! Controlling-tty geometry queries with preserved errno and explicit loss.
use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

/// A tty query failure distinguishes terminal loss from recoverable query errors.
#[derive(Debug)]
pub(crate) enum GeometryError {
    /// The controlling terminal is no longer usable.
    Gone(io::Error),
    /// No evidence of terminal loss; retain a previously validated geometry.
    Transient(io::Error),
}

fn classify(error: io::Error) -> GeometryError {
    match error.raw_os_error() {
        Some(libc::EIO | libc::ENXIO | libc::ENOTTY) => GeometryError::Gone(error),
        _ => GeometryError::Transient(error),
    }
}

/// Query the controlling tty rather than a possibly redirected stdin/stdout.
/// Unlike a terminal-library fallback, ioctl preserves the errno needed to
/// distinguish a gone tty from a transient failure. Returned dimensions include
/// columns, rows, pixel width and pixel height; zero pixel dimensions are unknown.
pub(crate) fn query() -> Result<(u16, u16, u16, u16), GeometryError> {
    let tty = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC)
        .open("/dev/tty")
        .map_err(classify)?;
    let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    // SAFETY: tty is borrowed and TIOCGWINSZ writes one winsize to this buffer.
    if unsafe { libc::ioctl(tty.as_raw_fd(), libc::TIOCGWINSZ, size.as_mut_ptr()) } < 0 {
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

/// Force termination through the owned standard child handle. The PTY library's
/// cloned killer sends SIGHUP, which terminal signal handlers may ignore.
#[cfg(test)]
pub(crate) fn terminate_test_child(
    child: &mut (dyn portable_pty::Child + Send + Sync),
) -> io::Result<()> {
    let child = child
        .as_any_mut()
        .downcast_mut::<std::process::Child>()
        .expect("the supported Unix PTY backend owns a standard child");
    if child.try_wait()?.is_none() {
        child.kill()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
