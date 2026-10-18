//! Startup-only credential removal from the process environment.
use std::{ffi::CStr, io};
use zeroize::Zeroizing;

/// Capture a secret into zeroizing storage, overwrite its original value bytes,
/// then unlink the libc environment entry. Volatile filler writes retain the
/// original terminator and prevent optimization from removing the scrub.
///
/// This removes the value from this child's exec-time environment as inspected
/// by KERN_PROCARGS2 or `/proc`, as well as from subsequent getenv/inheritance.
/// It cannot close the window between exec and this call, or scrub the harness
/// process that supplied the environment. It is not a same-UID security boundary.
///
/// # Safety
/// Call only during single-threaded startup, before any copy or mutation of this
/// entry and before any worker/runtime starts. No other code may hold a
/// borrowed pointer to this entry. The writable getenv allocation remains live
/// until unsetenv, and must not be relocated during the capture and scrub.
pub(crate) unsafe fn take(name: &CStr) -> io::Result<Option<Zeroizing<Vec<u8>>>> {
    // SAFETY: the caller guarantees exclusive startup access to the environment.
    let pointer = unsafe { libc::getenv(name.as_ptr()) };
    if pointer.is_null() {
        return Ok(None);
    }
    // SAFETY: getenv returns a live NUL-terminated value. No mutation intervenes.
    let bytes = unsafe { CStr::from_ptr(pointer) }.to_bytes();
    let length = bytes.len();
    let captured = Zeroizing::new(bytes.to_vec());
    for offset in 0..length {
        // SAFETY: every offset is within the writable value allocation. Keep
        // the terminator intact so environment readers can still parse entries.
        unsafe { pointer.add(offset).write_volatile(b'0' as libc::c_char) };
    }
    // SAFETY: all original value bytes have been overwritten, and the caller
    // guarantees no concurrent environment access while libc unlinks the entry.
    if unsafe { libc::unsetenv(name.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(captured))
}
