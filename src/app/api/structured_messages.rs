//! Authenticated message custody to the single queue owner (ADR-0036 §3/S5,
//! ADR-0026). Verified signatures and native bindings fence recovered custody.
use super::mesh_mail::{envelope, payload, Payload};
use super::responses::{encode_error, encode_error_with_data};
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

/// Deferred response and custody settlement for the owner's admission outcome.
pub(super) struct Ack {
    key: MessageKey,
    signed: Envelope,
    recovered: bool,
    relay: bool,
    message: PendingMessage,
    request: Option<MessageKey>,
    warnings: Vec<String>,
}
impl Ack {
    pub(super) fn refusal(&self, id: String, code: ErrorCode) -> String {
        // A recovered transient outcome keeps its stable key for in-process retry.
        let code = if self.recovered && transient(code) {
            ErrorCode::InputWriteUnknown
        } else {
            code
        };
        admission_error(id, code, &self.key)
    }
    pub(super) fn response(&self, item_id: String) -> ResponseResult {
        ResponseResult::MsgQueued {
            item_id: Some(item_id),
            message_key: Some(self.key.clone()),
            correlation_id: self.message.correlation_id.clone(),
            state: if self.relay { "delivered" } else { "queued" }.into(),
            warnings: self.warnings.clone(),
            to_host: None,
            path: None,
        }
    }
}
fn transient(code: ErrorCode) -> bool {
    matches!(
        code.retry(),
        crate::cli::exit::Retry::Later
            | crate::cli::exit::Retry::Yes
            | crate::cli::exit::Retry::Maybe
            | crate::cli::exit::Retry::AfterInspecting
    )
}
fn lifecycle(key: &MessageKey) -> String {
    key.lifecycle_id()
}
fn stored<T>(
    op: impl FnOnce(&mut crate::mesh::store::Store) -> crate::mesh::store::Result<T>,
) -> Result<T, ErrorCode> {
    with_store(|s| Ok(op(s)))
        .map_err(|_| ErrorCode::MailStoreUnavailable)?
        .map_err(|error| ErrorCode::from_code(error.code()).unwrap_or(ErrorCode::InternalError))
}
/// A refusal becomes terminal before it is reported. Failed settlement is
/// uncertain instead: preserve the key so a caller can inspect both stores.
fn admission_error(id: String, mut code: ErrorCode, key: &MessageKey) -> String {
    if code != ErrorCode::InputWriteUnknown {
        match stored(|s| {
            s.settle_queue_admission(key, Some(code.as_str()), super::messages::now_ms() as i64)
        }) {
            Ok(()) | Err(ErrorCode::MessageNotFound) => (),
            Err(failure) => {
                crate::logging::mesh_custody_failed("structured-refuse", failure.as_str());
                code = ErrorCode::InputWriteUnknown;
            }
        }
    }
    encode_error_with_data(
        id,
        code.as_str(),
        if code == ErrorCode::InputWriteUnknown {
            "message admission outcome unknown; inspect custody and queue before retrying"
        } else {
            "message admission refused"
        },
        json!({"message_key":key,"item_id":lifecycle(key)}),
    )
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
                    || !matches!(original.state.as_str(), "inbox" | "read" | "delivered")
                {
                    return Err(ErrorCode::ReplyUnavailable);
                }
                signed.request_key = Some(key.clone());
                signed.return_binding.collection_token =
                    original.envelope.return_binding.collection_token;
            }
            crate::mesh::sign::seal(&mut signed, &identity);
            let admission =
                stored(|s| s.accept_local(&signed, CUSTODY_TTL_MS, &target, &session, now as i64))
                    .and_then(|_| self.admit_queue_custody(&signed));
            let (ticket, mut ack) = match admission {
                Ok(admitted) => admitted,
                Err(code) => return Ok(admission_error(id.clone(), code, &signed.key)),
            };
            ack.warnings = warnings;
            self.structured_sends.staged = Some(Staged(id.clone(), ticket, Some(ack)));
            Ok(String::new())
        })();
        result.unwrap_or_else(|code| {
            encode_error(id, code.as_str(), "local structured message refused")
        })
    }
    pub(super) fn verified_queue_custody(
        &self,
        signed: &Envelope,
    ) -> Result<(u64, Payload), ErrorCode> {
        let origin = self.node_id.as_deref().ok_or(ErrorCode::IdentityUnknown)?;
        if signed.return_binding.recipient_node != origin
            || crate::mesh::sign::verify(signed).is_err()
        {
            return Err(ErrorCode::NotAuthorized);
        }
        let (record, _, generation) = self
            .state
            .structured_agents
            .get(&signed.target_agent)
            .ok_or(ErrorCode::AgentNotFound)?;
        let remote = signed.key.origin_node != origin;
        if remote {
            if let Some(request) = &signed.request_key {
                self.verified_mesh_answer(request, signed)
                    .map_err(|_| ErrorCode::NotAuthorized)?;
            }
            let name = stored(|s| s.origin_name(&signed.key.origin_node))?;
            if !self.state.config.msg.accepts_origin(name.as_deref()) {
                return Err(ErrorCode::MsgNotAllowed);
            }
            if stored(|s| s.recipient_binding(&signed.key))?
                != Some((record.agent_id.clone(), record.session_id.clone()))
            {
                return Err(ErrorCode::RecipientGone);
            }
        } else if record.session_id != signed.target_session {
            return Err(ErrorCode::RecipientGone);
        }
        let data = payload(signed).map_err(|_| ErrorCode::InvalidEnvelope)?;
        if data.message.from_agent.as_deref().unwrap_or_default() != signed.sender
            || data.message.correlation_id != signed.correlation_id
            || data.message.in_reply_to != signed.in_reply_to
        {
            return Err(ErrorCode::InvalidEnvelope);
        }
        Ok((generation, data))
    }
    pub(super) fn admit_queue_custody(
        &mut self,
        signed: &Envelope,
    ) -> Result<(super::structured_queue::Ticket, Ack), ErrorCode> {
        let (generation, mut data) = self.verified_queue_custody(signed)?;
        // Recovery snapshots can predate an owner's terminal refusal.
        let current = stored(|s| s.get(&signed.key))?.ok_or(ErrorCode::MessageNotFound)?;
        match current.state.as_str() {
            "inbox" => (),
            "expired" | "inbox_expired" => return Err(ErrorCode::MessageExpired),
            "recipient_gone" => return Err(ErrorCode::RecipientGone),
            "refused" => return Err(ErrorCode::MsgNotAllowed),
            "delivered" | "read" => return Err(ErrorCode::MessageNotFound),
            _ => return Err(ErrorCode::MessageNotFound),
        }
        self.structured_workers
            .runtime(&signed.target_agent, generation)
            .ok_or(ErrorCode::QueueSourceCold)?;
        let remote = signed.key.origin_node != self.node_id.as_deref().unwrap_or_default();
        if remote {
            data.message.from_pane = None;
            data.message.from_host = Some(signed.key.origin_node.clone());
            data.message.to_pane = signed.target_agent.clone();
        }
        let principal = if remote {
            Principal::Origin(Producer::NodeAsserted {
                sender: signed.sender.clone(),
                origin: signed.key.origin_node.clone(),
            })
        } else if signed.sender.is_empty() {
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
                key: signed.key.clone(),
                signed: signed.clone(),
                recovered: remote,
                relay: remote,
                message: data.message,
                request: if remote {
                    None
                } else {
                    signed.request_key.clone()
                },
                warnings: vec![],
            },
        ))
    }
    /// Recovery waits for an owner/capacity wake, never writes a journal itself.
    /// Unknown origin is retained in custody and refused, never re-attributed.
    pub(crate) fn finish_structured_messages(&mut self, changed: bool) {
        for (ack, code) in std::mem::take(&mut self.structured_sends.failed) {
            match stored(|s| s.get(&ack.key)) {
                Ok(Some(record)) if ack.recovered && record.state == "inbox" => {
                    self.retry_queue_custody(record.envelope, code);
                }
                Ok(Some(record))
                    if matches!(record.state.as_str(), "refused" | "recipient_gone") =>
                {
                    self.structured_sends.retries.remove(&ack.key);
                    self.publish_queue_refusal(&record.envelope, code)
                }
                Err(_) if ack.recovered => {
                    self.retry_queue_custody(ack.signed, code);
                }
                _ => (),
            }
        }
        for ack in std::mem::take(&mut self.structured_sends.acknowledged) {
            let current = stored(|s| s.get(&ack.key));
            let settled =
                matches!(&current, Ok(Some(r)) if matches!(r.state.as_str(), "delivered" | "read"));
            let settlement = if settled {
                Ok(())
            } else {
                stored(|s| {
                    s.settle_queue_admission(&ack.key, None, super::messages::now_ms() as i64)
                })
            };
            if let Err(code) = settlement {
                self.retry_queue_custody(ack.signed, code);
                continue;
            }
            self.structured_sends.retries.remove(&ack.key);
            self.mailboxes.record_delivered(&ack.message);
            if settled {
                continue;
            }
            if ack.message.intent == crate::api::schema::MsgIntent::Blocking {
                self.mailboxes.record_blocking(
                    &if ack.relay {
                        super::messages::blocking_budget_key(None, None)
                    } else {
                        super::messages::blocking_budget_key(
                            ack.message.from_agent.as_deref(),
                            ack.message.from_pane.as_deref(),
                        )
                    },
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
        let now = std::time::Instant::now();
        for signed in std::mem::take(&mut self.structured_sends.recovering) {
            if self.fleet_pause.paused
                || (!changed
                    && self
                        .structured_sends
                        .retries
                        .get(&signed.key)
                        .is_some_and(|r| now < r.at))
            {
                self.structured_sends.recovering.push(signed);
                continue;
            }
            match self.admit_queue_custody(&signed) {
                Ok((ticket, mut ack)) => {
                    ack.recovered = true;
                    let (tx, _rx) = std::sync::mpsc::channel();
                    self.structured_sends
                        .park(Staged(String::new(), ticket, Some(ack)), tx);
                }
                Err(code) => {
                    self.retry_queue_custody(signed, code);
                }
            }
        }
    }
    /// Retry the same authenticated envelope using the existing bounded backoff.
    /// Permanent refusals use the ordinary status event, never a log-only outcome.
    pub(super) fn retry_queue_custody(&mut self, signed: Envelope, code: ErrorCode) -> ErrorCode {
        if stored(|s| s.get(&signed.key))
            .is_ok_and(|r| r.is_none_or(|r| !matches!(r.state.as_str(), "inbox")))
        {
            self.structured_sends.retries.remove(&signed.key);
            return code;
        }
        if !transient(code)
            && stored(|s| {
                s.settle_queue_admission(
                    &signed.key,
                    Some(code.as_str()),
                    super::messages::now_ms() as i64,
                )
            })
            .is_ok()
        {
            self.publish_queue_refusal(&signed, code);
            self.structured_sends.retries.remove(&signed.key);
            return code;
        }
        let retry = super::mesh_routes::Retry::failed(
            self.structured_sends.retries.get(&signed.key),
            0,
            0,
            std::time::Instant::now(),
        );
        self.structured_sends
            .retries
            .insert(signed.key.clone(), retry);
        if !self
            .structured_sends
            .recovering
            .iter()
            .any(|e| e.key == signed.key)
        {
            self.structured_sends.recovering.push(signed);
        }
        if transient(code) {
            code
        } else {
            ErrorCode::InputWriteUnknown
        }
    }
    pub(super) fn publish_queue_refusal(&mut self, signed: &Envelope, code: ErrorCode) {
        self.emit_event(crate::api::schema::EventEnvelope {
            event: crate::api::schema::EventKind::MessageDelivered,
            data: crate::api::schema::EventData::MessageDelivered {
                correlation_id: signed.correlation_id.clone(),
                delivered: false,
                outcome: format!("refused: {}", code.as_str()),
                delivery_attempts: 0,
                latency_ms: 0,
            },
        });
    }
}
