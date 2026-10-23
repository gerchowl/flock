//! Atomic owner-side admission; pending reservations never expire by a clock.
use super::*;
use crate::spawn::{self, FleetConfig, SpawnCensus, SpawnVerb};

/// Authenticated caller's policy snapshot. Structured counts are added by the owner.
pub(crate) struct Admission {
    /// File-configured spawn limits.
    pub config: FleetConfig,
    /// PTY-only counts and the already-attested child depth.
    pub pty: SpawnCensus,
    /// Current owner pause state.
    pub paused: bool,
    /// The shared narrowed/fork policy gate.
    pub verb: SpawnVerb,
}
impl Workers {
    /// Count lease and pending identities once, including agent-owned launches only.
    pub(crate) fn counts(
        &self,
        leases: &SessionLeases,
        parent: &str,
    ) -> Result<(usize, usize), ErrorCode> {
        let mut identities = leases.admitted_identities().ok_or(ErrorCode::SessionBusy)?;
        for (id, parent) in &self.pending {
            identities
                .entry(id.clone())
                .or_insert_with(|| parent.clone());
        }
        Ok((
            identities.values().filter(|p| p.is_some()).count(),
            identities
                .values()
                .filter(|p| p.as_deref() == Some(parent))
                .count(),
        ))
    }
    pub(super) fn reserve_pending(
        &mut self,
        id: &str,
        parent: Option<&str>,
        leases: &SessionLeases,
        policy: Option<&Admission>,
    ) -> Result<(), ErrorCode> {
        if self.pending.contains_key(id) {
            return Err(ErrorCode::SessionBusy);
        }
        if let Some(policy) = policy {
            let parent = parent.ok_or(ErrorCode::SpawnCallerUnknown)?;
            let (live, children) = self.counts(leases, parent)?;
            let mut census = policy.pty;
            census.live_agent_started += live;
            census.parent_live_children += children;
            spawn::admit(&policy.config, policy.verb, census, policy.paused).map_err(|error| {
                ErrorCode::from_code(error.code()).unwrap_or(ErrorCode::InternalError)
            })?;
        }
        self.pending.insert(id.into(), parent.map(str::to_owned));
        Ok(())
    }
}
