//! Interactive Send shares the durable owner. No raw stdin writes or automatic retry.
use super::{api::structured_queue::Ticket, App, StructuredSendParams};
use crate::{
    api::error_code::ErrorCode,
    structured::{
        permissions::Principal,
        view::{
            contracts::{Admission, Composer},
            Target,
        },
    },
};
use std::sync::mpsc::TryRecvError;
pub(super) struct Submission {
    target: Target,
    draft: Composer,
    ticket: Ticket,
    outcome: Option<Result<(), ErrorCode>>,
}
impl App {
    pub(in crate::app) fn submit_native(
        &mut self,
        target: Target,
        draft: Composer,
    ) -> Result<(), ErrorCode> {
        if draft.text.trim().is_empty() {
            return Err(ErrorCode::InvalidParams);
        }
        if let Some(pending) = self.native_submissions.get(&target.agent_id) {
            return Err(
                if pending.outcome == Some(Err(ErrorCode::InputWriteUnknown)) {
                    ErrorCode::InputWriteUnknown
                } else {
                    ErrorCode::SessionBusy
                },
            );
        }
        let frame = self
            .state
            .native_frames
            .get(&target.agent_id)
            .ok_or(ErrorCode::SessionBusy)?
            .as_ref()
            .map_err(|c| *c)?;
        if frame.target != target
            || frame.composer.text != draft.text
            || frame.composer.cursor != draft.cursor
        {
            return Err(ErrorCode::InvalidParams);
        }
        self.structured_views
            .get(&target.agent_id)
            .ok_or(ErrorCode::AgentNotFound)?
            .admission(Admission::Pending, None)?;
        let result = self.structured_send(
            StructuredSendParams {
                target: target.agent_id.clone(),
                text: draft.text.clone(),
                submit: true,
                item_id: None,
            },
            &Principal::Operator,
        );
        match result {
            Ok(ticket) => {
                self.native_submissions.insert(
                    target.agent_id.clone(),
                    Submission {
                        target,
                        draft,
                        ticket,
                        outcome: None,
                    },
                );
                Ok(())
            }
            Err(code) => {
                self.structured_views
                    .get(&target.agent_id)
                    .ok_or(ErrorCode::AgentNotFound)?
                    .admission(Admission::Refused(code), None)?;
                Err(code)
            }
        }
    }
    pub(super) fn poll_native_submissions(&mut self) {
        self.native_submissions.retain(|id, pending| {
            if pending.target.validate(&self.state).is_err() {
                return false;
            }
            let Some(driver) = self.structured_views.get(id) else {
                return false;
            };
            if let Some(outcome) = pending.outcome {
                // Keep success fenced until the cleared draft is actually published.
                // Unknown admission stays latched with its lifecycle ID; never resend.
                return outcome.is_err()
                    || driver.frame().is_ok_and(|f| {
                        f.composer.text == pending.draft.text
                            && f.composer.cursor == pending.draft.cursor
                    });
            }
            let outcome = match pending.ticket.committed.try_recv() {
                Ok(r) => r.map(|_| ()),
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => Err(ErrorCode::InputWriteUnknown),
            };
            let status = outcome.map_or_else(Admission::Refused, |()| Admission::Ready);
            let clear = outcome.is_ok().then(|| Composer {
                text: pending.draft.text.clone(),
                cursor: pending.draft.cursor,
            });
            if let Err(code) = driver.admission(status, clear) {
                tracing::warn!(code = code.as_str(), "native admission view retired");
            }
            tracing::info!(
                item_id = pending.ticket.item_id,
                outcome = outcome.map_or_else(|code| code.as_str(), |()| "committed"),
                "native queue admission settled"
            );
            pending.outcome = Some(outcome);
            outcome.is_ok() || outcome == Err(ErrorCode::InputWriteUnknown)
        });
    }
}
