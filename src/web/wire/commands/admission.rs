//! Claim-bound lifecycle authority. Verified ingress is supplied by M2a-2.
use super::{journal, Ownership};
use crate::{api::error_code::ErrorCode, structured_wire::AgentLifecycleCommand};

/// A single dispatch capability, never a credential or management authority.
/// Only this module constructs it, after a newly committed J1 claim.
pub(crate) struct GrantAdmission {
    home: String,
    principal: String,
    issuance: String,
    ordinal: u64,
    destination: String,
    target: String,
    workspace: String,
    payload_digest: [u8; 32],
    command: AgentLifecycleCommand,
}
impl GrantAdmission {
    /// Dormant claim adapter. The caller must supply verified destination facts.
    /// Pending and replayed claims never mint a second dispatch capability.
    async fn claim(
        owner: &Ownership,
        phase: u64,
        mut claim: journal::grants::Claim,
    ) -> Result<(journal::Admission, Option<Self>), ErrorCode> {
        let command: AgentLifecycleCommand = serde_json::from_value(serde_json::json!({
            "verb": claim.command.verb, "args": claim.command.args,
        }))
        .map_err(|_| ErrorCode::InvalidParams)?;
        let target = match &command {
            AgentLifecycleCommand::Start(p) => p.workspace_id.as_deref(),
            AgentLifecycleCommand::Stop(p) => Some(p.target.as_str()),
            AgentLifecycleCommand::Restart(p) => Some(p.target.as_str()),
        };
        if target != Some(claim.target.as_str()) {
            return Err(ErrorCode::NotAuthorized);
        }
        let home = claim.home.clone();
        let principal = claim.principal.clone();
        let payload_digest = journal::digest(&claim.command)?;
        let workspace = std::sync::Arc::new(parking_lot::Mutex::new(None));
        let captured = workspace.clone();
        let start_workspace =
            matches!(command, AgentLifecycleCommand::Start(_)).then(|| claim.target.clone());
        let environment = claim.environment;
        claim.environment = Box::new(move || {
            let (clock, member) = environment();
            let member =
                member.filter(|w| start_workspace.as_ref().is_none_or(|target| target == w));
            *captured.lock() = member.clone();
            (clock, member)
        });
        let result = owner
            .submit(phase, journal::Operation::Grant(Box::new(claim)))?
            .await
            .map_err(|_| ErrorCode::SessionUnconfirmed)??;
        let authority = match &result {
            journal::Admission::Granted { consumption, state }
                if matches!(**state, journal::Admission::Claimed(_)) =>
            {
                Some(Self {
                    home,
                    principal,
                    payload_digest,
                    command,
                    issuance: consumption.issuance.clone(),
                    ordinal: consumption.ordinal,
                    destination: consumption.destination.clone(),
                    target: consumption.target.clone(),
                    workspace: workspace
                        .lock()
                        .take()
                        .ok_or(ErrorCode::SessionUnconfirmed)?,
                })
            }
            _ => None,
        };
        Ok((result, authority))
    }
    pub(crate) fn check(
        &self,
        node: Option<&str>,
        command: &AgentLifecycleCommand,
    ) -> Result<(), ErrorCode> {
        if node != Some(self.destination.as_str()) || command != &self.command {
            return Err(ErrorCode::NotAuthorized);
        }
        Ok(())
    }
    pub(crate) fn permits_resume(&self) -> bool {
        matches!(self.command, AgentLifecycleCommand::Restart(_))
    }
    pub(crate) fn workspace(&self) -> &str {
        &self.workspace
    }
    pub(crate) fn subject(&self) -> crate::structured::permissions::auth::lifecycle::RemoteSubject {
        crate::structured::permissions::auth::lifecycle::RemoteSubject {
            home: self.home.clone(),
            principal: self.principal.clone(),
        }
    }
    pub(crate) fn start(
        &self,
        node: Option<&str>,
        params: &crate::api::schema::AgentStartParams,
    ) -> Result<&str, ErrorCode> {
        if params.runtime != crate::api::schema::AgentRuntime::Structured {
            return Err(ErrorCode::NotAuthorized);
        }
        let expected = match &self.command {
            AgentLifecycleCommand::Start(p) => p.clone(),
            AgentLifecycleCommand::Restart(p) => {
                crate::structured::permissions::auth::lifecycle::restart_start(&p.target)?
            }
            _ => return Err(ErrorCode::NotAuthorized),
        };
        if node != Some(self.destination.as_str()) || params != &expected {
            return Err(ErrorCode::NotAuthorized);
        }
        Ok(&self.workspace)
    }
    pub(crate) fn stop(&self, node: Option<&str>, target: &str) -> bool {
        node == Some(self.destination.as_str())
            && self.target == target
            && matches!(&self.command, AgentLifecycleCommand::Stop(p) if p.target == target)
    }
}

#[cfg(test)]
pub(crate) mod tests;
