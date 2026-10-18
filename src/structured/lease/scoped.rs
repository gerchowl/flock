//! Per-reservation local API credentials, distinct from operator/client tokens.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// A revocable generation; shared snapshots observe revocation before removal.
#[derive(Clone)]
pub(super) struct Generation {
    credential: LeaseCredential,
    cell: Arc<GenerationCell>,
}
struct GenerationCell {
    revoked: AtomicBool,
    deadline: Option<Instant>,
}
impl Generation {
    /// Create a fresh live reservation generation.
    pub(super) fn new() -> io::Result<Self> {
        Ok(Self {
            credential: LeaseCredential::generate()?,
            cell: Arc::new(GenerationCell {
                revoked: AtomicBool::new(false),
                deadline: None,
            }),
        })
    }
    /// Publish irrevocable revocation before any registry removal.
    pub(super) fn revoke(&self) {
        self.cell.revoked.store(true, Ordering::Release);
    }
    /// Revoke the old proof before publishing a new root generation.
    pub(super) fn bump(&mut self) {
        self.revoke();
        self.cell = Arc::new(GenerationCell {
            revoked: AtomicBool::new(false),
            deadline: self.cell.deadline,
        });
    }
    fn live(&self) -> bool {
        !self.cell.revoked.load(Ordering::Acquire)
            && self
                .cell
                .deadline
                .is_none_or(|deadline| Instant::now() < deadline)
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
    /// Missing, revoked or mismatched credential evidence.
    Invalid,
    /// More than sixteen process facts would be needed to attest ancestry.
    AncestryBudget,
}

impl ScopedRefusal {
    fn caller(self) -> Caller {
        match self {
            Self::RegistryContended => Caller::Busy,
            Self::AncestryBudget => Caller::AncestryLimit,
            Self::Invalid => Caller::Uncertain,
        }
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
            .unwrap_or_else(ScopedRefusal::caller)
    }
    fn scoped_with(
        &self,
        credential: Option<&LeaseCredential>,
        peer: platform::peer::Credentials,
        read: impl FnMut(u32) -> Option<(u32, u64)>,
    ) -> Result<Caller, ScopedRefusal> {
        self.admit_with(credential, peer, read)
            .map(|(caller, _)| caller)
    }
    /// Capture the registry-owned generation for an independently verified admission.
    pub(crate) fn admit(
        &self,
        credential: Option<&LeaseCredential>,
        peer: platform::peer::Credentials,
    ) -> Result<(Caller, ContinuationProof), ScopedRefusal> {
        self.admit_with(credential, peer, platform::live_process_facts)
    }
    fn admit_with(
        &self,
        credential: Option<&LeaseCredential>,
        peer: platform::peer::Credentials,
        mut read: impl FnMut(u32) -> Option<(u32, u64)>,
    ) -> Result<(Caller, ContinuationProof), ScopedRefusal> {
        let (Some(credential), Some(_), Some(_), Some(_)) =
            (credential, peer.pid, peer.birth, peer.uid)
        else {
            return Err(ScopedRefusal::Invalid);
        };
        // Unlike ancestry's compatibility snapshot, scoped auth must never use a
        // stale view after registry contention or generation replacement.
        let proof = self
            .0
            .try_lock()
            .ok_or(ScopedRefusal::RegistryContended)?
            .values()
            .find(|e| e.generation.live() && e.generation.credential.matches(credential))
            .map(|entry| ContinuationProof {
                entry: entry.clone(),
                peer,
            });
        let Some(proof) = proof else {
            return Err(ScopedRefusal::Invalid);
        };
        let caller = proof.verify_with(&mut read)?;
        Ok((caller, proof))
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
            .unwrap_or_else(ScopedRefusal::caller)
    }
}

/// Admission-bound identity plus live revocation state, never a cached authorization.
#[derive(Clone)]
pub(crate) struct ContinuationProof {
    entry: Entry,
    peer: platform::peer::Credentials,
}
impl ContinuationProof {
    fn live(&self) -> bool {
        self.entry.generation.live()
    }
    /// Re-read kernel ancestry and the cell at each effect boundary without table locks.
    pub(crate) fn caller(&self) -> Caller {
        self.verify_with(platform::live_process_facts)
            .unwrap_or(Caller::Uncertain)
    }
    pub(crate) fn verify_with(
        &self,
        mut read: impl FnMut(u32) -> Option<(u32, u64)>,
    ) -> Result<Caller, ScopedRefusal> {
        if !self.live() {
            return Ok(Caller::Uncertain);
        }
        let (Some(mut pid), Some(peer_birth), Some(root)) =
            (self.peer.pid, self.peer.birth, self.entry.process)
        else {
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
            if Some(pid) == self.peer.pid && birth != peer_birth {
                return Ok(Caller::Uncertain);
            }
            if pid == root.pid {
                return Ok(if birth == root.birth && self.live() {
                    Caller::Agent {
                        agent: self.entry.agent.clone(),
                        depth: self.entry.depth,
                    }
                } else {
                    Caller::Uncertain
                });
            }
            pid = parent;
        }
        Err(ScopedRefusal::AncestryBudget)
    }
}

#[cfg(test)]
mod tests;
