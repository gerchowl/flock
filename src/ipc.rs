use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SocketFileIdentity {
    dev: u64,
    ino: u64,
}

/// Lifetime ownership of a pathname listener, including across SCM_RIGHTS handoff.
/// The sibling `.lock` is never unlinked. Only close releases ownership; do not
/// explicitly unlock a descriptor shared with an importing process.
#[derive(Clone, Debug)]
pub(crate) struct SocketOwner(std::sync::Arc<fs::File>, std::path::PathBuf);

impl SocketOwner {
    /// Acquire BEFORE bind or stale removal. The parent directory must exclude
    /// entry replacement by other users. A held lock is busy, regardless of probes.
    pub(crate) fn acquire(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut lock = path.as_os_str().to_os_string();
        lock.push(".lock");
        let file = crate::platform::socket::open_owner(Path::new(&lock))?;
        Self::validate(&file, Path::new(&lock))?;
        crate::platform::socket::lock_owner(&file).map_err(|e| {
            if e.kind() == io::ErrorKind::WouldBlock {
                io::Error::new(io::ErrorKind::AddrInUse, "socket ownership is busy")
            } else {
                e
            }
        })?;
        Self::validate(&file, Path::new(&lock))?;
        Ok(Self(std::sync::Arc::new(file), path.to_owned()))
    }
    fn validate(file: &fs::File, path: &Path) -> io::Result<()> {
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.uid() != crate::platform::peer::effective_uid()
            || metadata.permissions().mode() & 0o777 != 0o600
            || identity(&metadata) != identity(&fs::symlink_metadata(path)?)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "invalid socket ownership file",
            ));
        }
        Ok(())
    }
    /// Duplicate the locked open description for handoff, retaining rollback ownership.
    pub(crate) fn duplicate(&self) -> io::Result<std::os::fd::OwnedFd> {
        Ok(self.0.try_clone()?.into())
    }
    /// Adopt an SCM_RIGHTS ownership descriptor before rebinding the pathname.
    pub(crate) fn take(path: &Path, fds: &mut Vec<std::os::fd::RawFd>) -> io::Result<Self> {
        use std::os::fd::{AsRawFd, FromRawFd};
        let fd = fds
            .pop()
            .ok_or_else(|| io::Error::other("missing socket ownership descriptor"))?;
        // SAFETY: recv_fds transfers exclusive ownership of each received descriptor.
        let file = unsafe { fs::File::from_raw_fd(fd) };
        crate::platform::received_fd::close_on_exec(file.as_raw_fd())?;
        let mut lock = path.as_os_str().to_os_string();
        lock.push(".lock");
        Self::validate(&file, Path::new(&lock))?;
        crate::platform::socket::lock_owner(&file)?;
        Ok(Self(std::sync::Arc::new(file), path.to_owned()))
    }
    /// Reclaim only a same-user socket while holding authoritative ownership.
    pub(crate) fn prepare(&self, path: &Path) -> io::Result<()> {
        if path != self.1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "socket ownership path mismatch",
            ));
        }
        let metadata = match fs::symlink_metadata(path) {
            Ok(value) => value,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        require_owned_socket(&metadata, crate::platform::peer::effective_uid())?;
        let original = identity(&metadata);
        if socket_file_identity(path)? != original {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "socket path changed during reclamation",
            ));
        }
        remove_socket_file_if_owned(path, original)
    }
}

/// Acquire lifetime ownership and reclaim a stale pathname. Keep the returned
/// guard until after listener shutdown and inode-safe pathname cleanup.
pub(crate) fn prepare_socket_path(
    path: &Path,
    busy_message: impl FnOnce(&Path) -> String,
) -> io::Result<SocketOwner> {
    let owner = SocketOwner::acquire(path).map_err(|e| {
        if e.kind() == io::ErrorKind::AddrInUse {
            io::Error::new(e.kind(), busy_message(path))
        } else {
            e
        }
    })?;
    owner.prepare(path)?;
    Ok(owner)
}

fn require_owned_socket(metadata: &fs::Metadata, operator_uid: u32) -> io::Result<()> {
    if !metadata.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path is not a socket",
        ));
    }
    if metadata.uid() != operator_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "socket belongs to another user",
        ));
    }
    Ok(())
}
fn identity(metadata: &fs::Metadata) -> SocketFileIdentity {
    SocketFileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    }
}
/// Capture an operator-owned socket without following a symlink.
pub(crate) fn socket_file_identity(path: &Path) -> io::Result<SocketFileIdentity> {
    let metadata = fs::symlink_metadata(path)?;
    require_owned_socket(&metadata, crate::platform::peer::effective_uid())?;
    Ok(identity(&metadata))
}

/// Remove only the recorded operator-owned socket; a replacement path is untouched.
/// The caller must retain its socket ownership lock throughout cleanup.
/// Check device/inode without following symlinks, under the directory-owner policy.
pub(crate) fn remove_socket_file_if_owned(
    path: &Path,
    identity: SocketFileIdentity,
) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };

    if self::identity(&metadata) != identity {
        return Ok(());
    }

    require_owned_socket(&metadata, crate::platform::peer::effective_uid())?;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

pub(crate) fn restrict_socket_permissions(path: &Path, mode: u32) -> io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions)
}

#[cfg(test)]
mod tests;
