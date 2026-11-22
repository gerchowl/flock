//! Node-signed attribution only (ADR-0036 §3/S5); transport custody remains
//! distinct from queue admission and actual reads (ADR-0026).
use super::{messages::now_ms, structured_queue::Staged};
use crate::{
    app::App,
    mesh::{delivery::Deliver, hello::with_store, store::Accepted},
};
fn terminal_refusal(reason: Option<&str>) -> String {
    use crate::{api::error_code::ErrorCode, cli::exit::Retry};
    reason
        .and_then(|reason| ErrorCode::from_code(reason.split(':').next().unwrap_or(reason)))
        .filter(|code| {
            matches!(
                code.retry(),
                Retry::Never | Retry::AfterHuman | Retry::AfterHandoff
            )
        })
        .unwrap_or(ErrorCode::MsgNotAllowed)
        .as_str()
        .to_owned()
}
impl App {
    /// Continue fresh or duplicate custody through the existing single owner.
    /// Background collection acknowledges custody; a socket waits for owner commit.
    pub(super) fn accept_structured_relay(
        &mut self,
        delivery: &Deliver,
        collected: bool,
        response_id: Option<String>,
    ) -> Result<(Accepted, bool), String> {
        let e = &delivery.envelope;
        if self.removed_agent(&e.target_agent)? {
            return Err("recipient_gone".into());
        }
        let record = &self
            .state
            .structured_agents
            .get(&e.target_agent)
            .ok_or("agent_not_found")?
            .0;
        let accepted = with_store(|s| {
            s.accept_bound_delivery(
                delivery,
                (&record.agent_id, &record.session_id),
                collected,
                now_ms() as i64,
            )
            .map_err(|e| e.to_string())
        })?;
        let state = with_store(|s| s.get(&e.key).map_err(|e| e.to_string()))?
            .ok_or("message_not_found")?
            .state;
        match state.as_str() {
            "delivered" | "read" => return Ok((accepted, true)),
            "expired" | "inbox_expired" => return Err("message_expired".into()),
            "recipient_gone" => return Err("recipient_gone".into()),
            "refused" => {
                let status = with_store(|s| {
                    s.status_key(&e.key, now_ms() as i64, None)
                        .map_err(|e| e.to_string())
                })?;
                let reason = status.and_then(|s| s.detail);
                return Err(terminal_refusal(reason.as_deref()));
            }
            _ => (),
        }
        match self.admit_queue_custody(e) {
            Ok((ticket, ack)) => {
                if let Some(id) = response_id {
                    self.structured_sends.staged = Some(Staged(id, ticket, Some(ack)));
                } else {
                    let (tx, _rx) = std::sync::mpsc::channel();
                    self.structured_sends
                        .park(Staged(String::new(), ticket, Some(ack)), tx);
                }
            }
            Err(code) => {
                let code = self.retry_queue_custody(e.clone(), code);
                if response_id.is_some() {
                    return Err(code.as_str().into());
                }
            }
        }
        Ok((accepted, true))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn legacy_terminal_refusal_reasons_never_request_a_retry() {
        use crate::api::error_code::ErrorCode;
        for reason in [None, Some("session_busy"), Some("future_refusal"), Some("")] {
            assert_eq!(super::terminal_refusal(reason), "msg_not_allowed");
        }
        for code in [
            ErrorCode::QueueItemConflict,
            ErrorCode::QueueItemTooLarge,
            ErrorCode::RecipientGone,
            ErrorCode::QueueJournalNewer,
        ] {
            assert_eq!(super::terminal_refusal(Some(code.as_str())), code.as_str());
        }
    }
}
