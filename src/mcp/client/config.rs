//! Startup credential validation shared with the dedicated allocation audit.
use crate::structured::lease::LeaseCredential;
use std::{ffi::OsString, io, path::PathBuf};

/// Validated startup transport, independent of connection construction.
pub(super) enum Transport {
    /// Explicit legacy mode with no inherited session credential.
    Legacy,
    /// Absolute endpoint and a validated, zeroizing reservation credential.
    Scoped(PathBuf, LeaseCredential),
}
/// Fail closed on inconsistent mode, credentials or endpoint without echoing secrets.
pub(super) fn parse(
    structured: bool,
    credential: Option<zeroize::Zeroizing<Vec<u8>>>,
    socket: Option<OsString>,
) -> io::Result<Transport> {
    let refuse = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "structured MCP transport configuration is invalid or incomplete",
        )
    };
    if !structured {
        if credential.is_some() {
            return Err(refuse());
        }
        return Ok(Transport::Legacy);
    }
    let mut bytes = credential.ok_or_else(refuse)?;
    let credential =
        crate::structured::lease::LeaseCredential::from_env_bytes(std::mem::take(&mut *bytes))
            .map_err(|_| refuse())?;
    let path = PathBuf::from(socket.ok_or_else(refuse)?);
    if !path.is_absolute() {
        return Err(refuse());
    }
    Ok(Transport::Scoped(path, credential))
}
