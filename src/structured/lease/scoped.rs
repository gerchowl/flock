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
    fn matches(&self, other: &Self) -> bool {
        self.0.len() == other.0.len()
            && self
                .0
                .bytes()
                .zip(other.0.bytes())
                .fold(0, |diff, (a, b)| diff | (a ^ b))
                == 0
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
    #[cfg_attr(not(test), allow(dead_code))] // Provisioning is slice B of #1734.
    pub(crate) fn credential(&self) -> LeaseCredential {
        self.generation.credential.clone()
    }
    /// Revoke before cleanup/release; contention can never undo this transition.
    pub(in crate::structured) fn revoke(&self) {
        self.generation.revoke();
    }
}
impl SessionLeases {
    /// Verify every admission from captured kernel credentials, never caller ids.
    pub(crate) fn scoped_caller(
        &self,
        credential: Option<&LeaseCredential>,
        peer: platform::peer::Credentials,
    ) -> Caller {
        self.scoped_with(credential, peer, platform::live_process_facts)
    }
    fn scoped_with(
        &self,
        credential: Option<&LeaseCredential>,
        peer: platform::peer::Credentials,
        mut read: impl FnMut(u32) -> Option<(u32, u64)>,
    ) -> Caller {
        let (Some(credential), Some(mut pid), Some(peer_birth), Some(_)) =
            (credential, peer.pid, peer.birth, peer.uid)
        else {
            return Caller::Uncertain;
        };
        // Unlike ancestry's compatibility snapshot, scoped auth must never use a
        // stale view after registry contention or generation replacement.
        let entry = self.0.try_lock_for(LOCK_LIMIT).and_then(|entries| {
            entries
                .values()
                .find(|e| e.generation.live() && e.generation.credential.matches(credential))
                .cloned()
        });
        let Some(entry) = entry else {
            return Caller::Uncertain;
        };
        let Some(root) = entry.process else {
            return Caller::Uncertain;
        };
        let mut seen = BTreeSet::new();
        for _ in 0..128 {
            if pid <= 1 || !seen.insert(pid) {
                return Caller::Uncertain;
            }
            let Some((parent, birth)) = read(pid) else {
                return Caller::Uncertain;
            };
            if Some(pid) == peer.pid && birth != peer_birth {
                return Caller::Uncertain;
            }
            if pid == root.pid {
                return if birth == root.birth && entry.generation.live() {
                    Caller::Agent {
                        agent: entry.agent,
                        depth: entry.depth,
                    }
                } else {
                    Caller::Uncertain
                };
            }
            pid = parent;
        }
        Caller::Uncertain
    }
}

#[cfg(test)]
mod tests;
