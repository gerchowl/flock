//! Generation-fenced interactive dispatch. A view or intent grants no tool authority.
use super::App;
use crate::{
    api::error_code::ErrorCode,
    layout::PaneId,
    structured::{
        permissions::{AgentDecide, Principal},
        view::contracts::{Action, Intent},
    },
};
impl App {
    pub(in crate::app) fn dispatch_native_intent(
        &mut self,
        pane: PaneId,
        intent: Intent,
    ) -> Result<(), ErrorCode> {
        crate::ui::structured::validate(&self.state, pane, &intent.target)?;
        let target = intent.target;
        match intent.action {
            Action::Send(draft) => self.submit_native(target, draft),
            Action::EditComposer(edit) => self.edit_native(&target, edit),
            Action::Close => {
                let workspace = self.state.active.ok_or(ErrorCode::AgentNotFound)?;
                self.state.focus_pane_in_workspace(workspace, pane);
                self.state.close_pane();
                Ok(())
            }
            Action::Decide {
                handle,
                decision,
                message,
            } => {
                // S10e supplies the operator capability and Always confirmation.
                // The current interactive UID principal deliberately cannot authorize.
                self.structured_workers
                    .runtime(&target.agent_id, target.generation)
                    .ok_or(ErrorCode::AgentNotFound)?
                    .agent_decide(
                        &Principal::Operator,
                        &AgentDecide {
                            agent: target.agent_id,
                            request_id: handle.request_id,
                            request_seq: handle.request_seq,
                            decision,
                            message,
                        },
                    )
            }
            action => self
                .structured_views
                .get(&target.agent_id)
                .filter(|d| d.target == target)
                .ok_or(ErrorCode::AgentNotFound)?
                .control(action),
        }
    }
}
