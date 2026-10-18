//! Anchored private client-token namespace; no filesystem path follows a link.
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// A directory entry with a lossless, non-secret management identifier.
pub(crate) struct Entry {
    /// Hex-encoded raw filename, used only as an opaque management identifier.
    pub(crate) id: String,
    /// The filename when it is UTF-8, absent for arbitrary Unix byte names.
    pub(crate) name: Option<String>,
}
impl Entry {
    /// Preserve arbitrary directory-name bytes even on non-UTF-8 filesystems.
    pub(crate) fn from_name(name: OsString) -> Self {
        Self {
            id: name.as_bytes().iter().map(|b| format!("{b:02x}")).collect(),
            name: name.into_string().ok(),
        }
    }
}
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
    pub(crate) fn check(&self) -> io::Result<()> {
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
    pub(crate) fn names(&self) -> io::Result<Vec<Entry>> {
        self.check()?;
        fs::read_dir(&self.root)?
            .map(|entry| Ok(Entry::from_name(entry?.file_name())))
            .collect()
    }
    fn decode_entry(id: &str) -> io::Result<Vec<u8>> {
        if id.is_empty()
            || !id.len().is_multiple_of(2)
            || !id.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Ok(id
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let digit = |b: u8| {
                    if b.is_ascii_digit() {
                        b - b'0'
                    } else {
                        b.to_ascii_lowercase() - b'a' + 10
                    }
                };
                digit(pair[0]) * 16 + digit(pair[1])
            })
            .collect())
    }
    /// Unlink an exact listed token entry, without trusting its name or contents.
    pub(crate) fn remove_entry(&self, id: &str) -> io::Result<()> {
        let bytes = Self::decode_entry(id)?;
        if !bytes.ends_with(b".token") {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        self.unlink(OsStr::from_bytes(&bytes))
    }
    /// Remove only a temporary entry while the caller holds the writer lock.
    pub(crate) fn remove_temporary(&self, id: &str) -> io::Result<()> {
        self.check()?;
        let bytes = Self::decode_entry(id)?;
        if !bytes.starts_with(b".tmp-") {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let name = CString::new(bytes.clone()).map_err(|_| io::ErrorKind::InvalidInput)?;
        if bytes.contains(&b'/') {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: terminated component and owned directory; no final symlink is followed.
        if unsafe {
            libc::fstatat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful fstatat initialized stat.
        if unsafe { stat.assume_init() }.st_mode & libc::S_IFMT == libc::S_IFDIR {
            return Err(io::ErrorKind::IsADirectory.into());
        }
        self.unlink(OsStr::from_bytes(&bytes))
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
        self.unlink(OsStr::new(name))
    }
    fn unlink(&self, name: &OsStr) -> io::Result<()> {
        self.check()?;
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes.contains(&b'/') || matches!(bytes, b"." | b"..") {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let name = CString::new(bytes).map_err(|_| io::ErrorKind::InvalidInput)?;
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
