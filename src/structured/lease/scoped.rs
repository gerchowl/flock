//! Per-reservation local API credentials, distinct from operator/client tokens.
use super::*;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};

/// Secret for exactly one reservation generation. Never grants operator authority.
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct LeaseCredential(zeroize::Zeroizing<String>);
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
    fn matches(&self, other: &Self) -> bool {
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

/// A revocable generation; shared snapshots observe revocation before removal.
#[derive(Clone)]
pub(super) struct Generation {
    credential: LeaseCredential,
    revoked: Arc<AtomicBool>,
}
impl Generation {
    /// Create a fresh live reservation generation.
    pub(super) fn new() -> io::Result<Self> {
        Ok(Self {
            credential: LeaseCredential::generate()?,
            revoked: Arc::new(AtomicBool::new(false)),
        })
    }
    /// Publish irrevocable revocation before any registry removal.
    pub(super) fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
    }
    fn live(&self) -> bool {
        !self.revoked.load(Ordering::Acquire)
    }
}

impl Lease {
    /// Provision only through the generated private MCP config in slice B (#1734).
    pub(crate) fn credential(&self) -> LeaseCredential {
        self.generation.credential.clone()
    }
    /// Revoke before cleanup/release; contention can never undo this transition.
    pub(in crate::structured) fn revoke(&self) {
        self.generation.revoke();
    }
}
/// Scoped admission failures requiring refusal rather than snapshot fallback.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ScopedRefusal {
    /// The registry is busy. App dispatch must never wait for it.
    RegistryContended,
    /// More than sixteen process facts would be needed to attest ancestry.
    AncestryBudget,
}

impl SessionLeases {
    /// Verify every admission from captured kernel credentials, never caller ids.
    pub(crate) fn scoped_caller(
        &self,
        credential: Option<&LeaseCredential>,
        peer: platform::peer::Credentials,
    ) -> Caller {
        self.scoped_with(credential, peer, platform::live_process_facts)
            .unwrap_or(Caller::Busy)
    }
    fn scoped_with(
        &self,
        credential: Option<&LeaseCredential>,
        peer: platform::peer::Credentials,
        mut read: impl FnMut(u32) -> Option<(u32, u64)>,
    ) -> Result<Caller, ScopedRefusal> {
        let (Some(credential), Some(mut pid), Some(peer_birth), Some(_)) =
            (credential, peer.pid, peer.birth, peer.uid)
        else {
            return Ok(Caller::Uncertain);
        };
        // Unlike ancestry's compatibility snapshot, scoped auth must never use a
        // stale view after registry contention or generation replacement.
        let entry = self
            .0
            .try_lock()
            .ok_or(ScopedRefusal::RegistryContended)?
            .values()
            .find(|e| e.generation.live() && e.generation.credential.matches(credential))
            .cloned();
        let Some(entry) = entry else {
            return Ok(Caller::Uncertain);
        };
        let Some(root) = entry.process else {
            return Ok(Caller::Uncertain);
        };
        let mut seen = BTreeSet::new();
        for _ in 0..16 {
            if pid <= 1 || !seen.insert(pid) {
                return Ok(Caller::Uncertain);
            }
            let Some((parent, birth)) = read(pid) else {
                return Ok(Caller::Uncertain);
            };
            if Some(pid) == peer.pid && birth != peer_birth {
                return Ok(Caller::Uncertain);
            }
            if pid == root.pid {
                return Ok(if birth == root.birth && entry.generation.live() {
                    Caller::Agent {
                        agent: entry.agent,
                        depth: entry.depth,
                    }
                } else {
                    Caller::Uncertain
                });
            }
            pid = parent;
        }
        Err(ScopedRefusal::AncestryBudget)
    }
    /// Hold registry contention only inside synchronous deterministic tests.
    #[cfg(test)]
    pub(crate) fn with_registry_locked<T>(&self, run: impl FnOnce() -> T) -> T {
        let _guard = self.0.lock();
        run()
    }
    #[cfg(test)]
    fn scoped_for_test(
        &self,
        credential: Option<&LeaseCredential>,
        peer: platform::peer::Credentials,
        read: impl FnMut(u32) -> Option<(u32, u64)>,
    ) -> Caller {
        self.scoped_with(credential, peer, read)
            .unwrap_or(Caller::Busy)
    }
}

#[cfg(test)]
mod tests;
