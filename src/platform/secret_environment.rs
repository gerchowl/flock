//! Startup-only credential removal from the process environment.
use std::{ffi::CStr, io};
use zeroize::Zeroizing;

/// Capture the first matching value (the getenv result) into zeroizing storage,
/// overwrite every exact-key entry’s original value bytes, then unlink all copies.
/// Volatile filler writes retain each terminator and prevent optimization from
/// removing the scrub.
///
/// This removes all matching values from this child's exec-time environment
/// inspected by KERN_PROCARGS2 or `/proc`, and from subsequent getenv/inheritance.
/// It cannot close the window between exec and this call, or scrub the harness
/// process that supplied the environment. It is not a same-UID security boundary.
///
/// # Safety
/// Call only during single-threaded startup, before any copy or mutation of this
/// entry and before any worker/runtime starts. No other code may borrow these
/// entries. The writable getenv/environ allocations and pointer table must stay
/// live and must not be relocated until the capture and scrub finish.
pub(crate) unsafe fn take(name: &CStr) -> io::Result<Option<Zeroizing<Vec<u8>>>> {
    // SAFETY: the caller guarantees exclusive startup access to the environment.
    let pointer = unsafe { libc::getenv(name.as_ptr()) };
    // SAFETY: startup excludes concurrent environment mutation; libc keeps
    // these writable strings and the pointer table live until removal.
    unsafe {
        take_from(name, environment(), pointer, || {
            if libc::unsetenv(name.as_ptr()) != 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        })
    }
}

/// Shared operation on an exclusively owned, writable C environment table.
unsafe fn take_from(
    name: &CStr,
    mut entries: *mut *mut libc::c_char,
    pointer: *mut libc::c_char,
    remove: impl FnOnce() -> io::Result<()>,
) -> io::Result<Option<Zeroizing<Vec<u8>>>> {
    if pointer.is_null() {
        return Ok(None);
    }
    // SAFETY: getenv returns a live NUL-terminated value. No mutation intervenes.
    let bytes = unsafe { CStr::from_ptr(pointer) }.to_bytes();
    let captured = Zeroizing::new(bytes.to_vec());
    let key = name.to_bytes();
    while !entries.is_null() {
        // SAFETY: the caller supplies a live NUL-terminated environ pointer table.
        let entry = unsafe { *entries };
        if entry.is_null() {
            break;
        }
        // SAFETY: every non-null environ entry is a live NUL-terminated string.
        let bytes = unsafe { CStr::from_ptr(entry) }.to_bytes();
        if bytes.starts_with(key) && bytes.get(key.len()) == Some(&b'=') {
            let start = key.len() + 1;
            let length = bytes.len();
            for offset in start..length {
                // SAFETY: these are only the bytes before the NUL that
                // getenv/environ returned. The key and terminator stay intact.
                unsafe { entry.add(offset).write_volatile(b'0' as libc::c_char) };
            }
        }
        // SAFETY: advance within the live table, including its final null entry.
        entries = unsafe { entries.add(1) };
    }
    remove()?;
    Ok(Some(captured))
}

/// Access the live C environment only at the single-threaded startup boundary.
unsafe fn environment() -> *mut *mut libc::c_char {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: the caller excludes concurrent environment access.
        unsafe { *libc::_NSGetEnviron() }
    }
    #[cfg(target_os = "linux")]
    {
        unsafe extern "C" {
            static mut environ: *mut *mut libc::c_char;
        }
        // SAFETY: the caller excludes concurrent environment access.
        unsafe { environ }
    }
}

#[cfg(test)]
mod tests;
