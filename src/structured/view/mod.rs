//! Native placement only: ADR-0036 §11, S10a1. Closing never stops an agent.
pub(crate) mod contracts;
pub(crate) mod placement;
use crate::{app::AppState, layout::PaneId};
use placement::View;
use ratatui::layout::Direction;

/// A UI target is valid only in its registry generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub agent_id: String,
    pub generation: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlacementError {
    StaleGeneration,
    Membership,
    RelocationUnsupported,
}
impl Target {
    pub fn validate(&self, state: &AppState) -> Result<(), PlacementError> {
        match state.structured_agents.get(&self.agent_id) {
            Some((_, _, generation)) if generation == self.generation => Ok(()),
            _ => Err(PlacementError::StaleGeneration),
        }
    }
}
impl AppState {
    /// Reuse the agent's sole placement, or split without allocating a PTY.
    /// A resumed generation may rebind placement only after registry validation.
    pub(crate) fn attach_native_view(
        &mut self,
        target: Target,
        workspace: usize,
        tab: usize,
        direction: Direction,
    ) -> Result<PaneId, PlacementError> {
        target.validate(self)?;
        for (wi, ws) in self.workspaces.iter_mut().enumerate() {
            for (ti, t) in ws.tabs.iter_mut().enumerate() {
                if let Some((&id, view)) = t
                    .native_views
                    .iter_mut()
                    .find(|(_, view)| view.target().is_some_and(|t| t.agent_id == target.agent_id))
                {
                    *view = View::Ready(target);
                    t.layout.focus_pane(id);
                    ws.active_tab = ti;
                    self.active = Some(wi);
                    return Ok(id);
                }
            }
        }
        let ws = self
            .workspaces
            .get_mut(workspace)
            .ok_or(PlacementError::Membership)?;
        let t = ws.tabs.get_mut(tab).ok_or(PlacementError::Membership)?;
        if !t.membership_valid() {
            return Err(PlacementError::Membership);
        }
        let id = t.layout.split_focused(direction);
        t.native_views.insert(id, View::Ready(target));
        ws.register_new_pane(id);
        ws.active_tab = tab;
        self.active = Some(workspace);
        Ok(id)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        structured::registry::{Record, StructuredRuntime},
        workspace::Workspace,
    };

    pub(crate) fn fixture() -> (AppState, Target) {
        let mut state = AppState::test_new();
        state
            .workspaces
            .push(Workspace::test_new("native-view-fixture"));
        let generation = state
            .structured_agents
            .register(Record {
                runtime: StructuredRuntime::Structured,
                agent_id: "fixture-agent".into(),
                name: None,
                harness: "claude".into(),
                session_id: "fixture-session".into(),
                cwd: std::env::temp_dir(),
                model: None,
            })
            .unwrap();
        (
            state,
            Target {
                agent_id: "fixture-agent".into(),
                generation,
            },
        )
    }
    #[test]
    fn placement_reuses_without_runtime_and_close_keeps_agent() {
        let (mut state, target) = fixture();
        let terminals = state.terminals.len();
        let root = state.workspaces[0].tabs[0].root_pane;
        let id = state
            .attach_native_view(target.clone(), 0, 0, Direction::Horizontal)
            .unwrap();
        assert_eq!(
            state.attach_native_view(target.clone(), 0, 0, Direction::Vertical),
            Ok(id)
        );
        assert_eq!(state.terminals.len(), terminals);
        let ws = &mut state.workspaces[0];
        assert!(ws.tabs[0].runtimes.is_empty());
        assert_eq!(ws.tabs[0].panes.len(), 1);
        assert!(ws.tabs[0].membership_valid());
        assert!(!ws.close_pane(root));
        assert_eq!(ws.tabs[0].root_pane, id);
        assert!(ws.tabs[0].membership_valid());
        ws.test_split(Direction::Horizontal);
        assert!(!ws.close_pane(id));
        assert!(ws.tabs[0].native_views.is_empty());
        assert!(ws.tabs[0].membership_valid());
        assert!(state.structured_agents.get(&target.agent_id).is_some());
    }
    #[test]
    fn native_move_refuses_before_removing_last_pane_tab() {
        let (mut state, target) = fixture();
        let root = state.workspaces[0].tabs[0].root_pane;
        let id = state
            .attach_native_view(target, 0, 0, Direction::Horizontal)
            .unwrap();
        let ws = &mut state.workspaces[0];
        ws.close_pane(root);
        assert!(matches!(
            ws.take_pane_for_move(id),
            Err(PlacementError::RelocationUnsupported)
        ));
        assert_eq!(ws.tabs.len(), 1);
        assert_eq!(ws.tabs[0].layout.pane_ids(), vec![id]);
        assert!(ws.tabs[0].membership_valid());
    }
    #[test]
    fn stale_generation_cannot_rebind_resumed_placement() {
        let (mut state, old) = fixture();
        let id = state
            .attach_native_view(old.clone(), 0, 0, Direction::Horizontal)
            .unwrap();
        let generation = state
            .structured_agents
            .begin_launch(&old.agent_id, old.generation)
            .unwrap();
        assert_eq!(
            state.attach_native_view(old.clone(), 0, 0, Direction::Vertical),
            Err(PlacementError::StaleGeneration)
        );
        let current = Target { generation, ..old };
        assert_eq!(
            state.attach_native_view(current, 0, 0, Direction::Vertical),
            Ok(id)
        );
    }
    #[test]
    fn membership_rejects_missing_duplicate_and_orphan_content() {
        let (mut state, target) = fixture();
        let tab = &mut state.workspaces[0].tabs[0];
        let root = tab.root_pane;
        tab.native_views.insert(root, View::Ready(target.clone()));
        assert!(tab.content(root).is_err());
        assert!(!tab.membership_valid());
        tab.native_views.remove(&root);
        let pane = tab.panes.remove(&root).unwrap();
        assert!(tab.content(root).is_err());
        tab.panes.insert(root, pane);
        tab.native_views
            .insert(PaneId::alloc(), View::Ready(target));
        assert!(!tab.membership_valid());
    }
    #[test]
    fn terminal_move_and_invalid_close_preserve_membership() {
        let (mut state, _) = fixture();
        let ws = &mut state.workspaces[0];
        let root = ws.tabs[0].root_pane;
        let missing = PaneId::alloc();
        let before = ws.tabs[0].layout.pane_ids();
        ws.test_split(Direction::Horizontal);
        assert!(ws.tabs[0].close_pane(missing).is_none());
        assert_eq!(ws.tabs[0].layout.pane_count(), 2);
        assert!(ws.take_pane_for_move(root).is_ok());
        assert!(ws.tabs[0].membership_valid());
        assert_eq!(before, vec![root]);
    }
}
