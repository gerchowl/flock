//! Custody is committed before projecting mail or acknowledging a held edge.
use super::messages::{now_ms, ResolvedTarget};
use super::responses::{encode_error, encode_success};
use crate::api::schema::{EventData, EventEnvelope, EventKind, MessageTarget, ResponseResult};
use crate::app::{mailboxes::PendingMessage, message_relay::RelaySend, App};
use crate::mesh::{
    delivery::Deliver,
    hello::with_store,
    key::MessageKey,
    store::{Accepted, Admission, Envelope, Outcome, ReturnBinding, CUSTODY_TTL_MS},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Payload {
    pub(super) message: PendingMessage,
    pub(super) peer: Option<String>,
    pub(super) host: Option<String>,
    pub(super) direct: bool,
}

pub(super) fn envelope(
    origin: &str,
    target: String,
    payload: &Payload,
) -> Result<Envelope, String> {
    let key = MessageKey::mint(origin.into(), now_ms()).map_err(|e| e.to_string())?;
    let return_binding =
        ReturnBinding::mint(key.clone(), String::new(), Vec::new()).map_err(|e| e.to_string())?;
    Ok(Envelope {
        kind: Default::default(),
        origin_key: Vec::new(),
        signature: Vec::new(),
        key,
        sender: payload.message.from_agent.clone().unwrap_or_default(),
        target_agent: target,
        target_session: payload.message.to_pane.clone(),
        correlation_id: payload.message.correlation_id.clone(),
        in_reply_to: payload.message.in_reply_to.clone(),
        request_key: None,
        return_binding,
        intent: serde_json::to_string(&payload.message.intent).map_err(|e| e.to_string())?,
        body: serde_json::to_vec(payload).map_err(|e| e.to_string())?,
    })
}

pub(super) fn payload(envelope: &Envelope) -> Result<Payload, String> {
    serde_json::from_slice(&envelope.body).map_err(|e| e.to_string())
}

impl App {
    pub(super) fn emit_mesh_wake(&mut self, next_hop: &str) {
        if self.outbound_reply_peer(next_hop).is_some()
            || !self
                .inbound
                .live(crate::platform::process_start_time)
                .any(|edge| edge.enrolled() && edge.enrollment.node_id.as_deref() == Some(next_hop))
        {
            return;
        }
        self.emit_event(EventEnvelope {
            event: EventKind::MeshOutboundPending,
            data: EventData::MeshOutboundPending {},
        });
    }

    pub(super) fn import_mesh_receipt(
        &mut self,
        mail: &Envelope,
    ) -> Result<(Accepted, bool), String> {
        let receipt: crate::mesh::collect::Receipt =
            serde_json::from_slice(&mail.body).map_err(|_| "invalid_envelope".to_string())?;
        with_store(|store| {
            if self.node_id.as_deref() != Some(receipt.key.origin_node.as_str()) {
                return Err("invalid reply binding".into());
            }
            let original = store
                .get(&receipt.key)
                .map_err(|e| e.to_string())?
                .ok_or("receipt_original_not_ready")?;
            if original.envelope.return_binding.recipient_node != mail.key.origin_node
                || original.envelope.return_binding.collection_token != receipt.token
            {
                return Err("invalid reply binding".into());
            }
            match store
                .import_receipt(&receipt.key, &receipt.state)
                .map_err(|e| e.to_string())?
            {
                crate::mesh::store::ReceiptImport::Applied => Ok((Accepted::New, true)),
                crate::mesh::store::ReceiptImport::Duplicate => Ok((Accepted::Duplicate, true)),
                crate::mesh::store::ReceiptImport::OriginalNotReady => {
                    Err("receipt_original_not_ready".into())
                }
            }
        })
    }

    pub(super) fn persist_mesh_send(
        &mut self,
        peer: &crate::config::PeerConfig,
        to_agent: &str,
        data: &Payload,
    ) -> Result<Deliver, String> {
        // Pure App fixtures have no server identity or filesystem store.
        #[cfg(test)]
        if self.node_id.is_none() {
            return Ok(Deliver {
                envelope: envelope("nodea", to_agent.to_string(), data)?,
                remaining_ms: CUSTODY_TTL_MS,
            });
        }
        if self.fleet_pause.paused {
            return Err("fleet_paused".into());
        }
        let origin = self
            .node_id
            .as_deref()
            .ok_or("mesh node identity unavailable")?;
        let mut envelope = envelope(origin, to_agent.to_string(), data)?;
        with_store(|store| {
            if let Some(pin) = store.get_pin(&peer.name).map_err(|e| e.to_string())? {
                envelope
                    .return_binding
                    .collection_peers
                    .push(pin.node_id.clone());
                envelope.return_binding.recipient_node = pin.node_id;
            }
            Ok(())
        })?;
        with_store(|store| {
            store
                .accept(
                    &envelope,
                    CUSTODY_TTL_MS,
                    Admission::Custody,
                    now_ms() as i64,
                )
                .map_err(|e| e.to_string())?;
            store
                .schedule_retry(&envelope.key, 60_000, now_ms() as i64)
                .map_err(|e| e.to_string())
        })?;
        self.mesh_retry_at = None;
        Ok(Deliver {
            envelope,
            remaining_ms: CUSTODY_TTL_MS,
        })
    }

    pub(super) fn persist_local_mail(
        &mut self,
        message: &mut PendingMessage,
    ) -> Result<(), String> {
        if self.mailboxes.queued_len(&message.to_pane) >= crate::app::mailboxes::MAX_QUEUED_PER_PANE
        {
            return Err("mailbox_full".into());
        }
        if self.fleet_pause.paused {
            return Err("fleet_paused".into());
        }
        let origin = self
            .node_id
            .as_deref()
            .ok_or("mesh node identity unavailable")?;
        let data = Payload {
            message: message.clone(),
            peer: None,
            host: None,
            direct: true,
        };
        let mut envelope = envelope(origin, String::new(), &data)?;
        envelope.return_binding.recipient_node = origin.into();
        with_store(|store| {
            store
                .accept(&envelope, CUSTODY_TTL_MS, Admission::Inbox, now_ms() as i64)
                .map(|_| ())
                .map_err(|e| e.to_string())
        })?;
        message.message_key = Some(envelope.key);
        Ok(())
    }

    pub(super) fn handle_mesh_deliver(&mut self, id: String, delivery: Deliver) -> String {
        match self.import_mesh_mail(&delivery) {
            Ok((accepted, delivered)) => encode_success(
                id,
                ResponseResult::MsgQueued {
                    message_key: Some(delivery.envelope.key),
                    correlation_id: delivery.envelope.correlation_id,
                    state: if !delivered {
                        "custody"
                    } else if accepted == Accepted::New {
                        "delivered"
                    } else {
                        "duplicate"
                    }
                    .into(),
                    warnings: Vec::new(),
                    to_host: None,
                    path: None,
                },
            ),
            Err(reason) => encode_error(
                id,
                if reason == "origin_mismatch" {
                    "origin_mismatch"
                } else if reason == super::mesh_replies::UNAVAILABLE {
                    "reply_unavailable"
                } else {
                    "mesh_delivery_refused"
                },
                reason,
            ),
        }
    }

    fn import_mesh_mail(&mut self, delivery: &Deliver) -> Result<(Accepted, bool), String> {
        if self.fleet_pause.paused {
            return Err("fleet_paused".into());
        }
        let edge = self
            .inbound
            .edge(
                self.current_api_peer_pid,
                crate::platform::process_start_time,
            )
            .map(|edge| &edge.enrollment)
            .filter(|edge| edge.state == "pinned")
            .ok_or("mesh edge is not enrolled")?;
        let envelope = &delivery.envelope;
        if edge.node_id.as_deref() != Some(envelope.key.origin_node.as_str()) {
            crate::logging::mesh_custody_failed("import", "origin_mismatch");
            return Err("origin_mismatch".into());
        }
        self.import_attested_mesh_mail(delivery)
    }

    pub(super) fn import_attested_mesh_mail(
        &mut self,
        delivery: &Deliver,
    ) -> Result<(Accepted, bool), String> {
        if self.fleet_pause.paused {
            return Err("fleet_paused".into());
        }
        let envelope = &delivery.envelope;
        let sender_host = with_store(|store| {
            store
                .origin_name(&envelope.key.origin_node)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "origin_mismatch".into())
        })?;
        if !self.state.config.msg.accepts_from(Some(&sender_host)) {
            return Err("msg_not_allowed".into());
        }
        if let Some(request) = &envelope.request_key {
            self.import_mesh_answer(request, delivery, None)?;
            return Ok((Accepted::New, true));
        }
        let mut data = payload(envelope)?;
        if data.message.correlation_id != envelope.correlation_id
            || data.message.from_agent.as_deref().unwrap_or_default() != envelope.sender
            || envelope.return_binding.request != envelope.key
        {
            return Err("inconsistent mesh envelope".into());
        }
        let duplicate = with_store(|store| {
            let Some(record) = store.get(&envelope.key).map_err(|e| e.to_string())? else {
                return Ok(None);
            };
            store
                .accept(
                    envelope,
                    delivery.remaining_ms,
                    Admission::Inbox,
                    now_ms() as i64,
                )
                .map_err(|e| e.to_string())?;
            if matches!(record.state.as_str(), "expired" | "recipient_gone") {
                return Err(record.state);
            }
            Ok(Some((Accepted::Duplicate, record.delivered)))
        })?;
        if let Some(receipt) = duplicate {
            return Ok(receipt);
        }
        let target = MessageTarget::Agent {
            agent: envelope.target_agent.clone(),
        };
        let (ws, pane) = match self
            .resolve_message_target(&target)
            .map_err(|(code, reason)| format!("{code}: {reason}"))?
        {
            ResolvedTarget::Local(ws, pane) => (ws, pane),
            ResolvedTarget::Remote(_) => return Err("forward_limit".into()),
        };
        data.message.to_pane = self
            .public_pane_id(ws, pane)
            .ok_or("missing recipient pane")?;
        data.message.from_pane = None;
        data.message.from_host = Some(sender_host);
        data.message.message_key = Some(envelope.key.clone());
        data.message.enqueued_at_ms = now_ms();
        let unbound_muted = self.mailboxes.owes_deferral(&data.message)
            && self
                .mailboxes
                .muted_until(&data.message.to_pane, now_ms())
                .is_some()
            && self.node_id.as_deref() != Some(envelope.return_binding.recipient_node.as_str());
        let accepted = with_store(|store| {
            let existing = store.get(&envelope.key).map_err(|e| e.to_string())?;
            if existing.is_none()
                && self.mailboxes.queued_len(&data.message.to_pane)
                    >= crate::app::mailboxes::MAX_QUEUED_PER_PANE
            {
                return Err("mailbox_full".into());
            }
            store
                .accept(
                    envelope,
                    delivery.remaining_ms,
                    Admission::Inbox,
                    now_ms() as i64,
                )
                .map_err(|e| e.to_string())
        })?;
        if accepted == Accepted::New {
            self.emit_mesh_wake(&envelope.key.origin_node);
            self.queue_message_tiered(String::new(), data.message, Vec::new(), "unattested");
        }
        if unbound_muted {
            return Err(super::mesh_replies::UNAVAILABLE.into());
        }
        Ok((accepted, true))
    }

    pub(super) fn complete_mesh_send(
        &mut self,
        send: RelaySend,
        mut result: Result<bool, crate::peers::PeerMessageFailure>,
    ) {
        let delivery = &send.mesh;
        let mut warnings = Vec::new();
        if result
            .as_ref()
            .err()
            .is_some_and(|failure| failure.detail().contains("reply_unavailable"))
        {
            // The receiver queued the question. Only its return path is unavailable.
            warnings.push(format!(
                "reply_unavailable: {}",
                super::mesh_replies::UNAVAILABLE
            ));
            result = Ok(true);
        }
        let mut state = "queued";
        match result {
            Ok(true) => match with_store(|store| {
                store
                    .finish(&delivery.envelope.key, Outcome::Delivered, now_ms() as i64)
                    .map_err(|e| e.to_string())
            }) {
                Ok(()) => state = "delivered",
                Err(reason) => warnings.push(reason),
            },
            Ok(false) => warnings.push(format!(
                "custody accepted by {}; awaiting delivery",
                send.peer.name
            )),
            Err(failure) if !failure.retryable() => {
                let reason = failure.detail();
                match with_store(|store| {
                    store
                        .refuse(&delivery.envelope.key, reason, now_ms() as i64)
                        .map_err(|e| e.to_string())
                }) {
                    Ok(()) => {
                        state = "refused";
                        warnings.push(format!("refused by {}: {reason}", send.peer.name));
                        self.emit_event(EventEnvelope {
                            event: EventKind::MessageDelivered,
                            data: EventData::MessageDelivered {
                                correlation_id: send.correlation_id.clone(),
                                delivered: false,
                                outcome: format!("refused: {reason}"),
                                delivery_attempts: 0,
                                latency_ms: 0,
                            },
                        });
                    }
                    Err(error) => warnings.push(error),
                }
            }
            Err(failure) => warnings.push(format!(
                "queued for {}: {}",
                send.peer.name,
                failure.detail()
            )),
        }
        if state == "queued" {
            let spent = CUSTODY_TTL_MS.saturating_sub(delivery.remaining_ms);
            let delay = match spent {
                0..60_000 => 60_000,
                60_000..180_000 => 120_000,
                _ => 300_000,
            };
            let jitter = delivery
                .envelope
                .key
                .message_id
                .bytes()
                .map(i64::from)
                .sum::<i64>()
                % 1000;
            if let Err(reason) = with_store(|store| {
                store
                    .schedule_retry(
                        &delivery.envelope.key,
                        (delay + jitter).min(300_000),
                        now_ms() as i64,
                    )
                    .map_err(|e| e.to_string())
            }) {
                warnings.push(reason);
            }
        }
        if state == "queued" && delivery.envelope.request_key.is_some() {
            if let Err(reason) = with_store(|store| {
                store
                    .hold_answer(&delivery.envelope.key)
                    .map_err(|e| e.to_string())
            }) {
                warnings.push(reason);
            } else {
                state = "held";
                if self
                    .outbound_reply_peer(&delivery.envelope.return_binding.recipient_node)
                    .is_none()
                {
                    self.emit_mesh_wake(&delivery.envelope.return_binding.recipient_node);
                }
            }
        }
        if state == "delivered" {
            self.emit_event(EventEnvelope {
                event: EventKind::MessageRelayed,
                data: EventData::MessageRelayed {
                    correlation_id: send.correlation_id.clone(),
                    from_agent: send.from_agent.clone(),
                    to_agent: send.to_agent.clone(),
                    to_host: send.host.clone(),
                    route: send.peer.name.clone(),
                    relayed_at_ms: now_ms(),
                    intent: send.intent,
                    via: (!send.direct).then(|| send.peer.name.clone()),
                },
            });
        }
        self.mailboxes
            .finish_relaying_question(&send.correlation_id);
        if send.intent.wakes() && state != "refused" {
            self.mailboxes
                .record_relayed_question(send.correlation_id.clone());
        }
        if let Some(respond_to) = send.respond_to {
            let _ = respond_to.send(encode_success(
                send.id,
                ResponseResult::MsgQueued {
                    message_key: Some(delivery.envelope.key.clone()),
                    correlation_id: send.correlation_id,
                    state: state.into(),
                    warnings,
                    to_host: Some(send.host),
                    path: Some(if send.direct {
                        "direct".into()
                    } else {
                        format!("via {}", send.peer.name)
                    }),
                },
            ));
        }
    }

    pub(super) fn mark_mesh_inbox_read(&mut self, pane: &str) -> Result<Vec<MessageKey>, String> {
        if self.node_id.is_none() {
            return Ok(Vec::new());
        }
        let keys: Vec<_> = self
            .mailboxes
            .pending_messages()
            .into_iter()
            .filter(|message| message.to_pane == pane)
            .filter_map(|message| message.message_key)
            .collect();
        let (expired, changed) = with_store(|store| {
            let mut changed = Vec::new();
            for key in &keys {
                if store
                    .get(key)
                    .map_err(|e| e.to_string())?
                    .is_some_and(|record| record.state == "inbox")
                {
                    changed.push(key.origin_node.clone());
                }
            }
            let expired = store
                .read_inbox(&keys, now_ms() as i64)
                .map_err(|e| e.to_string())?;
            Ok((expired, changed))
        })?;
        for node in changed {
            self.emit_mesh_wake(&node);
        }
        Ok(expired)
    }

    pub(crate) fn initialize_mesh_mail(
        &mut self,
        server: Option<&crate::api::ServerHandle>,
    ) -> Result<(), String> {
        self.node_id = server.and_then(|server| server.node_id.clone());
        self.clone_detection_warning =
            server.and_then(|server| server.clone_detection_warning.clone());
        if crate::mesh::runtime_store::suspended()? {
            return Ok(());
        }
        self.restore_mesh_mail()?;
        self.restore_delivery_attempts();
        Ok(())
    }

    pub(crate) fn restore_mesh_mail(&mut self) -> Result<(), String> {
        let Some(origin) = self.node_id.clone() else {
            return Ok(());
        };
        let records = load_mesh_mail(
            origin,
            self.mailboxes.pending_messages(),
            self.fleet_pause.paused,
        )?;
        self.apply_mesh_mail(records);
        Ok(())
    }

    pub(crate) fn apply_mesh_mail(&mut self, records: Vec<RecoveredMessage>) {
        self.mailboxes.clear_queued_projection();
        for mut record in records {
            if !record.target.is_empty() {
                if let Some(location) = self.locate_agent(&record.target).filter(|l| l.local) {
                    record.message.to_pane = location.pane_id;
                }
            }
            if record.read {
                self.mailboxes.record_delivered(&record.message);
            } else {
                self.mailboxes.enqueue(record.message);
            }
        }
        self.sync_blocking_mail();
    }

    pub(super) fn retry_mesh_mail(&mut self) {
        if crate::mesh::runtime_store::recovery_reason().is_some() {
            if self
                .mesh_store_retry_at
                .is_none_or(|at| std::time::Instant::now() >= at)
            {
                self.resume_mesh_store(0);
            }
            return;
        }
        if self.node_id.is_none() {
            return;
        }
        let generation = crate::peer_stream::enrollment_generation();
        let now = std::time::Instant::now();
        let paused = self.fleet_pause.paused;
        if self.mesh_retry_at.is_some_and(|deadline| now < deadline)
            && self.mesh_enrollment_generation == generation
            && self.mesh_pause_seen == Some(paused)
        {
            return;
        }
        self.mesh_retry_at = Some(now + std::time::Duration::from_secs(1));
        let enrollment_changed = self.mesh_enrollment_generation != generation;
        let resumed = self.mesh_pause_seen != Some(false) && !paused;
        self.mesh_pause_seen = Some(paused);
        if let Err(reason) = with_store(|store| {
            store
                .set_paused(self.fleet_pause.paused, now_ms() as i64)
                .map_err(|e| e.to_string())
        }) {
            crate::logging::mesh_custody_failed("clock", error_code(&reason));
            return;
        }
        if !self.fleet_pause.paused
            && self
                .mesh_maintenance_at
                .is_none_or(|deadline| std::time::Instant::now() >= deadline)
        {
            if let Err(reason) = with_store(|store| {
                store
                    .maintain_if_due(now_ms() as i64)
                    .map_err(|e| e.to_string())
            }) {
                crate::logging::mesh_custody_failed("maintenance", error_code(&reason));
            }
            self.mesh_maintenance_at =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(60));
        }
        if !paused && (enrollment_changed || resumed) {
            let peers: Vec<String> = self
                .state
                .peers
                .iter()
                .filter_map(|peer| {
                    let status = crate::peer_stream::enrollment(peer);
                    (status.state == "pinned")
                        .then_some(status.node_id)
                        .flatten()
                })
                .collect();
            let origin = self.node_id.as_deref().unwrap_or_default();
            if let Err(reason) = with_store(|store| {
                store
                    .activate_held(origin, &peers, now_ms() as i64)
                    .map_err(|e| e.to_string())
            }) {
                crate::logging::mesh_custody_failed("held", error_code(&reason));
                return;
            }
        }
        self.mesh_enrollment_generation = generation;
        if self.fleet_pause.paused || !self.message_relays.is_idle() {
            return;
        }
        let pushable: Vec<String> = self
            .state
            .peers
            .iter()
            .filter_map(|peer| {
                let status = crate::peer_stream::enrollment(peer);
                (status.state == "pinned")
                    .then_some(status.node_id)
                    .flatten()
            })
            .collect();
        let records = with_store(|store| {
            let limit = crate::mesh::delivery::push_concurrency();
            let mut keys = store
                .push_ready(now_ms() as i64, limit, &pushable)
                .map_err(|e| e.to_string())?;
            // Preserve the existing request retry lane for sends accepted
            // before their peer had an identity pin.
            if keys.len() < limit {
                keys.extend(
                    store
                        .retry_ready_limit(now_ms() as i64, limit - keys.len())
                        .map_err(|e| e.to_string())?,
                );
            }
            keys.into_iter()
                .map(|key| store.get(&key).map_err(|e| e.to_string()))
                .collect::<Result<Vec<_>, _>>()
        });
        let Ok(records) = records else {
            return;
        };
        for record in records.into_iter().flatten() {
            let Ok(data) = payload(&record.envelope) else {
                continue;
            };
            if Some(record.envelope.key.origin_node.as_str()) != self.node_id.as_deref() {
                continue;
            }
            let peer = if record.envelope.request_key.is_some() {
                self.outbound_reply_peer(&record.envelope.return_binding.recipient_node)
                    .filter(|peer| crate::peer_stream::enrollment(peer).state == "pinned")
            } else {
                self.state
                    .peers
                    .iter()
                    .find(|peer| Some(&peer.name) == data.peer.as_ref())
                    .cloned()
            };
            let Some(peer) = peer else {
                if record.envelope.request_key.is_some() {
                    let _ = with_store(|store| {
                        store
                            .hold_answer(&record.envelope.key)
                            .map_err(|e| e.to_string())
                    });
                }
                continue;
            };
            let host = data.host.unwrap_or_else(|| peer.name.clone());
            let message = data.message;
            let send = RelaySend {
                mesh: Deliver {
                    envelope: record.envelope.clone(),
                    remaining_ms: record.remaining_ms,
                },
                id: String::new(),
                peer,
                to_agent: record.envelope.target_agent,
                host,
                direct: data.direct,
                from_agent: message.from_agent.unwrap_or_default(),
                correlation_id: message.correlation_id,
                intent: message.intent,
                respond_to: None,
            };
            self.enqueue_message_relay(send.into_work());
        }
    }
}

