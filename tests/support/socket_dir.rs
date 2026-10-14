//! Exclusive short paths shared by scripted Unix socket peers.

use std::path::{Path, PathBuf};

/// A random socket directory independent of TMPDIR and PID reuse.
///
/// Normal drop and unwinding remove the directory. Abrupt process termination
/// can leave it behind, and cleanup errors during drop are best effort.
/// Entropy, collision and filesystem errors during reservation fail immediately.
pub struct SocketDir(PathBuf);

impl SocketDir {
    /// Reserve `/tmp/<prefix>-<16 hex>` exclusively, without removing an existing
    /// path or retrying a collision. Prefixes must be 1–16 ASCII letters, digits
    /// or hyphens, preserving room for socket names under macOS SUN_LEN.
    pub fn reserve(prefix: &str) -> Self {
        assert!(
            !prefix.is_empty()
                && prefix.len() <= 16
                && prefix
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
            "socket prefix must be 1–16 ASCII letters, digits or hyphens"
        );
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).expect("socket directory entropy");
        // guardrails-ok(hermetic): Unix sockets need a short path on macOS too.
        let path = PathBuf::from(format!("/tmp/{prefix}-{:016x}", u64::from_ne_bytes(random)));
        std::fs::create_dir(&path).expect("exclusive peer directory reservation");
        Self(path)
    }

    /// The reserved directory. Keep the guard alive while its socket is needed.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for SocketDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
