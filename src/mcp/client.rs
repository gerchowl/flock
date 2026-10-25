//! Startup-only transport capture for ADR-0036 §1a. No scoped-to-legacy retry.
use crate::api::client::{ApiClient, ConnectionTarget};
use base64::Engine;
use std::{ffi::OsString, io, path::PathBuf};

pub(super) const CREDENTIAL_ENV: &str = "FLOCK_MCP_LEASE_CREDENTIAL";

/// Capture mode, endpoint and credential before reading any model input.
pub(super) fn capture(structured: bool) -> io::Result<ApiClient> {
    configured(
        structured,
        std::env::var_os(CREDENTIAL_ENV),
        std::env::var_os("FLOCK_SOCKET_PATH"),
    )
}
fn configured(
    structured: bool,
    credential: Option<OsString>,
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
    let raw = zeroize::Zeroizing::new(
        credential
            .ok_or_else(refuse)?
            .into_string()
            .map_err(|_| refuse())?,
    );
    let decoded = zeroize::Zeroizing::new(
        base64::engine::general_purpose::STANDARD
            .decode(raw.as_bytes())
            .map_err(|_| refuse())?,
    );
    if decoded.len() != 32 {
        return Err(refuse());
    }
    let credential =
        serde_json::from_value(serde_json::Value::String(raw.to_string())).map_err(|_| refuse())?;
    let path = PathBuf::from(socket.ok_or_else(refuse)?);
    if !path.is_absolute() {
        return Err(refuse());
    }
    Ok(ApiClient::scoped(path, credential))
}

#[cfg(test)]
mod tests;
