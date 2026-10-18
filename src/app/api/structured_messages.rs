//! Authenticated local custody to the single queue owner (ADR-0036 §3/S5,
//! ADR-0026). Signed native recipient fields fence recovery from legacy claims.
use super::mesh_mail::{envelope, payload, Payload};
use super::responses::encode_error;
use super::structured_queue::Staged;
use crate::{
    api::{error_code::ErrorCode, schema::ResponseResult},
    app::{mailboxes::PendingMessage, App},
    mesh::{
        hello::with_store,
        key::MessageKey,
        store::{Envelope, Outcome, CUSTODY_TTL_MS},
    },
    structured::queue::{
        admission::Principal,
        model::{Delivery, Kind, Priority, Producer, State},
        Item,
    },
};
use serde_json::json;
use sha2::{Digest, Sha256};

/// Deferred response and custody settlement, consumed only after owner commit.
pub(super) struct Ack {
    message: PendingMessage,
    request: Option<MessageKey>,
    warnings: Vec<String>,
}
impl Ack {
    pub(super) fn response(&self, item_id: String) -> ResponseResult {
        ResponseResult::MsgQueued {
            item_id: Some(item_id),
            message_key: self.message.message_key.clone(),
            correlation_id: self.message.correlation_id.clone(),
            state: "queued".into(),
            warnings: self.warnings.clone(),
            to_host: None,
            path: None,
        }
    }
}
fn lifecycle(key: &MessageKey) -> String {
    let hash = Sha256::digest(format!("{}:{}", key.origin_node, key.message_id).as_bytes());
    let mut b = [0; 16];
    b.copy_from_slice(&hash[..16]);
    // RFC 9562 UUIDv8: the complete custody key supplies the custom digest.
    b[6] = (b[6] & 15) | 128;
    b[8] = (b[8] & 63) | 128;
    let s: String = b.iter().map(|v| format!("{v:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &s[..8],
        &s[8..12],
        &s[12..16],
        &s[16..20],
        &s[20..]
    )
}
fn stored<T>(
    op: impl FnOnce(&mut crate::mesh::store::Store) -> crate::mesh::store::Result<T>,
) -> Result<T, ErrorCode> {
    with_store(|s| Ok(op(s)))
        .map_err(|_| ErrorCode::MailStoreUnavailable)?
        .map_err(|error| ErrorCode::from_code(error.code()).unwrap_or(ErrorCode::InternalError))
}
impl App {
    pub(super) fn local_structured_message(
        &mut self,
        id: String,
        target: String,
        mut message: PendingMessage,
        warnings: Vec<String>,
        request: Option<MessageKey>,
    ) -> String {
        let result: Result<String, ErrorCode> = (|| {
            if !self.state.config.msg.accepts_from(None) {
                return Err(ErrorCode::MsgNotAllowed);
            }
            if self.fleet_pause.paused {
                return Err(ErrorCode::FleetPaused);
            }
            let sender = self.attested_sender_agent();
            if sender.is_none() && !self.caller_is_local_operator() {
                return Err(ErrorCode::NotAuthorized);
            }
            if sender.as_ref() == Some(&target) {
                return Err(ErrorCode::SelfMessageForbidden);
            }
            let (record, _, generation) = self
                .state
                .structured_agents
                .get(&target)
                .ok_or(ErrorCode::AgentNotFound)?;
            let session = record.session_id.clone();
            self.structured_workers
                .runtime(&target, generation)
                .ok_or(ErrorCode::QueueSourceCold)?;
            let now = super::messages::now_ms();
            let origin = self.node_id.clone().ok_or(ErrorCode::IdentityUnknown)?;
            let identity = crate::mesh::identity::NodeIdentity::existing()
                .map_err(|_| ErrorCode::IdentityUnknown)?;
            if identity.node_id() != origin {
                return Err(ErrorCode::IdentityUnknown);
            }
            message.from_agent = sender.clone();
            message.to_pane = target.clone();
            let mut signed = envelope(
                &origin,
                target.clone(),
                &Payload {
                    message,
                    peer: None,
                    host: None,
                    direct: true,
                },
            )
            .map_err(|_| ErrorCode::InternalError)?;
            signed.target_session = session.clone();
            signed.return_binding.recipient_node = origin.clone();
            if let Some(key) = &request {
                let original = stored(|s| s.collection_record(key, now as i64))?
                    .ok_or(ErrorCode::MessageNotFound)?;
                if original.remaining_ms == 0 {
                    return Err(ErrorCode::MessageExpired);
                }
                if original.envelope.return_binding.request != *key
                    || original.envelope.return_binding.recipient_node != origin
                    || !matches!(original.state.as_str(), "inbox" | "read")
                {
                    return Err(ErrorCode::ReplyUnavailable);
                }
                signed.request_key = Some(key.clone());
                signed.return_binding.collection_token =
                    original.envelope.return_binding.collection_token;
            }
            crate::mesh::sign::seal(&mut signed, &identity);
            stored(|s| s.accept_local(&signed, CUSTODY_TTL_MS, &target, &session, now as i64))?;
            let (ticket, mut ack) = self.admit_local_custody(&signed)?;
            ack.warnings = warnings;
            self.structured_sends.staged = Some(Staged(id.clone(), ticket, Some(ack)));
            Ok(String::new())
        })();
        result.unwrap_or_else(|code| {
            encode_error(id, code.as_str(), "local structured message refused")
        })
    }
    pub(super) fn verified_local_custody(
        &self,
        signed: &Envelope,
    ) -> Result<(u64, Payload), ErrorCode> {
        let origin = self.node_id.as_deref().ok_or(ErrorCode::IdentityUnknown)?;
        if signed.key.origin_node != origin
            || signed.return_binding.recipient_node != origin
            || crate::mesh::sign::verify(signed).is_err()
        {
            return Err(ErrorCode::NotAuthorized);
        }
        let (record, _, generation) = self
            .state
            .structured_agents
            .get(&signed.target_agent)
            .ok_or(ErrorCode::AgentNotFound)?;
        if record.session_id != signed.target_session {
            return Err(ErrorCode::NotAuthorized);
        }
        let data = payload(signed).map_err(|_| ErrorCode::InvalidEnvelope)?;
        if data.message.from_agent.as_deref().unwrap_or_default() != signed.sender
            || data.message.correlation_id != signed.correlation_id
        {
            return Err(ErrorCode::InvalidEnvelope);
        }
        Ok((generation, data))
    }
    fn admit_local_custody(
        &mut self,
        signed: &Envelope,
    ) -> Result<(super::structured_queue::Ticket, Ack), ErrorCode> {
        let (generation, mut data) = self.verified_local_custody(signed)?;
        self.structured_workers
            .runtime(&signed.target_agent, generation)
            .ok_or(ErrorCode::QueueSourceCold)?;
        let principal = if signed.sender.is_empty() {
            Principal::Operator
        } else {
            Principal::Origin(Producer::Agent(signed.sender.clone()))
        };
        let item = Item {
            item_id: lifecycle(&signed.key),
            producer: Producer::Human,
            kind: Kind::Message,
            state: State::Queued,
            delivery: Delivery::AtIdle,
            priority: Priority::Normal,
            body: data.message.body.clone(),
            attachments: vec![],
            metadata: [
                ("message_key".into(), json!(signed.key)),
                ("correlation_id".into(), json!(signed.correlation_id)),
                ("in_reply_to".into(), json!(signed.in_reply_to)),
                ("intent".into(), json!(data.message.intent)),
            ]
            .into(),
        };
        let ticket =
            self.structured_enqueue(signed.target_agent.clone(), generation, item, principal)?;
        data.message.message_key = Some(signed.key.clone());
        Ok((
            ticket,
            Ack {
                message: data.message,
                request: signed.request_key.clone(),
                warnings: vec![],
            },
        ))
    }
    /// Recovery waits for an owner/capacity wake, never writes a journal itself.
    /// Unknown origin is retained in custody and refused, never re-attributed.
    pub(crate) fn finish_structured_messages(&mut self, changed: bool) {
        for ack in std::mem::take(&mut self.structured_sends.acknowledged) {
            self.mailboxes.record_delivered(&ack.message);
            if ack.message.intent == crate::api::schema::MsgIntent::Blocking {
                self.mailboxes.record_blocking(
                    &super::messages::blocking_budget_key(
                        ack.message.from_agent.as_deref(),
                        ack.message.from_pane.as_deref(),
                    ),
                    ack.message.enqueued_at_ms,
                );
            }
            if self.state.config.msg.channel_push {
                if let (Some(key), Some(agent)) = (ack.request, ack.message.from_agent) {
                    let result: Result<(), ErrorCode> = (|| {
                        let original =
                            stored(|s| s.get(&key))?.ok_or(ErrorCode::MessageNotFound)?;
                        if original.envelope.target_agent != agent {
                            return Ok(());
                        }
                        let legacy = self
                            .mailboxes
                            .queued_message(ack.message.in_reply_to.as_deref().unwrap_or_default())
                            .filter(|m| m.message_key.as_ref() == Some(&key))
                            .cloned();
                        if let Some(message) = legacy {
                            self.settle_original_in(&message.to_pane, &message.correlation_id);
                        } else {
                            stored(|s| {
                                s.finish(&key, Outcome::Read, super::messages::now_ms() as i64)
                            })?;
                        }
                        Ok(())
                    })();
                    if let Err(code) = result {
                        crate::logging::mesh_custody_failed("structured-settle", code.as_str());
                    }
                }
            }
        }
        if !changed {
            return;
        }
        for signed in std::mem::take(&mut self.structured_sends.recovering) {
            match self.admit_local_custody(&signed) {
                Ok((ticket, ack)) => {
                    let (tx, _rx) = std::sync::mpsc::channel();
                    self.structured_sends
                        .park(Staged(String::new(), ticket, Some(ack)), tx);
                }
                Err(ErrorCode::SessionBusy | ErrorCode::QueueSourceCold) => {
                    self.structured_sends.recovering.push(signed)
                }
                Err(code) => {
                    crate::logging::mesh_custody_failed("structured-restore", code.as_str())
                }
            }
        }
    }
}
