//! Descriptor-relative blob operations keep namespace replacements from redirecting I/O.
use std::ffi::CString;
use std::fs::{File, Metadata};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// A pinned, validated owner-only directory. All names are single components.
#[derive(Debug)]
pub(crate) struct Directory(File);
impl Directory {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        super::structured::ensure_private_dir(path)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)?;
        validate(&file.metadata()?, true)?;
        Ok(Self(file))
    }
    fn name(name: &str) -> io::Result<CString> {
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        CString::new(name).map_err(|_| io::ErrorKind::InvalidInput.into())
    }
    pub(crate) fn file(&self, name: &str, create: bool) -> io::Result<File> {
        let component = Self::name(name)?;
        let flags = libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if create {
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
            } else {
                libc::O_RDONLY
            };
        // SAFETY: directory owns its fd and name is a valid terminated component.
        let fd = unsafe { libc::openat(self.0.as_raw_fd(), component.as_ptr(), flags, 0o600) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful openat transfers this new descriptor to File.
        let file = unsafe { File::from_raw_fd(fd) };
        let validation = (|| {
            if create {
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            validate(&file.metadata()?, false)
        })();
        if let Err(error) = validation {
            if create {
                self.remove(name)?;
            }
            return Err(error);
        }
        Ok(file)
    }
    pub(crate) fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let (from, to) = (Self::name(from)?, Self::name(to)?);
        // SAFETY: both names are terminated and resolve relative to the owned fd.
        let result = unsafe {
            libc::renameat(
                self.0.as_raw_fd(),
                from.as_ptr(),
                self.0.as_raw_fd(),
                to.as_ptr(),
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        self.sync()
    }
    pub(crate) fn sync(&self) -> io::Result<()> {
        self.0.sync_all()
    }
    pub(crate) fn remove(&self, name: &str) -> io::Result<()> {
        let name = Self::name(name)?;
        // SAFETY: the component resolves inside the pinned owned directory.
        if unsafe { libc::unlinkat(self.0.as_raw_fd(), name.as_ptr(), 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
fn validate(meta: &Metadata, directory: bool) -> io::Result<()> {
    // SAFETY: geteuid has no preconditions.
    if meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o7777 != if directory { 0o700 } else { 0o600 }
        || if directory {
            !meta.is_dir()
        } else {
            !meta.is_file()
        }
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(())
}

/// Recognize filesystem refusals without leaking platform errno into core code.
pub(crate) fn unsafe_path(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied
            | io::ErrorKind::NotADirectory
            | io::ErrorKind::InvalidInput
    ) || error.raw_os_error() == Some(libc::ELOOP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::structured::launch::SessionConfigDir;
    use std::io::{Read, Write};
    #[test]
    fn exclusive_creation_collision_preserves_the_existing_entry() {
        let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let directory = Directory::open(&root.path().join("blobs")).unwrap();
        directory
            .file(".tmp-collision", true)
            .unwrap()
            .write_all(b"owned elsewhere")
            .unwrap();
        assert_eq!(
            directory.file(".tmp-collision", true).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        let mut bytes = Vec::new();
        directory
            .file(".tmp-collision", false)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"owned elsewhere");
    }
}
