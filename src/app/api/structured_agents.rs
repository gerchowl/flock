//! Typed structured read/control routes; public launch stays gated until #1843 part 3.
use super::{
    responses::{encode_error, encode_success},
    spawn::SpawnCaller,
};
use crate::{
    api::{
        agent_state::{Lifecycle, Phase},
        error_code::ErrorCode,
        schema::{AgentStatus, AgentTarget, ResponseResult},
        structured_agent::StructuredAgentInfo,
    },
    app::App,
};
impl App {
    /// Resolve identity/name without silently choosing between runtime domains.
    pub(in crate::app) fn structured_target(
        &self,
        target: &str,
    ) -> Result<Option<(String, u64)>, ErrorCode> {
        let target = if target == "self" {
            match self.lease_caller() {
                crate::structured::lease::Caller::Agent { agent, .. } => agent,
                _ => return Ok(None),
            }
        } else {
            target.to_owned()
        };
        let found = self
            .state
            .structured_agents
            .get(&target)
            .map(|(r, _, g)| (r.agent_id.clone(), g))
            .or_else(|| {
                self.state
                    .structured_agents
                    .records()
                    .iter()
                    .find(|r| r.name.as_deref() == Some(target.as_str()))
                    .and_then(|r| {
                        self.state
                            .structured_agents
                            .get(&r.agent_id)
                            .map(|(_, _, g)| (r.agent_id.clone(), g))
                    })
            });
        if found.is_some()
            && (self.parse_pane_id(&target).is_some()
                || self.state.terminals.values().any(|t| {
                    t.id.to_string() == target
                        || t.agent_id.to_string() == target
                        || t.agent_name() == Some(target.as_str())
                        || t.effective_agent_label() == Some(target.as_str())
                }))
        {
            return Err(ErrorCode::AgentTargetAmbiguous);
        }
        Ok(found)
    }
    pub(super) fn structured_info(&self, id: &str) -> Option<StructuredAgentInfo> {
        let (record, projection, _) = self.state.structured_agents.get(id)?;
        let state = projection.update.state.clone();
        let status = match state.phase {
            Phase::Gone | Phase::Offline => AgentStatus::Offline,
            Phase::Hibernated => AgentStatus::Hibernated,
            Phase::LimitReached | Phase::Blocked | Phase::NeedsHuman => AgentStatus::Blocked,
            Phase::Working => AgentStatus::Working,
            Phase::WaitingOnEvent | Phase::Settling => AgentStatus::Idle,
            Phase::FinishedWithBackground | Phase::Finished => AgentStatus::Done,
            Phase::Unknown => AgentStatus::Unknown,
        };
        Some(StructuredAgentInfo {
            runtime: record.runtime,
            agent_id: record.agent_id.clone(),
            name: record.name.clone(),
            harness: record.harness.clone(),
            lifecycle: state.lifecycle,
            status,
            state,
            session_id: record.session_id.clone(),
            cwd: record.cwd.to_string_lossy().into_owned(),
            model: record.model.clone(),
            lease_state: self.session_leases.state(id, &record.session_id),
            view: None,
        })
    }
    pub(super) fn structured_infos(&self) -> Vec<StructuredAgentInfo> {
        self.state
            .structured_agents
            .records()
            .iter()
            .map(|r| {
                self.structured_info(&r.agent_id)
                    .expect("registry iteration retains its record")
            })
            .collect()
    }
    pub(super) fn handle_structured_stop(&mut self, id: String, target: AgentTarget) -> String {
        let (agent, generation) = match self.structured_target(&target.target) {
            Ok(Some(target)) => target,
            Err(code) => {
                return encode_error(
                    id,
                    code.as_str(),
                    "target is ambiguous across runtimes; use an agent id",
                )
            }
            Ok(None) => {
                return match self.resolve_terminal_target(&target.target) {
                    Ok(_) => encode_error(
                        id,
                        ErrorCode::RuntimeUnsupported.as_str(),
                        "agent.stop requires a registered structured agent",
                    ),
                    Err(error) => {
                        super::responses::encode_error_body(id, self.agent_target_error_body(error))
                    }
                };
            }
        };
        let allowed = match self.spawn_caller() {
            SpawnCaller::Operator { .. } => true,
            SpawnCaller::Agent { agent_id, .. }
            | SpawnCaller::Structured {
                agent_id: Some(agent_id),
                ..
            } => agent_id == agent,
            _ => false,
        };
        if !allowed {
            return encode_error(
                id,
                ErrorCode::NotAuthorized.as_str(),
                "only the operator or owning agent may stop this session",
            );
        }
        let info = self
            .structured_info(&agent)
            .expect("resolved registered record");
        if info.lifecycle == Lifecycle::Unknown
            || (!self.structured_workers.stop(&agent, generation)
                && info.lifecycle != Lifecycle::Offline)
        {
            return encode_error(
                id,
                ErrorCode::StructuredRecoveryRequired.as_str(),
                "confirm or clear uncertain ownership before retrying",
            );
        }
        // Stop is asynchronous: return the current facts, never a fabricated Offline.
        encode_success(id, ResponseResult::StructuredAgentInfo { agent: info })
    }
}
#[cfg(test)]
mod tests;