pub(super) fn error_code(reason: &str) -> &'static str {
    match reason.split(':').next() {
        Some("mailbox_full") => "mailbox_full",
        Some("mail_store_full") => "mail_store_full",
        Some("fleet_paused") => "fleet_paused",
        Some("message_expired") => "message_expired",
        Some("message_not_found") => "message_not_found",
        Some("message has no valid mesh return binding") => "reply_unavailable",
        _ => "mail_store_unavailable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn status_shows_quarantine_count() {
        let fixture = crate::mesh::runtime_store::TestStore::new();
        let delivery = fixture.delivery();
        with_store(|store| {
            store
                .accept(
                    &delivery.envelope,
                    CUSTODY_TTL_MS,
                    Admission::Inbox,
                    now_ms() as i64,
                )
                .map_err(|e| e.to_string())?;
            store
                .quarantine(&delivery.envelope.key)
                .map_err(|e| e.to_string())
        })
        .unwrap();
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        let response = app.handle_api_request(crate::api::schema::Request {
            id: "quarantine".into(),
            method: crate::api::schema::Method::PeersEnrollment(crate::api::schema::EmptyParams {}),
        });
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["result"]["mesh_quarantined"], 1);
    }

    #[tokio::test]
    async fn retry_batch_uses_fixed_push_concurrency() {
        use std::os::unix::fs::PermissionsExt;
        let shim = std::env::temp_dir().join(format!("flock-retry-shim-{}", std::process::id()));
        std::fs::create_dir_all(&shim).unwrap();
        let ssh = shim.join("ssh");
        std::fs::write(&ssh, "#!/bin/sh\nexit 255\n").unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let previous_path = std::env::var_os("PATH");
        let mut paths = vec![shim.clone()];
        if let Some(path) = &previous_path {
            paths.extend(std::env::split_paths(path));
        }
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        let _store = crate::mesh::runtime_store::TestStore::new();
        let (_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.node_id = Some("nodea".into());
        let peer = crate::config::PeerConfig {
            name: "nodeb".into(),
            ssh: "retry-batch.invalid".into(),
            ..Default::default()
        };
        app.state.peers.push(peer.clone());
        for index in 0..5 {
            let data = Payload {
                message: PendingMessage {
                    message_key: None,
                    correlation_id: format!("batch-{index}"),
                    body: "retry me".into(),
                    from_pane: None,
                    from_agent: Some("agent_nodea_sender".into()),
                    from_host: Some("nodea".into()),
                    from_repo: None,
                    to_pane: "p1".into(),
                    to_repo: None,
                    in_reply_to: None,
                    enqueued_at_ms: now_ms(),
                    delivery_attempts: 0,
                    intent: crate::api::schema::MsgIntent::Fyi,
                },
                peer: Some(peer.name.clone()),
                host: Some(peer.name.clone()),
                direct: true,
            };
            let envelope = envelope("nodea", "agent_nodeb_recipient".into(), &data).unwrap();
            with_store(|store| {
                store
                    .accept(
                        &envelope,
                        CUSTODY_TTL_MS,
                        Admission::Custody,
                        now_ms() as i64,
                    )
                    .map_err(|e| e.to_string())
            })
            .unwrap();
        }
        app.retry_mesh_mail();
        assert_eq!(app.message_relays.slots(5), 1, "four records dispatched");
        let remaining = with_store(|store| {
            store
                .retry_ready_limit(now_ms() as i64, 10)
                .map_err(|e| e.to_string())
        })
        .unwrap();
        assert_eq!(
            remaining.len(),
            1,
            "the fifth record is not leased by this batch"
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !app.message_relays.is_idle() {
                let event = app.event_rx.recv().await.unwrap();
                app.handle_internal_event(event);
            }
        })
        .await
        .unwrap();
        assert!(app.message_relays.is_idle());
        match previous_path {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
        let _ = std::fs::remove_dir_all(shim);
    }
}

