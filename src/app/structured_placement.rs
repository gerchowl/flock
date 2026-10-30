//! Real native placement, independent of worker ownership (ADR-0036 §11).
use super::App;
use crate::{
    api::{error_code::ErrorCode, structured_agent::StructuredAgentView},
    structured::view::Target,
};
type Location = (usize, usize, crate::layout::PaneId);
impl App {
    fn native_location(&self, id: &str) -> Result<Option<Location>, ErrorCode> {
        let mut found = None;
        for (wi, ws) in self.state.workspaces.iter().enumerate() {
            for (ti, tab) in ws.tabs.iter().enumerate() {
                for (&pane, view) in &tab.native_views {
                    if view.target().is_none_or(|t| t.agent_id != id) {
                        continue;
                    }
                    if found.is_some()
                        || !tab.membership_valid()
                        || ws.public_pane_number(pane).is_none()
                    {
                        return Err(ErrorCode::InternalError);
                    }
                    found = Some((wi, ti, pane));
                }
            }
        }
        Ok(found)
    }
    /// Preflight the exact destination before admitting a worker or mutating layout.
    pub(in crate::app) fn native_destination(
        &self,
        id: Option<&str>,
    ) -> Result<Option<(usize, usize)>, ErrorCode> {
        if let Some(id) = id {
            if let Some((wi, ti, _)) = self.native_location(id)? {
                return Ok(Some((wi, ti)));
            }
        }
        let Some(wi) = self.state.active else {
            return Ok(None);
        };
        let ws = self
            .state
            .workspaces
            .get(wi)
            .ok_or(ErrorCode::AgentNoActiveWorkspace)?;
        let tab = ws.tabs.get(ws.active_tab).ok_or(ErrorCode::InternalError)?;
        if !tab.membership_valid() {
            return Err(ErrorCode::InternalError);
        }
        Ok(Some((wi, ws.active_tab)))
    }
    pub(in crate::app) fn place_structured_view(
        &mut self,
        target: Target,
        destination: Option<(usize, usize)>,
    ) -> Result<(), ErrorCode> {
        target
            .validate(&self.state)
            .map_err(|_| ErrorCode::AgentNotFound)?;
        if let Some((wi, ti)) = destination {
            self.state
                .attach_native_view(target, wi, ti, ratatui::layout::Direction::Horizontal)
                .map_err(|_| ErrorCode::InternalError)?;
        } else {
            let cwd = self
                .state
                .structured_agents
                .get(&target.agent_id)
                .ok_or(ErrorCode::AgentNotFound)?
                .0
                .cwd
                .clone();
            self.state
                .workspaces
                .push(crate::workspace::Workspace::native(
                    target,
                    cwd,
                    self.event_tx.clone(),
                    self.render_notify.clone(),
                    self.render_dirty.clone(),
                ));
            self.state.active = Some(self.state.workspaces.len() - 1);
        }
        self.state.mode = super::Mode::Terminal;
        Ok(())
    }
    /// Report only the placement bound to this current generation.
    pub(in crate::app) fn structured_placement(
        &self,
        target: &Target,
    ) -> Result<Option<StructuredAgentView>, ErrorCode> {
        let Some((wi, ti, pane)) = self.native_location(&target.agent_id)? else {
            return Ok(None);
        };
        let ws = &self.state.workspaces[wi];
        let tab = &ws.tabs[ti];
        if tab.native_views.get(&pane).and_then(|v| v.target()) != Some(target) {
            return Ok(None);
        }
        Ok(Some(StructuredAgentView {
            workspace_id: ws.id.clone(),
            tab_id: crate::workspace::public_tab_id_for_number(&ws.id, tab.number),
            pane_id: crate::workspace::public_pane_id_for_number(
                &ws.id,
                ws.public_pane_number(pane)
                    .ok_or(ErrorCode::InternalError)?,
            ),
        }))
    }
}
