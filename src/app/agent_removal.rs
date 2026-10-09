//! Only the owning lifecycle can declare an agent permanently gone.
use super::App;
use crate::{
    api::schema::{EventData, EventEnvelope, EventKind},
    terminal::TerminalState,
};

// Non-removing events are part of the decision table, not lifecycle write sites.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemovalEvent {
    Kill,
    Close,
    Exit,
    Hibernate,
    Restart,
    Resume,
    Disconnect,
    ResumeFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reason {
    Killed,
    Closed,
    Exited,
}
impl Reason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Killed => "killed",
            Self::Closed => "closed",
            Self::Exited => "exited",
        }
    }
}

pub(crate) fn removal_decision(event: RemovalEvent, has_resumable_session: bool) -> Option<Reason> {
    match event {
        RemovalEvent::Kill => Some(Reason::Killed),
        RemovalEvent::Close if !has_resumable_session => Some(Reason::Closed),
        RemovalEvent::Exit if !has_resumable_session => Some(Reason::Exited),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Removal {
    pub(crate) agent: String,
    pub(crate) session: String,
    reason: Reason,
}

pub(crate) fn identity(terminal: &TerminalState) -> (String, String) {
    (
        terminal.agent_id.to_string(),
        terminal.submission_session_id().unwrap_or_default(),
    )
}

pub(crate) fn capture(terminal: &TerminalState, event: RemovalEvent) -> Option<Removal> {
    if !terminal.is_agent_terminal() {
        return None;
    }
    let resumable = terminal.pending_agent_resume_plan.is_some()
        || terminal.hibernated_resume_plan.is_some()
        || terminal.restart_in_progress
        || terminal.persisted_agent_session.as_ref().is_some_and(|s| {
            crate::agent_resume::plan(&s.source, &s.agent, &s.session_ref).is_some()
        });
    let reason = removal_decision(event, resumable)?;
    let (agent, session) = identity(terminal);
    Some(Removal {
        agent,
        session,
        reason,
    })
}

impl App {
    pub(crate) fn retain_exited_session(&mut self, pane: crate::layout::PaneId) {
        let id = self
            .find_pane(pane)
            .map(|(_, p)| p.attached_terminal_id.clone());
        let Some(terminal) = id.and_then(|id| self.state.terminals.get_mut(&id)) else {
            return;
        };
        if terminal.hibernated_resume_plan.is_none()
            && terminal.pending_agent_resume_plan.is_none()
            && !terminal.restart_in_progress
        {
            if let Some(plan) = terminal
                .persisted_agent_session
                .as_ref()
                .and_then(|s| crate::agent_resume::plan(&s.source, &s.agent, &s.session_ref))
            {
                terminal.set_hibernated_resume_plan(Some(plan));
                terminal.respawn_shell_on_exit = false;
                self.state.mark_session_dirty();
            }
        }
    }

    pub(crate) fn remove_agent_for_pane(
        &mut self,
        ws: usize,
        pane: crate::layout::PaneId,
        event: RemovalEvent,
    ) {
        let removal = self
            .state
            .terminal_id_for_pane(ws, pane)
            .and_then(|id| self.state.terminals.get(&id))
            .and_then(|terminal| capture(terminal, event));
        if let Some(removal) = removal {
            self.record_agent_removal(removal);
        }
    }

    pub(crate) fn remove_workspace_agents(&mut self, ws: usize) {
        let removals: Vec<_> = self
            .state
            .terminal_ids_for_workspace(ws)
            .into_iter()
            .filter_map(|id| self.state.terminals.get(&id))
            .filter_map(|t| capture(t, RemovalEvent::Kill))
            .collect();
        for removal in removals {
            self.record_agent_removal(removal);
        }
    }

    fn write_agent_removal(&mut self, removal: &Removal) -> Result<(), String> {
        if self.node_id.is_none() {
            return Ok(());
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let changed = crate::mesh::hello::with_store(|store| {
            store.set_local_node(self.node_id.as_deref().unwrap_or_default());
            if store
                .is_tombstoned(&removal.agent, &removal.session, now)
                .map_err(|e| e.to_string())?
            {
                return Ok(false);
            }
            store
                .tombstone(
                    &removal.agent,
                    &removal.session,
                    removal.reason.as_str(),
                    now,
                )
                .map_err(|e| e.to_string())?;
            Ok(true)
        })?;
        if changed {
            self.emit_event(EventEnvelope {
                event: EventKind::AgentRemoved,
                data: EventData::AgentRemoved {
                    agent_id: removal.agent.clone(),
                    session: removal.session.clone(),
                    reason: removal.reason.as_str().into(),
                },
            });
        }
        Ok(())
    }

    pub(crate) fn record_agent_removal(&mut self, removal: Removal) {
        if self
            .pending_agent_removals
            .iter()
            .any(|r| r.agent == removal.agent && r.session == removal.session)
        {
            return;
        }
        if let Err(reason) = self.write_agent_removal(&removal) {
            if reason != "fleet_paused" {
                crate::logging::mesh_custody_failed("tombstone", "mail_store_unavailable");
            }
            if self.pending_agent_removals.len() == 128 {
                self.pending_agent_removals.pop_front();
                tracing::warn!(
                    "agent removal retry capacity reached; dropped mail will expire normally"
                );
            }
            self.pending_agent_removals.push_back(removal);
        }
    }

    pub(crate) fn retry_agent_removals(&mut self) {
        for _ in 0..self.pending_agent_removals.len().min(8) {
            if let Some(removal) = self.pending_agent_removals.pop_front() {
                if self.write_agent_removal(&removal).is_err() {
                    self.pending_agent_removals.push_back(removal);
                }
            }
        }
    }

    pub(crate) fn local_recipient_identity(
        &self,
        ws: usize,
        pane: crate::layout::PaneId,
    ) -> Option<(String, String)> {
        let id = self.state.terminal_id_for_pane(ws, pane)?;
        self.state.terminals.get(&id).map(identity)
    }

    pub(crate) fn removed_agent(&self, agent: &str) -> Result<bool, String> {
        if self.node_id.is_none() {
            return Ok(false);
        }
        if self.pending_agent_removals.iter().any(|r| r.agent == agent) {
            return Ok(true);
        }
        crate::mesh::hello::with_store(|store| {
            store
                .agent_removed(
                    agent,
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as i64,
                )
                .map_err(|e| e.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn removal_decision_table() {
        for retained in [false, true] {
            for event in [
                RemovalEvent::Kill,
                RemovalEvent::Close,
                RemovalEvent::Exit,
                RemovalEvent::Hibernate,
                RemovalEvent::Restart,
                RemovalEvent::Resume,
                RemovalEvent::Disconnect,
                RemovalEvent::ResumeFailed,
            ] {
                let expected = match event {
                    RemovalEvent::Kill => Some(Reason::Killed),
                    RemovalEvent::Close if !retained => Some(Reason::Closed),
                    RemovalEvent::Exit if !retained => Some(Reason::Exited),
                    _ => None,
                };
                assert_eq!(
                    removal_decision(event, retained),
                    expected,
                    "{event:?} retained={retained}"
                );
            }
        }
    }
}