#[derive(Debug)]
pub(crate) struct RecoveredMessage {
    message: PendingMessage,
    target: String,
    read: bool,
}

/// Load and decode on the recovery worker, projecting only on the app loop.
pub(crate) fn load_mesh_mail(
    origin: String,
    legacy: Vec<PendingMessage>,
    paused: bool,
) -> Result<Vec<RecoveredMessage>, String> {
    with_store(|store| {
        if !store
            .migration_done("audit-inbox-v1")
            .map_err(|e| e.to_string())?
        {
            for message in legacy {
                // A deterministic migration key makes a crash before the marker harmless.
                use sha2::{Digest, Sha256};
                let digest = Sha256::digest(
                    format!("{}:{}", message.to_pane, message.correlation_id).as_bytes(),
                );
                let data = Payload {
                    message,
                    peer: None,
                    host: None,
                    direct: true,
                };
                let mut envelope = envelope(&origin, String::new(), &data)?;
                envelope.key.message_id = format!(
                    "0{}",
                    digest[..25]
                        .iter()
                        .map(|b| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ"[(b & 31) as usize] as char)
                        .collect::<String>()
                );
                envelope.return_binding.request = envelope.key.clone();
                if store
                    .get(&envelope.key)
                    .map_err(|e| e.to_string())?
                    .is_none()
                {
                    store
                        .accept(&envelope, CUSTODY_TTL_MS, Admission::Inbox, now_ms() as i64)
                        .map_err(|e| e.to_string())?;
                }
            }
            store
                .finish_migration("audit-inbox-v1")
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    })?;
    let records = with_store(|store| {
        if !paused {
            store
                .maintain_if_due(now_ms() as i64)
                .map_err(|e| e.to_string())?;
        }
        store
            .mailbox_keys()
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|key| match store.get(&key) {
                Err(
                    crate::mesh::store::Error::Json(_) | crate::mesh::store::Error::InvalidEnvelope,
                ) => {
                    store.quarantine(&key).map_err(|e| e.to_string())?;
                    crate::logging::mesh_custody_failed("restore", "undecodable_record");
                    Ok(None)
                }
                result => result.map_err(|e| e.to_string()),
            })
            .collect::<Result<Vec<_>, _>>()
    })?;
    let mut loaded = Vec::new();
    for record in records.into_iter().flatten() {
        let mut data = match payload(&record.envelope) {
            Ok(data) => data,
            Err(_) => {
                with_store(|store| {
                    store
                        .quarantine(&record.envelope.key)
                        .map_err(|e| e.to_string())
                })?;
                crate::logging::mesh_custody_failed("restore", "undecodable_record");
                continue;
            }
        };
        if Some(record.envelope.key.origin_node.as_str()) != Some(origin.as_str()) {
            data.message.from_host = with_store(|store| {
                store
                    .origin_name(&record.envelope.key.origin_node)
                    .map_err(|e| e.to_string())
            })?;
        }
        data.message.message_key = Some(record.envelope.key);
        data.message.enqueued_at_ms =
            now_ms().saturating_sub((record.mailbox_ttl_ms - record.remaining_ms).max(0) as u64);
        loaded.push(RecoveredMessage {
            message: data.message,
            target: record.envelope.target_agent,
            read: record.state == "read",
        });
    }
    Ok(loaded)
}
