//! Zeroizing reservation credentials (ADR-0036 §1a).
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::io;

/// Secret for exactly one reservation generation. Never grants operator authority.
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct LeaseCredential(pub(super) zeroize::Zeroizing<String>);
impl std::fmt::Debug for LeaseCredential {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("LeaseCredential([redacted])")
    }
}
impl LeaseCredential {
    /// Generate independent OS entropy for this reservation only.
    pub(super) fn generate() -> io::Result<Self> {
        let mut bytes = zeroize::Zeroizing::new([0; 32]);
        getrandom::fill(bytes.as_mut()).map_err(io::Error::other)?;
        Ok(Self(zeroize::Zeroizing::new(
            base64::engine::general_purpose::STANDARD.encode(*bytes),
        )))
    }
    /// Consume environment bytes only into buffers that zeroize on every exit.
    pub(crate) fn from_env_bytes(bytes: Vec<u8>) -> io::Result<Self> {
        let bytes = zeroize::Zeroizing::new(bytes);
        let invalid = || io::Error::from(io::ErrorKind::InvalidInput);
        let text = std::str::from_utf8(&bytes).map_err(|_| invalid())?;
        let mut decoded = zeroize::Zeroizing::new([0; 33]);
        if base64::engine::general_purpose::STANDARD
            .decode_slice(&bytes, decoded.as_mut())
            .map_err(|_| invalid())?
            != 32
        {
            return Err(invalid());
        }
        let mut credential = zeroize::Zeroizing::new(String::with_capacity(text.len()));
        credential.push_str(text);
        Ok(Self(credential))
    }
    pub(super) fn matches(&self, other: &Self) -> bool {
        self.0.len() == other.0.len()
            && self
                .0
                .bytes()
                .zip(other.0.bytes())
                // Opaque accumulation prevents optimizer replacement with an
                // early-exit comparison. Encoded length is public wire shape.
                .fold(0, |diff, (a, b)| {
                    std::hint::black_box(diff | std::hint::black_box(a ^ b))
                })
                == std::hint::black_box(0)
    }
}
