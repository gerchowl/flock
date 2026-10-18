//! Session exclusivity and process attribution (ADR-0036 §1, §1a, §3).
//! Reserve before launch. Unconfirmed process cleanup retains the reservation;
//! an unresolved reservation never grants operator authority. UID policy is #1629.
#![deny(missing_docs)]
mod scoped;
use super::supervision::ProcessIdentity;
use crate::platform;
use parking_lot::Mutex;
use scoped::Generation;
pub(crate) use scoped::LeaseCredential;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::Arc;
use std::time::Duration;

const LOCK_LIMIT: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Root {
    Launcher(u32),
    Process(u32),
}
#[derive(Clone)]
struct Entry {
    agent: String,
    depth: u32,
    parent: Option<String>,
    process: Option<ProcessIdentity>,
    root: Root,
    generation: Generation,
}
/// Server-owned lease table shared with its runtime workers.
#[derive(Clone, Default)]
pub struct SessionLeases(Arc<Mutex<BTreeMap<String, Entry>>>, Arc<Mutex<Vec<Entry>>>);
/// Attribution from trusted process evidence, never a caller-supplied id.
#[derive(Debug, PartialEq, Eq)]
pub enum Caller {
    /// A process with no related structured launch in its ancestry.
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
    /// Scoped verification could not acquire the registry or exhausts its fact budget.
    Busy,
}
impl Caller {
    /// Refusals never acquire unrelated/operator authority, including under churn.
    pub(crate) fn refusal(&self) -> Option<crate::api::error_code::ErrorCode> {
        use crate::api::error_code::ErrorCode;
        match self {
            Self::Busy => Some(ErrorCode::LeaseBusy),
            Self::Uncertain => Some(ErrorCode::NotAuthorized),
            _ => None,
        }
    }
}
/// Exclusive session reservation. Drop releases only a never-spawned launch.
pub struct Lease {
    leases: SessionLeases,
    session: String,
    agent: String,
    spawned: bool,
    released: bool,
    cleanup_confirmed: bool,
    generation: Generation,
}
impl SessionLeases {
    // ADR-0036 §1a: acquire both views before mutation; timeout leaves the
    // reservation intact. No process creation or OS lookup runs under these locks.
    fn update<T>(&self, change: impl FnOnce(&mut BTreeMap<String, Entry>) -> T) -> io::Result<T> {
        let mut entries = self
            .0
            .try_lock_for(LOCK_LIMIT)
            .ok_or_else(|| io::Error::other("lease registry busy"))?;
        let mut published = self
            .1
            .try_lock_for(LOCK_LIMIT)
            .ok_or_else(|| io::Error::other("lease snapshot busy"))?;
        let result = change(&mut entries);
        *published = entries.values().cloned().collect();
        Ok(result)
    }
    fn snapshot(&self) -> Option<Vec<Entry>> {
        // ADR-0036 §1a: short contention waits; longer registry contention uses
        // the last published snapshot, which already contains its uncertain root.
        // Independent callers can still prove unrelated ancestry at the deadline.
        if let Some(entries) = self.0.try_lock_for(LOCK_LIMIT) {
            Some(entries.values().cloned().collect())
        } else {
            self.1
                .try_lock_for(LOCK_LIMIT)
                .map(|entries| entries.clone())
        }
    }
    /// Reserve before preparation/spawn; an existing session returns `session_busy`.
    /// Registry contention returns `agent_start_failed` without changing either view.
    /// Forks use a new session id; resume and in-place fork cannot share a lease.
    /// `lineage` supplies the parent id and parent depth; None is an operator start.
    pub(crate) fn reserve(
        &self,
        agent: &str,
        session: &str,
        lineage: Option<(&str, u32)>,
    ) -> Result<Lease, crate::api::error_code::ErrorCode> {
        let generation =
            Generation::new().map_err(|_| crate::api::error_code::ErrorCode::AgentStartFailed)?;
        self.update(|entries| {
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
                    root: Root::Launcher(std::process::id()),
                    generation: generation.clone(),
                },
            );
            Ok(())
        })
        .map_err(|_| crate::api::error_code::ErrorCode::AgentStartFailed)??;
        Ok(Lease {
            leases: self.clone(),
            session: session.into(),
            agent: agent.into(),
            spawned: false,
            released: false,
            cleanup_confirmed: false,
            generation,
        })
    }
    /// Worker-side cleanup confirmation. Never wait on this lock in the app loop.
    pub(crate) fn is_reserved(&self, session: &str) -> io::Result<bool> {
        let entries = self
            .0
            .try_lock_for(LOCK_LIMIT)
            .ok_or_else(|| io::Error::other("lease registry busy"))?;
        Ok(entries.contains_key(session))
    }

    /// Conservative live reservation counts, including pending agent-started launches.
    pub fn counts(&self, parent: &str) -> io::Result<(usize, usize)> {
        let entries = self
            .snapshot()
            .ok_or_else(|| io::Error::other("lease registry unavailable"))?;
        Ok((
            entries.iter().filter(|e| e.parent.is_some()).count(),
            entries
                .iter()
                .filter(|e| e.parent.as_deref() == Some(parent))
                .count(),
        ))
    }
    /// Whether no structured reservation exists (bounded registry wait).
    pub fn is_empty(&self) -> bool {
        self.snapshot().is_some_and(|entries| entries.is_empty())
    }
    /// Resolve a socket peer using stable kernel process facts.
    pub fn caller(&self, peer: Option<u32>) -> Caller {
        self.caller_with_pane(peer, None)
    }
    /// Preserve a proven pane boundary under ADR-0036 §1a: structured roots
    /// take precedence, but unrelated pending launches cannot erase PTY identity.
    pub(crate) fn caller_with_pane(&self, peer: Option<u32>, pane: Option<u32>) -> Caller {
        self.resolve_with_pane(peer, pane, |pid| {
            let birth = platform::process_start_time(pid)?;
            let parent = platform::process_parent_id(pid)?;
            (platform::process_start_time(pid) == Some(birth)).then_some((parent, birth))
        })
    }
    fn resolve_with_pane(
        &self,
        peer: Option<u32>,
        pane: Option<u32>,
        mut read: impl FnMut(u32) -> Option<(u32, u64)>,
    ) -> Caller {
        // ADR-0036 §1a: wait out short contention instead of changing identity.
        // On deadline, an independently proven pane still has its PTY identity.
        let entries = match self.snapshot() {
            Some(entries) => entries,
            None => {
                return if pane.is_some() {
                    Caller::Unrelated
                } else {
                    Caller::Uncertain
                }
            }
        };
        if entries.is_empty() {
            return Caller::Unrelated;
        }
        let Some(mut pid) = peer else {
            return Caller::Uncertain;
        };
        let mut seen = BTreeSet::new();
        for _ in 0..128 {
            if pid <= 1 {
                return Caller::Unrelated;
            }
            if !seen.insert(pid) {
                return Caller::Uncertain;
            }
            // ADR-0036 §1a: preparation/version probes and a spawn panic can
            // only belong to the launcher's ancestry. A proven pane is already
            // independent of that launch; an actual spawned root takes precedence.
            if entries.iter().any(|entry| {
                entry.process.is_none()
                    && match entry.root {
                        Root::Launcher(root) => pane.is_none() && root == pid,
                        Root::Process(root) => root == pid,
                    }
            }) {
                return Caller::Uncertain;
            }
            let Some((parent, birth)) = read(pid) else {
                return if pane.is_some()
                    && !entries
                        .iter()
                        .any(|entry| entry.process.is_some_and(|process| process.pid == pid))
                {
                    Caller::Unrelated
                } else {
                    Caller::Uncertain
                };
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
            if parent == 0 {
                return Caller::Unrelated;
            }
            pid = parent;
        }
        Caller::Uncertain
    }
}
impl Lease {
    /// Require this live, unspawned reservation before installing session MCP.
    pub(super) fn matches_session(&self, session: &str) -> bool {
        !self.released && !self.spawned && self.session == session
    }
    /// Verify that the reserved identity is the runtime's stream identity.
    pub(super) fn matches(&self, agent: &str, session: &str) -> bool {
        self.agent == agent && self.session == session
    }
    /// Publish launcher uncertainty with a bounded wait, before attempting spawn.
    pub(super) fn spawned(&mut self) -> io::Result<()> {
        self.leases.update(|entries| {
            let entry = entries
                .get_mut(&self.session)
                .ok_or_else(|| io::Error::other("lease missing"))?;
            entry.root = Root::Launcher(std::process::id());
            Ok::<_, io::Error>(())
        })??;
        self.spawned = true;
        Ok(())
    }
    /// Spawn only on the runtime worker, holding no registry lock across fork/exec.
    /// Launcher ancestry stays uncertain until bind, including early child exit.
    pub(super) fn spawn(
        &mut self,
        spawn: impl FnOnce() -> io::Result<std::process::Child>,
    ) -> io::Result<std::process::Child> {
        self.spawned()?;
        spawn()
    }
    /// Publish PID/birth before the runtime handle, retaining uncertainty on failure.
    pub(super) fn bind(&mut self, pid: u32) -> io::Result<()> {
        let process = ProcessIdentity::capture(pid);
        self.leases.update(|entries| {
            let entry = entries
                .get_mut(&self.session)
                .ok_or_else(|| io::Error::other("lease missing"))?;
            entry.root = Root::Process(pid);
            entry.process = process.as_ref().ok().copied();
            Ok::<_, io::Error>(())
        })??;
        process.map(|_| ())
    }
    /// Release after the runtime confirms process-tree cleanup, including startup errors.
    pub(super) fn release(&mut self) {
        self.revoke();
        self.cleanup_confirmed = true;
        if self.released {
            return;
        }
        if self
            .leases
            .update(|entries| entries.remove(&self.session))
            .is_ok()
        {
            self.released = true;
        } else {
            tracing::warn!(
                session = self.session.as_str(),
                agent = self.agent.as_str(),
                "structured lease retained: registry busy during release"
            );
        }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.revoke();
        if !self.spawned && !self.released {
            self.release();
        }
        if !self.released && !self.cleanup_confirmed {
            tracing::warn!(
                session = self.session.as_str(),
                agent = self.agent.as_str(),
                "structured lease retained: process cleanup unconfirmed"
            );
        }
    }
}
#[cfg(test)]
mod tests;

#[cfg(test)]
impl Lease {
    /// Bind the current fixture peer without spawning a harness.
    pub(crate) fn bind_test_peer(&mut self, pid: u32) {
        self.bind(pid).expect("bind owned test peer");
    }
}
