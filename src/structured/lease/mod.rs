//! Session exclusivity and process attribution (ADR-0036 §1, §1a, §3).
//! Reserve before launch. Unconfirmed process cleanup retains the reservation;
//! an unresolved reservation never grants operator authority. UID policy is #1629.
#![deny(missing_docs)]
use super::supervision::ProcessIdentity;
use crate::platform;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Entry {
    agent: String,
    depth: u32,
    parent: Option<String>,
    process: Option<ProcessIdentity>,
}
/// Server-owned lease table shared with its runtime workers.
#[derive(Clone, Default)]
pub struct SessionLeases(Arc<Mutex<BTreeMap<String, Entry>>>);
/// Attribution from trusted process evidence, never a caller-supplied id.
#[derive(Debug, PartialEq, Eq)]
pub enum Caller {
    /// An unrelated process, with no unresolved launch reservation.
    Unrelated,
    /// A leased process or descendant, including opt-in operator-mode agents.
    Agent {
        /// Fleet-global agent identity.
        agent: String,
        /// Spawn depth recorded by the admitting caller.
        depth: u32,
    },
    /// Missing/reused process identity, pending launch or unavailable registry.
    Uncertain,
}
/// Exclusive session reservation. Drop releases only a never-spawned launch.
pub struct Lease {
    leases: SessionLeases,
    session: String,
    agent: String,
    spawned: bool,
    released: bool,
}
impl SessionLeases {
    /// Reserve a session before preparation/spawn, or return `session_busy`.
    /// Forks use a new session id; resume and in-place fork cannot share a lease.
    /// `lineage` supplies the parent id and parent depth; None is an operator start.
    pub(crate) fn reserve(
        &self,
        agent: &str,
        session: &str,
        lineage: Option<(&str, u32)>,
    ) -> Result<Lease, crate::api::error_code::ErrorCode> {
        let mut entries = self
            .0
            .lock()
            .map_err(|_| crate::api::error_code::ErrorCode::AgentStartFailed)?;
        if entries.contains_key(session) {
            return Err(crate::api::error_code::ErrorCode::SessionBusy);
        }
        entries.insert(
            session.into(),
            Entry {
                agent: agent.into(),
                depth: lineage.map_or(0, |(_, depth)| depth.saturating_add(1)),
                parent: lineage.map(|(parent, _)| parent.to_owned()),
                process: None,
            },
        );
        Ok(Lease {
            leases: self.clone(),
            session: session.into(),
            agent: agent.into(),
            spawned: false,
            released: false,
        })
    }
    /// Conservative live reservation counts, including pending agent-started launches.
    pub fn counts(&self, parent: &str) -> io::Result<(usize, usize)> {
        let entries = self
            .0
            .try_lock()
            .map_err(|_| io::Error::other("lease registry unavailable"))?;
        Ok((
            entries.values().filter(|e| e.parent.is_some()).count(),
            entries
                .values()
                .filter(|e| e.parent.as_deref() == Some(parent))
                .count(),
        ))
    }
    /// Resolve a socket peer without blocking the server on a worker-held lock.
    pub fn caller(&self, peer: Option<u32>) -> Caller {
        self.resolve(peer, |pid| {
            let birth = platform::process_start_time(pid)?;
            let parent = platform::process_parent_id(pid)?;
            (platform::process_start_time(pid) == Some(birth)).then_some((parent, birth))
        })
    }
    /// Resolve with a supplied process snapshot reader. Unknown facts fail closed.
    /// The reader must verify stable birth around its parent lookup.
    fn resolve(
        &self,
        peer: Option<u32>,
        mut read: impl FnMut(u32) -> Option<(u32, u64)>,
    ) -> Caller {
        let entries: Vec<_> = match self.0.try_lock() {
            Ok(entries) => entries.values().cloned().collect(),
            Err(_) => return Caller::Uncertain,
        };
        if entries.is_empty() {
            return Caller::Unrelated;
        }
        if entries.iter().any(|entry| entry.process.is_none()) {
            return Caller::Uncertain;
        }
        let Some(mut pid) = peer else {
            return Caller::Uncertain;
        };
        let mut seen = BTreeSet::new();
        for _ in 0..128 {
            if !seen.insert(pid) {
                return Caller::Uncertain;
            }
            let Some((parent, birth)) = read(pid) else {
                return Caller::Uncertain;
            };
            for entry in &entries {
                if let Some(process) = entry.process.filter(|process| process.pid == pid) {
                    return if process.birth == birth {
                        Caller::Agent {
                            agent: entry.agent.clone(),
                            depth: entry.depth,
                        }
                    } else {
                        Caller::Uncertain
                    };
                }
            }
            if pid <= 1 || parent == 0 {
                return Caller::Unrelated;
            }
            pid = parent;
        }
        Caller::Uncertain
    }
}
impl Lease {
    /// Verify that the reserved identity is the runtime's stream identity.
    pub(super) fn matches(&self, agent: &str, session: &str) -> bool {
        self.agent == agent && self.session == session
    }
    /// Mark spawn before acquiring its birth identity; a failed lookup retains it.
    pub(super) fn spawned(&mut self) {
        self.spawned = true;
    }
    /// Bind the newly spawned process before publishing a runtime/input handle.
    pub(super) fn bind(&mut self, pid: u32) -> io::Result<()> {
        let process = ProcessIdentity::capture(pid)?;
        let mut entries = self
            .leases
            .0
            .lock()
            .map_err(|_| io::Error::other("lease registry poisoned"))?;
        let entry = entries
            .get_mut(&self.session)
            .ok_or_else(|| io::Error::other("lease missing"))?;
        entry.process = Some(process);
        Ok(())
    }
    /// Release after the runtime confirms process-tree cleanup, including startup errors.
    pub(super) fn release(&mut self) {
        if self.released {
            return;
        }
        if let Ok(mut entries) = self.leases.0.lock() {
            entries.remove(&self.session);
            self.released = true;
        }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        if !self.spawned && !self.released {
            self.release();
        }
        if !self.released {
            tracing::warn!("structured lease retained: process cleanup unconfirmed");
        }
    }
}
#[cfg(test)]
mod tests;
