//! Startup-only transport capture for ADR-0036 §1a. No scoped-to-legacy retry.
use crate::api::client::{ApiClient, ConnectionTarget};
use std::{ffi::OsString, io, path::PathBuf};

/// Capture mode, endpoint and credential before reading any model input.
pub(super) fn capture(structured: bool) -> io::Result<ApiClient> {
    capture_using(
        structured,
        || {
            // SAFETY: main -> CLI -> serve remains single-threaded, and this is
            // the first serve operation, before stdin, config or worker setup.
            unsafe { crate::platform::secret_environment::take(c"FLOCK_MCP_LEASE_CREDENTIAL") }
        },
        || std::env::var_os("FLOCK_SOCKET_PATH"),
    )
}
fn capture_using(
    structured: bool,
    take: impl FnOnce() -> io::Result<Option<zeroize::Zeroizing<Vec<u8>>>>,
    endpoint: impl FnOnce() -> Option<OsString>,
) -> io::Result<ApiClient> {
    let credential = take()?;
    configured(structured, credential, endpoint())
}

fn configured(
    structured: bool,
    credential: Option<zeroize::Zeroizing<Vec<u8>>>,
    socket: Option<OsString>,
) -> io::Result<ApiClient> {
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
        return Ok(ApiClient::for_target(ConnectionTarget::SocketPath(
            crate::api::socket_path(),
        )));
    }
    let mut bytes = credential.ok_or_else(refuse)?;
    let credential =
        crate::structured::lease::LeaseCredential::from_env_bytes(std::mem::take(&mut *bytes))
            .map_err(|_| refuse())?;
    let path = PathBuf::from(socket.ok_or_else(refuse)?);
    if !path.is_absolute() {
        return Err(refuse());
    }
    Ok(ApiClient::scoped(path, credential))
}

#[cfg(test)]
mod tests;
