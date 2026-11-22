//! On-disk fixture names matching the compiled CLI's registry contract.
use std::path::Path;

/// Match the compiled CLI's debug or release state namespace.
pub fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "flock-dev"
    } else {
        "flock"
    }
}

/// FNV-1a over the lossy socket spelling, without canonicalizing, as the CLI uses.
pub fn server_key(socket: &Path) -> String {
    let hash = socket
        .to_string_lossy()
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    format!("{hash:016x}")
}
