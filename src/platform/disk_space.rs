//! Filesystem-local free space without mount enumeration.
use std::{ffi::CString, io, os::unix::ffi::OsStrExt, path::Path};

pub(crate) fn available(directory: &Path) -> io::Result<u64> {
    let path = CString::new(directory.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "directory contains NUL"))?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: path is NUL-terminated and stats points to writable storage.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful statvfs initialized the entire structure.
    let stats = unsafe { stats.assume_init() };
    let bytes = u128::from(stats.f_bavail) * u128::from(stats.f_frsize);
    Ok(bytes.min(u128::from(u64::MAX)) as u64)
}
