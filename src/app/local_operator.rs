//! Transport-neutral accepted-peer admission, ADR-0036 /v1/ws-over-UDS amendment.
use super::App;
use crate::{
    api::error_code::ErrorCode,
    platform::{peer::Credentials, process_exit::ExitWatch},
    structured::{identity::Principal, lease::Caller},
};
use std::sync::Arc;

/// Sealed LocalOperator origin plus a live process instance, independent of bearer scopes.
/// Admission consumers must recheck the persistent exit evidence under their fence.
#[derive(Clone)]
pub(crate) struct LocalPeer {
    peer: Credentials,
    process: Arc<ExitWatch>,
}
impl LocalPeer {
    /// Immutable watch bound to these credentials; consumers cannot replace it.
    pub(crate) fn process(&self) -> &Arc<ExitWatch> {
        &self.process
    }
    /// Accepted kernel facts, never values supplied in HTTP headers or hello frames.
    pub(crate) fn credentials(&self) -> Credentials {
        self.peer
    }
}
impl App {
    /// Validate an explicit peer without changing the current API caller or scope.
    /// UID/ancestry refusals take precedence over missing or changed process evidence.
    pub(crate) fn admit_local_peer(&mut self, peer: Credentials) -> Result<LocalPeer, ErrorCode> {
        self.admit_local_peer_using(peer, crate::platform::live_process_facts)
    }
    // Only the facts reader varies: production always uses the platform's live snapshot.
    fn admit_local_peer_using(
        &mut self,
        peer: Credentials,
        mut facts: impl FnMut(u32) -> Option<(u32, u64)>,
    ) -> Result<LocalPeer, ErrorCode> {
        if peer.uid != Some(self.runtime_config.uid_policy.operator)
            || self.runtime_config.uid_policy.classify(peer.uid) != Principal::Operator
        {
            return Err(ErrorCode::OperatorOnly);
        }
        // Arm before ancestry validation, then recheck readiness before publishing proof.
        let process = ExitWatch::arm(peer);
        let origin = self.operator_origin(
            peer.pid,
            self.lease_caller_for(peer.pid, Some(Principal::Operator)),
        );
        if origin == Some(false) {
            return Err(ErrorCode::OperatorOnly);
        }
        let process = process.map_err(|_| ErrorCode::PeerProcessChanged)?;
        if origin != Some(true)
            || !peer.pid.is_some_and(|pid| {
                crate::platform::peer::complete_ancestry(
                    pid,
                    &mut facts,
                    crate::platform::process_start_time,
                )
            })
        {
            return Err(ErrorCode::OperatorOnly);
        }
        if process
            .exited()
            .map_err(|_| ErrorCode::PeerProcessChanged)?
        {
            return Err(ErrorCode::PeerProcessChanged);
        }
        Ok(LocalPeer {
            peer,
            process: Arc::new(process),
        })
    }
    /// One shared pane/structured-agent/mesh origin rule for all local transports.
    pub(super) fn local_operator_origin(&mut self, pid: Option<u32>, caller: Caller) -> bool {
        self.operator_origin(pid, caller) == Some(true)
    }
    // Known forbidden ancestry precedes process errors; uncertain ancestry grants nothing.
    fn operator_origin(&mut self, pid: Option<u32>, caller: Caller) -> Option<bool> {
        if self.caller_pane_candidate_for(pid).is_some()
            || self
                .inbound
                .edge(pid, crate::platform::process_start_time)
                .is_some()
            || matches!(caller, Caller::Agent { .. })
        {
            return Some(false);
        }
        (pid.is_some() && caller == Caller::Unrelated).then_some(true)
    }
}
#[cfg(test)]
mod tests;
