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

/// Prepare an IPC path, refusing foreign/non-socket paths and uncertain probes.
/// WIP #2531: connection refusal is also possible for a full live backlog on macOS.
/// Removal needs an authoritative ownership proof before this policy can land.
/// The owning directory must prevent other users replacing entries during bind/cleanup.
pub(crate) fn prepare_socket_path(
    path: &Path,
    busy_message: impl FnOnce(&Path) -> String,
) -> io::Result<()> {
    prepare_socket_path_with(
        path,
        busy_message,
        crate::platform::socket::probe_listener,
        crate::platform::peer::effective_uid(),
    )
}

fn prepare_socket_path_with(
    path: &Path,
    busy_message: impl FnOnce(&Path) -> String,
    probe: impl FnOnce(&Path) -> io::Result<()>,
    operator_uid: u32,
) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    require_owned_socket(&metadata, operator_uid)?;
    let original = identity(&metadata);
    match probe(path) {
        Ok(()) => return Err(io::Error::new(io::ErrorKind::AddrInUse, busy_message(path))),
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {}
        Err(error) => return Err(error),
    }
    // A refused connection proves only the observed socket stale, not a replacement.
    let current = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    require_owned_socket(&current, operator_uid)?;
    if identity(&current) != original {
        return Err(io::Error::new(io::ErrorKind::AddrInUse, busy_message(path)));
    }

    if let Err(err) = fs::remove_file(path) {
        if err.kind() != io::ErrorKind::NotFound {
            return Err(err);
        }
    }

    Ok(())
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
