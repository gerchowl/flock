//! Boot identity for comparing persisted suspend-inclusive clock readings.
use std::io;

pub(crate) fn read() -> io::Result<String> {
    #[cfg(target_os = "linux")]
    let identity = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    #[cfg(target_os = "macos")]
    let identity = {
        let mut bytes = [0u8; 128];
        let mut length = bytes.len();
        // SAFETY: the output buffer and its size are live, with no new value supplied.
        if unsafe {
            libc::sysctlbyname(
                c"kern.bootsessionuuid".as_ptr(),
                bytes.as_mut_ptr().cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        } != 0
            || length > bytes.len()
        {
            return Err(io::Error::last_os_error());
        }
        String::from_utf8(bytes[..length].to_vec()).map_err(io::Error::other)?
    };
    let identity = identity.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    if !valid(identity) {
        return Err(io::Error::other("invalid boot identity"));
    }
    Ok(identity.to_ascii_lowercase())
}

fn valid(identity: &str) -> bool {
    identity.len() == 36
        && identity.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
        && identity.bytes().any(|b| b.is_ascii_hexdigit() && b != b'0')
}

#[cfg(test)]
mod tests {
    #[test]
    fn independent_reads_share_the_boot_identity() {
        let first = super::read().unwrap();
        assert_eq!(first, super::read().unwrap());
        assert!(super::valid(&first));
    }
}
