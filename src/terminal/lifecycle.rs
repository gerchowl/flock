//! Pure lifecycle projection shared by API and local presentation (ADR-0028 §1).
use crate::api::schema::AgentStatus;
use crate::detect::AgentState;

pub(crate) fn detector_status(state: AgentState, seen: bool) -> AgentStatus {
    match (state, seen) {
        (AgentState::Idle, false) => AgentStatus::Done,
        (AgentState::Idle, true) => AgentStatus::Idle,
        (AgentState::Working, _) => AgentStatus::Working,
        (AgentState::Blocked, _) => AgentStatus::Blocked,
        (AgentState::Unknown, _) => AgentStatus::Unknown,
    }
}

pub(crate) fn presentation_state(status: AgentStatus) -> (AgentState, bool) {
    match status {
        AgentStatus::Blocked => (AgentState::Blocked, true),
        AgentStatus::Working => (AgentState::Working, true),
        AgentStatus::Done => (AgentState::Idle, false),
        AgentStatus::Idle | AgentStatus::Hibernated => (AgentState::Idle, true),
        AgentStatus::Unknown | AgentStatus::Offline => (AgentState::Unknown, true),
    }
}

impl super::TerminalState {
    /// Lifecycle precedes detector turn/attention, including deferred restores.
    pub(crate) fn projected_agent_status(
        &self,
        state: AgentState,
        seen: bool,
        runtime_present: bool,
    ) -> AgentStatus {
        if self.resume_failed {
            AgentStatus::Offline
        } else if self.hibernated_resume_plan.is_some() {
            AgentStatus::Hibernated
        } else if self.pending_agent_resume_plan.is_some() && !runtime_present {
            AgentStatus::Offline
        } else {
            detector_status(state, seen)
        }
    }

    /// Uses the runtime-presence fact published before input/view computation.
    /// Rendering remains pure and never reads a runtime or alters detector state.
    pub(crate) fn presentation_state(&self, seen: bool) -> (AgentState, bool) {
        let status = self.projected_agent_status(self.state, seen, self.view_runtime_present);
        if status == detector_status(self.state, seen) {
            (self.state, seen)
        } else {
            presentation_state(status)
        }
    }
}
