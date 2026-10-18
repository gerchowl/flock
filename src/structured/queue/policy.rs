//! Size policy for a single journal owner, ADR-0036 §4.
use super::{Cut, Journal};
use std::{io, num::NonZeroU64, path::Path};

/// Configured byte trigger. A file strictly larger than this value needs a rewrite.
#[derive(Debug, Clone, Copy)]
pub struct RewritePolicy {
    bytes: NonZeroU64,
}
impl RewritePolicy {
    /// Construct from the validated `[sessions] queue_rewrite_bytes` setting.
    pub fn new(bytes: NonZeroU64) -> Self {
        Self { bytes }
    }
}
impl Journal {
    pub(super) fn rewrite_required(&self) -> io::Result<bool> {
        match self.rewrite {
            Some(policy) => Ok(self.bytes()? > policy.bytes.get()),
            None => Ok(false),
        }
    }

    /// Recover with a size policy before exposing mutation authority.
    /// The worker must retain this same configured policy on every reopen.
    /// Unknown data above the threshold re-establish the typed read-only latch
    /// before tail repair. Supported crash-torn tails otherwise retain repair.
    /// Known journals wait until an explicit check or append to rewrite.
    pub fn open_with_policy(data: &Path, agent: &str, policy: RewritePolicy) -> io::Result<Self> {
        Self::open_inner(data, agent, Some(policy), true)
    }
    /// Recover an existing private journal and lease without creating either.
    /// This applies the same policy, identity checks and supported tail repair
    /// as a writer open. Cold providers must call it on their I/O worker.
    pub(crate) fn open_existing_with_policy(
        data: &Path,
        agent: &str,
        policy: RewritePolicy,
    ) -> io::Result<Self> {
        Self::open_inner(data, agent, Some(policy), false)
    }
    /// Check the named byte trigger under exclusive ownership and return the
    /// complete atomic publication cut. No successful cut is returned on refusal.
    /// Append also enforces this policy. To include any automatic replacement,
    /// publish replay from the cursor before the operation, not only its record.
    /// Large materialized snapshots can exceed the threshold themselves, so this
    /// is a rewrite trigger, not a maximum file size. No retry loop is needed.
    pub fn checkpoint_if_required(&mut self) -> io::Result<Option<Cut>> {
        self.writable()?;
        if self.rewrite_required()? {
            self.checkpoint().map(Some)
        } else {
            Ok(None)
        }
    }
}
