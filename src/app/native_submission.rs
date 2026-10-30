//! Interactive Send shares the durable owner. No raw stdin writes or automatic retry.
use super::{api::structured_queue::Ticket, App, StructuredSendParams};
use crate::{
    api::error_code::ErrorCode,
    structured::{
        permissions::Principal,
        view::{
            contracts::{Admission, Composer},
            editor::Edit,
            Target,
        },
    },
};
use std::sync::mpsc::TryRecvError;
pub(super) struct Submission {
    target: Target,
    revision: u64,
    lifetime: u64,
    ticket: Ticket,
    outcome: Option<Result<(), ErrorCode>>,
}
impl App {
    pub(in crate::app) fn edit_native(
        &mut self,
        target: &Target,
        edit: Edit,
    ) -> Result<(), ErrorCode> {
        target
            .validate(&self.state)
            .map_err(|_| ErrorCode::AgentNotFound)?;
        let driver = self
            .structured_views
            .get(&target.agent_id)
            .filter(|d| d.target == *target)
            .ok_or(ErrorCode::AgentNotFound)?;
        if matches!(
            driver.frame()?.admission,
            Admission::Refused(ErrorCode::AgentNotFound)
        ) {
            return Err(ErrorCode::AgentNotFound);
        }
        let editor = self
            .state
            .native_editors
            .get_mut(&target.agent_id)
            .filter(|e| e.target == *target)
            .ok_or(ErrorCode::AgentNotFound)?;
        editor.edit(edit, &mut self.state.native_text_revision)?;
        self.state.native_control = None;
        driver.editor(editor.cut.clone())
    }
    pub(in crate::app) fn submit_native(
        &mut self,
        target: Target,
        draft: Composer,
    ) -> Result<(), ErrorCode> {
        target
            .validate(&self.state)
            .map_err(|_| ErrorCode::AgentNotFound)?;
        if let Some(pending) = self.native_submissions.get(&target.agent_id) {
            return Err(
                if pending.outcome == Some(Err(ErrorCode::InputWriteUnknown)) {
                    ErrorCode::InputWriteUnknown
                } else {
                    ErrorCode::SessionBusy
                },
            );
        }
        let driver = self
            .structured_views
            .get(&target.agent_id)
            .filter(|d| d.target == target)
            .ok_or(ErrorCode::AgentNotFound)?;
        if matches!(
            driver.frame()?.admission,
            Admission::Refused(ErrorCode::AgentNotFound)
        ) {
            return Err(ErrorCode::AgentNotFound);
        }
        let editor = self
            .state
            .native_editors
            .get_mut(&target.agent_id)
            .filter(|e| e.target == target)
            .ok_or(ErrorCode::AgentNotFound)?;
        if draft.text_revision != editor.cut.composer.text_revision
            || editor.cut.composer.text.trim().is_empty()
        {
            return Err(ErrorCode::InvalidParams);
        }
        let body = editor.cut.composer.text.clone();
        let revision = editor.cut.composer.text_revision;
        let lifetime = editor.lifetime;
        editor.cut.admission = Admission::Pending;
        driver.editor(editor.cut.clone())?;
        match self.structured_send(
            StructuredSendParams {
                target: target.agent_id.clone(),
                text: body,
                submit: true,
                item_id: None,
            },
            &Principal::Operator,
        ) {
            Ok(ticket) => {
                let editor = self
                    .state
                    .native_editors
                    .get_mut(&target.agent_id)
                    .ok_or(ErrorCode::AgentNotFound)?;
                editor.cut.receipt = Some(ticket.item_id.clone());
                if let Err(code) = self
                    .structured_views
                    .get(&target.agent_id)
                    .ok_or(ErrorCode::AgentNotFound)
                    .and_then(|driver| driver.editor(editor.cut.clone()))
                {
                    tracing::warn!(code = code.as_str(), "native admission view retired");
                }
                self.native_submissions.insert(
                    target.agent_id.clone(),
                    Submission {
                        target,
                        revision,
                        lifetime,
                        ticket,
                        outcome: None,
                    },
                );
                Ok(())
            }
            Err(code) => {
                let editor = self
                    .state
                    .native_editors
                    .get_mut(&target.agent_id)
                    .ok_or(ErrorCode::AgentNotFound)?;
                editor.cut.admission = Admission::Refused(code);
                if let Err(projection) = self
                    .structured_views
                    .get(&target.agent_id)
                    .ok_or(ErrorCode::AgentNotFound)
                    .and_then(|driver| driver.editor(editor.cut.clone()))
                {
                    tracing::warn!(code = projection.as_str(), "native admission view retired");
                }
                Err(code)
            }
        }
    }
    pub(super) fn poll_native_submissions(&mut self) {
        self.native_submissions.retain(|id, pending| {
            if pending.target.validate(&self.state).is_err() {
                return false;
            }
            let Some(editor) = self
                .state
                .native_editors
                .get_mut(id)
                .filter(|e| e.target == pending.target && e.lifetime == pending.lifetime)
            else {
                return false;
            };
            if let Some(outcome) = pending.outcome {
                return outcome == Err(ErrorCode::InputWriteUnknown);
            }
            let outcome = match pending.ticket.committed.try_recv() {
                Ok(r) => r.map(|_| ()),
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => Err(ErrorCode::InputWriteUnknown),
            };
            if let Err(code) = editor.settle(
                pending.revision,
                outcome,
                &mut self.state.native_text_revision,
            ) {
                editor.cut.admission = Admission::Refused(code);
            }
            if let Some(driver) = self.structured_views.get(id) {
                if let Err(code) = driver.editor(editor.cut.clone()) {
                    tracing::warn!(code = code.as_str(), "native admission view retired");
                }
            }
            tracing::info!(
                item_id = pending.ticket.item_id,
                outcome = outcome.map_or_else(|code| code.as_str(), |()| "committed"),
                "native queue admission settled"
            );
            pending.outcome = Some(outcome);
            outcome == Err(ErrorCode::InputWriteUnknown)
        });
    }
}
