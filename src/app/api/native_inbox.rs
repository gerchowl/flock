//! Lease-bound tool inbox projection (ADR-0036 §3 amendment).
use crate::{
    api::schema::{EventData, EventEnvelope, EventKind},
    app::App,
};
impl App {
    /// Native inbox identity is attested, never supplied by a target parameter.
    /// `None` leaves legacy pane/operator handling unchanged.
    pub(super) fn native_inbox_request(
        &mut self,
        id: &str,
        target: Option<&str>,
        consume: bool,
    ) -> Option<String> {
        use super::responses::{encode_error, encode_success};
        use crate::{
            api::{
                error_code::ErrorCode,
                schema::{InboxMessage, QueuedMessageInfo, ResponseResult},
            },
            structured::lease::Caller,
        };
        let result = (|| {
            let caller = match self.lease_caller() {
                Caller::Agent { agent, .. } => Some(agent),
                Caller::Uncertain => return Err(ErrorCode::StructuredMcpLeaseRequired),
                Caller::Unrelated => None,
            };
            let selected = match target {
                Some(target) => self.structured_send_target(target)?,
                None => None,
            };
            let Some(agent) = caller else {
                return if selected.is_some() {
                    Err(ErrorCode::InboxNotOwner)
                } else {
                    Ok(None)
                };
            };
            if target.is_some()
                && selected
                    .as_ref()
                    .is_none_or(|(selected, _)| selected != &agent)
            {
                return Err(ErrorCode::InboxNotOwner);
            }
            let session = self
                .state
                .structured_agents
                .get(&agent)
                .ok_or(ErrorCode::StructuredMcpLeaseRequired)?
                .0
                .session_id
                .clone();
            let (live, expired) = crate::mesh::hello::with_store(|s| {
                s.native_inbox_checked(
                    Some((&agent, &session)),
                    consume,
                    super::messages::now_ms() as i64,
                    |e| super::mesh_mail::payload(e).is_ok(),
                )
                .map_err(|e| e.to_string())
            })
            .map_err(|_| ErrorCode::MailStoreUnavailable)?;
            self.emit_native_inbox_expiry(expired);
            let mut messages = Vec::new();
            for e in live {
                let message = super::mesh_mail::payload(&e)
                    .map_err(|_| ErrorCode::InvalidEnvelope)?
                    .message;
                messages.push(InboxMessage {
                    correlation_id: e.correlation_id,
                    from_agent: Some(e.sender),
                    from_host: message.from_host,
                    from_pane: message.from_pane,
                    from_repo: message.from_repo,
                    to_pane: agent.clone(),
                    in_reply_to: e.in_reply_to,
                    enqueued_at_ms: message.enqueued_at_ms,
                    replyable: true,
                    reply_contract: "durable_return_binding".into(),
                    intent: message.intent,
                    body: message.body,
                });
            }
            Ok(Some(if consume {
                ResponseResult::MsgRead { messages }
            } else {
                ResponseResult::MsgList {
                    messages: messages
                        .into_iter()
                        .map(|m| QueuedMessageInfo {
                            attempts: vec![],
                            correlation_id: m.correlation_id,
                            to_pane: m.to_pane,
                            from_pane: m.from_pane,
                            in_reply_to: m.in_reply_to,
                            enqueued_at_ms: m.enqueued_at_ms,
                            delivery_attempts: 0,
                            intent: m.intent,
                            preview: m.body.chars().take(120).collect(),
                        })
                        .collect(),
                }
            }))
        })();
        match result {
            Ok(None) => None,
            Ok(Some(result)) => Some(encode_success(id.into(), result)),
            Err(code) => Some(encode_error(
                id.into(),
                code.as_str(),
                "native inbox authority refused or unavailable",
            )),
        }
    }
    fn emit_native_inbox_expiry(&mut self, expired: Vec<crate::mesh::store::Envelope>) {
        for envelope in expired {
            self.emit_event(EventEnvelope {
                event: EventKind::MessageDelivered,
                data: EventData::MessageDelivered {
                    correlation_id: envelope.correlation_id,
                    delivered: false,
                    outcome: "dropped_undeliverable".into(),
                    delivery_attempts: 0,
                    latency_ms: 0,
                },
            });
        }
    }
    /// Sweep only the independent tool-read window, never queue delivery/custody.
    pub(super) fn sweep_native_inboxes(&mut self) {
        match crate::mesh::hello::with_store(|s| {
            s.native_inbox(None, false, super::messages::now_ms() as i64)
                .map_err(|e| e.to_string())
        }) {
            Ok((_, expired)) => self.emit_native_inbox_expiry(expired),
            Err(_) => {
                crate::logging::mesh_custody_failed("native-inbox-sweep", "mail_store_unavailable")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn native_inbox_sweep_emits_each_expiry_once() {
        let (_env, _scrub) = crate::config::test_config_env_guard();
        let _store = crate::mesh::runtime_store::TestStore::new();
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        crate::mesh::hello::with_store(|s| {
            let e = crate::mesh::sign::tests::signed();
            let now = super::super::messages::now_ms() as i64;
            s.accept_local(
                &e,
                crate::mesh::store::CUSTODY_TTL_MS,
                "recipient",
                "session",
                now,
            )
            .unwrap();
            s.settle_queue_admission(&e.key, None, now).unwrap();
            s.native_inbox_delivered(&e.key, 0, now).unwrap();
            Ok(())
        })
        .unwrap();
        for _ in 0..2 {
            app.sweep_native_inboxes();
        }
        let events = app.event_hub.ring_snapshot_after(0);
        assert_eq!(events.iter().filter(|(_, e)| matches!(&e.data,
            EventData::MessageDelivered { delivered: false, outcome, .. } if outcome == "dropped_undeliverable")).count(), 1);
    }
}
