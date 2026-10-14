//! Private filesystem primitives for structured sessions (ADR-0036 §1).

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Open the token without following links or blocking on a FIFO, then validate
/// the opened inode. Checking the path before opening would race replacement.
pub(crate) fn open_token(path: &Path) -> io::Result<File> {
    let file = open_regular_file(path)?;
    let meta = file.metadata()?;
    // SAFETY: geteuid has no preconditions and accesses no Rust memory.
    let uid = unsafe { libc::geteuid() };
    if meta.uid() != uid || meta.mode() & 0o7777 != 0o600 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "structured token requires a regular file owned by the current user with mode 0600",
        ));
    }
    Ok(file)
}

/// Open a regular session file without following symlinks or blocking on FIFOs.
pub(crate) fn open_regular_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "structured session requires a regular file",
        ));
    }
    Ok(file)
}

/// Create a new owner-only directory, without a permissive creation window.
pub(crate) fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(path)?;
    // Restore owner access if a restrictive umask removed it at creation.
    if let Err(err) = fs::set_permissions(path, fs::Permissions::from_mode(0o700)) {
        if let Err(cleanup) = fs::remove_dir(path) {
            crate::logging::structured_config_cleanup_failed(&cleanup);
        }
        return Err(err);
    }
    Ok(())
}

/// Create a new owner-only file. Existing entries, including symlinks, fail.
pub(crate) fn create_private_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

/// Create or validate a persistent private directory without following symlinks.
/// Existing permissions and ownership are never silently changed.
pub(crate) fn ensure_private_dir(path: &Path) -> io::Result<()> {
    let open = || {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)
    };
    let directory = match open() {
        Ok(directory) => directory,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            match create_private_dir(path) {
                Ok(()) => (),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => (),
                Err(err) => return Err(err),
            }
            open()?
        }
        Err(err) => return Err(err),
    };
    // SAFETY: geteuid has no preconditions and accesses no Rust memory.
    validate_private_dir(&directory.metadata()?, unsafe { libc::geteuid() })
}

fn validate_private_dir(meta: &fs::Metadata, uid: libc::uid_t) -> io::Result<()> {
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o7777 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "structured profile requires a directory owned by the current user with mode 0700",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::structured::launch::SessionConfigDir;

    #[test]
    fn private_creation_survives_a_restrictive_umask() {
        struct RestoreMask(libc::mode_t);
        impl Drop for RestoreMask {
            fn drop(&mut self) {
                // SAFETY: restoring the process-local umask in this nextest process.
                unsafe { libc::umask(self.0) };
            }
        }
        let parent = SessionConfigDir::create(&std::env::temp_dir()).expect("test parent");
        // SAFETY: nextest isolates this test in its own process.
        let _restore = RestoreMask(unsafe { libc::umask(0o777) });
        let path = parent.path().join("private");
        create_private_dir(&path).expect("owner access restored");
        let file = create_private_file(&path.join("settings.json")).expect("owner file");
        assert_eq!(
            fs::metadata(&path).expect("directory").mode() & 0o777,
            0o700
        );
        assert_eq!(file.metadata().expect("file").mode() & 0o777, 0o600);
    }

    #[test]
    fn persistent_directory_owner_validation_refuses_another_uid() {
        let directory = SessionConfigDir::create(&std::env::temp_dir()).expect("directory fixture");
        let meta = fs::metadata(directory.path()).expect("fixture metadata");
        assert!(validate_private_dir(&meta, meta.uid()).is_ok());
        assert_eq!(
            validate_private_dir(&meta, meta.uid().wrapping_add(1))
                .expect_err("another owner must be refused")
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn token_fifo_is_refused_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::sync::mpsc;
        use std::time::Duration;

        let parent = SessionConfigDir::create(&std::env::temp_dir()).expect("test parent");
        let config_dir = parent.path().to_owned();
        fs::create_dir(config_dir.join("sessions")).expect("registry directory");
        let path = config_dir.join("sessions/123.json");
        let name = CString::new(path.as_os_str().as_bytes()).expect("fixture path");
        // SAFETY: name is a valid NUL-terminated path in our own private directory.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            tx.send((
                open_token(&path).map(|_| ()),
                crate::structured::launch::registry_has_messaging_socket(&config_dir, 123),
            ))
            .expect("report open results");
        });
        let result = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("FIFO open must not block");
        assert_eq!(
            result.0.expect_err("non-regular token").kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            result.1.expect_err("non-regular registry").kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
