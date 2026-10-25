//! Lease-owned tool inbox projection, ADR-0036 §3 and ADR-0008.
use super::{mesh_mail::payload, responses::encode_success};
use crate::{
    api::{error_code::ErrorCode, schema::*},
    app::App,
};
impl App {
    pub(super) fn native_inbox_request(
        &mut self,
        id: &str,
        target: Option<&str>,
        consume: bool,
    ) -> Option<String> {
        use crate::structured::lease::Caller;
        let result = (|| {
            let agent = match self.lease_caller() {
                Caller::Agent { agent, .. } => agent,
                Caller::Uncertain => return Err(ErrorCode::NotAuthorized),
                Caller::Unrelated => return Ok(None),
            };
            // Authority precedes resolving any caller-selected target.
            if target.is_some_and(|t| t != "self" && t != agent) {
                let own_name = self
                    .state
                    .structured_agents
                    .get(&agent)
                    .and_then(|(r, _, _)| r.name.as_deref());
                if target != own_name {
                    return Err(ErrorCode::NotAuthorized);
                }
                self.structured_target(target.unwrap_or_default())?;
            }
            let (record, _, _) = self
                .state
                .structured_agents
                .get(&agent)
                .ok_or(ErrorCode::NotAuthorized)?;
            let session = record.session_id.clone();
            let (live, expired) = crate::mesh::hello::with_store(|s| {
                s.native_inbox(
                    Some((&agent, &session)),
                    consume,
                    self.state.config.msg.inbox_retention_ms,
                    super::messages::now_ms() as i64,
                )
                .map_err(|e| e.to_string())
            })
            .map_err(|_| ErrorCode::MailStoreUnavailable)?;
            self.emit_native_expiry(expired);
            let mut messages = Vec::new();
            for envelope in live {
                let mut message = payload(&envelope)
                    .map_err(|_| ErrorCode::InvalidRequest)?
                    .message;
                message.to_pane.clone_from(&agent);
                if self.node_id.as_deref() != Some(envelope.key.origin_node.as_str()) {
                    message.from_host = crate::mesh::hello::with_store(|s| {
                        s.origin_name(&envelope.key.origin_node)
                            .map_err(|e| e.to_string())
                    })
                    .map_err(|_| ErrorCode::MailStoreUnavailable)?;
                }
                messages.push(InboxMessage {
                    correlation_id: message.correlation_id,
                    from_agent: message.from_agent,
                    from_host: message.from_host,
                    from_pane: message.from_pane,
                    from_repo: message.from_repo,
                    to_pane: message.to_pane,
                    in_reply_to: message.in_reply_to,
                    enqueued_at_ms: message.enqueued_at_ms,
                    intent: message.intent,
                    body: message.body,
                    replyable: true,
                    reply_contract: "durable_return_binding".into(),
                });
            }
            let result = if consume {
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
            };
            Ok(Some(encode_success(id.to_owned(), result)))
        })();
        match result {
            Ok(response) => response,
            Err(code) => Some(super::responses::encode_error(
                id.to_owned(),
                code.as_str(),
                "inbox access refused",
            )),
        }
    }
}

impl App {
    pub(super) fn sweep_native_inboxes(&mut self) {
        match crate::mesh::hello::with_store(|s| {
            s.native_inbox(
                None,
                false,
                self.state.config.msg.inbox_retention_ms,
                super::messages::now_ms() as i64,
            )
            .map_err(|e| e.to_string())
        }) {
            Ok((_, expired)) => self.emit_native_expiry(expired),
            Err(_) => {
                crate::logging::mesh_custody_failed("native-inbox-sweep", "mail_store_unavailable")
            }
        }
    }
    fn emit_native_expiry(&mut self, expired: Vec<crate::mesh::store::Envelope>) {
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
}
