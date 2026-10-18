//! Startup-only transport capture for ADR-0036 §1a. No scoped-to-legacy retry.
use crate::api::client::{ApiClient, ConnectionTarget};
use std::{ffi::OsString, io, path::PathBuf};

pub(super) const CREDENTIAL_ENV: &str = "FLOCK_MCP_LEASE_CREDENTIAL";

/// Capture mode, endpoint and credential before reading any model input.
pub(super) fn capture(structured: bool) -> io::Result<ApiClient> {
    capture_using(
        structured,
        |name| std::env::var_os(name),
        || {
            // The synchronous main -> CLI -> MCP edge reaches here before any
            // runtime, channel feed or logging worker is started. Environment
            // mutation must remain at this single-threaded startup boundary.
            std::env::remove_var(CREDENTIAL_ENV);
        },
    )
}
fn capture_using(
    structured: bool,
    read: impl Fn(&str) -> Option<OsString>,
    remove: impl FnOnce(),
) -> io::Result<ApiClient> {
    let credential = read(CREDENTIAL_ENV);
    remove();
    configured(structured, credential, read("FLOCK_SOCKET_PATH"))
}

fn configured(
    structured: bool,
    credential: Option<OsString>,
    socket: Option<OsString>,
) -> io::Result<ApiClient> {
    let credential = credential.map(|raw| zeroize::Zeroizing::new(raw.into_encoded_bytes()));
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
