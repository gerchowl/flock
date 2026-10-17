//! Anchored private client-token namespace; no filesystem path follows a link.
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub(crate) struct Namespace {
    root: PathBuf,
    directory: File,
}
impl Namespace {
    pub(crate) fn open(data: &Path) -> io::Result<Self> {
        let root = data.canonicalize()?.join("client-tokens");
        super::structured::ensure_private_dir(&root)?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&root)?;
        let result = Self { root, directory };
        result.check()?;
        File::open(result.root.parent().ok_or(io::ErrorKind::InvalidInput)?)?.sync_all()?;
        Ok(result)
    }
    fn check(&self) -> io::Result<()> {
        super::structured::check_private_dir(&self.root)?;
        let current = fs::symlink_metadata(&self.root)?;
        let pinned = self.directory.metadata()?;
        if current.dev() != pinned.dev() || current.ino() != pinned.ino() {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        Ok(())
    }
    fn name(name: &str) -> io::Result<CString> {
        if name.is_empty() || name.contains('/') || matches!(name, "." | "..") {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        CString::new(name).map_err(|_| io::ErrorKind::InvalidInput.into())
    }
    pub(crate) fn file(&self, name: &str, create: bool) -> io::Result<File> {
        self.check()?;
        let name = Self::name(name)?;
        let flags = libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if create {
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL
            } else {
                libc::O_RDONLY
            };
        // SAFETY: owned directory descriptor and terminated single component.
        let fd = unsafe { libc::openat(self.directory.as_raw_fd(), name.as_ptr(), flags, 0o600) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            return Err(if error.raw_os_error() == Some(libc::ELOOP) {
                io::ErrorKind::PermissionDenied.into()
            } else {
                error
            });
        }
        // SAFETY: successful openat transfers this fresh descriptor to File.
        let file = unsafe { File::from_raw_fd(fd) };
        if create {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        let meta = file.metadata()?;
        if !meta.is_file()
            || meta.uid() != super::peer::effective_uid()
            || meta.mode() & 0o7777 != 0o600
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        Ok(file)
    }
    pub(crate) fn names(&self) -> io::Result<Vec<String>> {
        self.check()?;
        fs::read_dir(&self.root)?
            .map(|e| {
                e?.file_name()
                    .into_string()
                    .map_err(|_| io::ErrorKind::InvalidData.into())
            })
            .collect()
    }
    pub(crate) fn publish(&self, from: &str, to: &str, replace: bool) -> io::Result<()> {
        self.check()?;
        let (from, to) = (Self::name(from)?, Self::name(to)?);
        let fd = self.directory.as_raw_fd();
        // SAFETY: both terminated components are relative to the owned directory.
        let result = unsafe {
            if replace {
                libc::renameat(fd, from.as_ptr(), fd, to.as_ptr())
            } else {
                libc::linkat(fd, from.as_ptr(), fd, to.as_ptr(), 0)
            }
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        self.sync()
    }
    pub(crate) fn remove(&self, name: &str) -> io::Result<()> {
        self.check()?;
        let name = Self::name(name)?;
        // SAFETY: a terminated component relative to the owned descriptor.
        if unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        self.sync()
    }
    fn sync(&self) -> io::Result<()> {
        self.directory.sync_all()
    }
}
