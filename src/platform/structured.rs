//! Private filesystem primitives for structured sessions (ADR-0036 §1).

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Transfer owned child pipes into nonblocking-file transport without raw fd copies.
pub(crate) fn child_pipes(child: &mut std::process::Child) -> io::Result<(File, File)> {
    use std::os::fd::OwnedFd;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing stdout pipe"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("missing stdin pipe"))?;
    Ok((
        File::from(OwnedFd::from(stdout)),
        File::from(OwnedFd::from(stdin)),
    ))
}

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

/// Open a resolved local path beneath an anchored home directory. Each component
/// is opened relative to the preceding descriptor without following symlinks,
/// so a concurrent directory replacement cannot redirect the open outside home.
pub(crate) fn open_regular_beneath(home: &Path, relative: &Path) -> io::Result<File> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Component;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(home)?;
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(io::Error::other("non-local rollout path component"));
        };
        let name = CString::new(name.as_bytes()).map_err(io::Error::other)?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | libc::O_CLOEXEC
            | if components.peek().is_some() {
                libc::O_DIRECTORY
            } else {
                0
            };
        // SAFETY: the directory descriptor is owned and the C string is terminated.
        let fd = unsafe { libc::openat(file.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful openat transfers a fresh descriptor exclusively here.
        file = unsafe { File::from_raw_fd(fd) };
    }
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("rollout path is not a regular file"));
    }
    Ok(file)
}

/// Local file identity, independent of names and timestamps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileIdentity(pub u64, pub u64);

/// Check an already-open rollout against its still-local primary path. Core
/// reader code never accesses platform-specific inode fields or follows links.
pub(crate) fn rollout_identity(file: &File, path: &Path) -> io::Result<FileIdentity> {
    let opened = file.metadata()?;
    let named = open_regular_file(path)?.metadata()?;
    if !opened.is_file()
        || path.canonicalize()? != path
        || (opened.dev(), opened.ino()) != (named.dev(), named.ino())
    {
        return Err(io::Error::other("rollout identity changed"));
    }
    Ok(FileIdentity(opened.dev(), opened.ino()))
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
    match check_private_dir(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match create_private_dir(path) {
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
                Err(error) => return Err(error),
            }
            check_private_dir(path)
        }
        result => result,
    }
}

/// Validate an existing private directory; absence is an error, never creation.
pub(crate) fn check_private_dir(path: &Path) -> io::Result<()> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
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

/// Set owned session pipes nonblocking so worker cancellation never waits for I/O.
pub(crate) fn set_nonblocking(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: the live file owns this descriptor; fcntl does not access memory.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: preserve existing flags and add nonblocking mode to the owned fd.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Reopen a private regular stream for recovery and append without following links.
pub(crate) fn open_append_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .append(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    // SAFETY: geteuid has no preconditions.
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o7777 != 0o600
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "session stream requires owner-only regular file",
        ));
    }
    Ok(file)
}

/// Remove a validated Claude transient. Sockets must use the observed cc-socks
/// namespace under the temp/runtime roots and the dead session's exact pid name.
/// Root aliases such as /tmp are resolved; linked namespace/final entries fail.
pub(crate) fn remove_transient(path: &Path, socket_pid: Option<u32>) -> io::Result<()> {
    let mut roots = vec![std::env::temp_dir(), PathBuf::from("/tmp")];
    if let Some(root) = std::env::var_os("XDG_RUNTIME_DIR") {
        roots.push(root.into());
    }
    remove_transient_in(path, socket_pid, &roots)
}

fn remove_transient_in(path: &Path, socket_pid: Option<u32>, roots: &[PathBuf]) -> io::Result<()> {
    use std::os::unix::fs::FileTypeExt;
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    if meta.uid() != uid
        || (if socket_pid.is_some() {
            !meta.file_type().is_socket()
        } else {
            !meta.is_file()
        })
        || (socket_pid.is_none() && path.canonicalize()? != path)
    {
        return Err(io::Error::other("unsafe Claude transient type/owner"));
    }
    if let Some(pid) = socket_pid {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("socket parent missing"))?;
        let name = parent.file_name().and_then(|n| n.to_str());
        if !path.is_absolute()
            || path.file_name() != Some(std::ffi::OsStr::new(&format!("{pid}.sock")))
            || !(name == Some("cc-socks") || name == Some(format!("cc-socks-{uid}").as_str()))
        {
            return Err(io::Error::other("unsafe Claude socket path"));
        }
        check_private_dir(parent)?;
        let canonical = path.canonicalize()?;
        let root = parent
            .parent()
            .ok_or_else(|| io::Error::other("socket root missing"))?
            .canonicalize()?;
        if !roots
            .iter()
            .filter_map(|root| root.canonicalize().ok())
            .any(|allowed| root.starts_with(allowed))
            || canonical
                != root
                    .join(
                        parent
                            .file_name()
                            .ok_or_else(|| io::Error::other("socket namespace missing"))?,
                    )
                    .join(
                        path.file_name()
                            .ok_or_else(|| io::Error::other("socket name missing"))?,
                    )
        {
            return Err(io::Error::other("Claude socket outside allowed roots"));
        }
    }
    fs::remove_file(path)
}

/// Read a pinned replay descriptor without moving its shared append offset.
pub(crate) fn read_file_at(file: &File, bytes: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(bytes, offset)
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

    #[test]
    fn socket_cleanup_resolves_root_aliases_but_refuses_escape_and_namespace_links() {
        use std::os::unix::{fs::symlink, net::UnixListener};
        struct Root(PathBuf);
        impl Drop for Root {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let mut entropy = [0u8; 6];
        getrandom::fill(&mut entropy).expect("fixture entropy");
        let suffix: String = entropy.iter().map(|byte| format!("{byte:02x}")).collect();
        let root = Root(
            std::env::temp_dir()
                .canonicalize()
                .expect("temp root")
                .join(suffix),
        );
        create_private_dir(&root.0).expect("short owned root");
        let real = root.0.join("real");
        create_private_dir(&real).expect("real root");
        let sockets = real.join("cc-socks");
        create_private_dir(&sockets).expect("socket namespace");
        let alias = root.0.join("alias");
        symlink(&real, &alias).expect("root alias like temporary directory alias");
        let socket = sockets.join("77.sock");
        let listener = UnixListener::bind(&socket).expect("short fixture socket");
        assert!(remove_transient_in(
            &alias.join("cc-socks/77.sock"),
            Some(77),
            &[root.0.join("outside")]
        )
        .is_err());
        assert!(socket.exists(), "outside allowed root preserved");
        remove_transient_in(
            &alias.join("cc-socks/77.sock"),
            Some(77),
            std::slice::from_ref(&alias),
        )
        .expect("root alias accepted");
        assert!(!socket.exists());
        drop(listener);
        let outside = root.0.join("outside");
        create_private_dir(&outside).expect("outside namespace");
        fs::remove_dir(&sockets).expect("remove empty namespace");
        symlink(&outside, &sockets).expect("redirected namespace");
        let listener = UnixListener::bind(outside.join("77.sock")).expect("outside socket");
        assert!(remove_transient_in(
            &alias.join("cc-socks/77.sock"),
            Some(77),
            std::slice::from_ref(&alias)
        )
        .is_err());
        assert!(outside.join("77.sock").exists());
        drop(listener);
    }
}
