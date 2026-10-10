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

    pub(super) fn persist_mesh_send(
        &mut self,
        owner: &str,
        next: &super::mesh_forward::NextHop,
        to_agent: &str,
        data: &Payload,
    ) -> Result<Deliver, String> {
        if self.fleet_pause.paused {
            return Err("fleet_paused".into());
        }
        let origin = self
            .node_id
            .as_deref()
            .ok_or("mesh node identity unavailable")?;
        let mut envelope = envelope(origin, to_agent.into(), data)?;
        envelope.return_binding.recipient_node = owner.into();
        envelope.return_binding.collection_peers = vec![owner.into()];
        let identity = crate::mesh::identity::NodeIdentity::load().map_err(|e| e.to_string())?;
        crate::mesh::sign::seal(&mut envelope, &identity);
        let hops_left = crate::mesh::delivery::hop_limit();
        with_store(|store| {
            store
                .accept_origin(
                    &envelope,
                    &next.node,
                    next.admission(),
                    hops_left,
                    now_ms() as i64,
                )
                .map_err(|e| e.to_string())?;
            if next.peer.is_some() {
                store
                    .schedule_retry(&envelope.key, 60_000, now_ms() as i64)
                    .map_err(|e| e.to_string())?;
            }
            Ok(())
        })?;
        self.mesh_retry_at = None;
        Ok(Deliver {
            visited: vec![origin.into()],
            hops_left,
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
        let (ws, pane) = self
            .resolve_pane_target(&message.to_pane)
            .map_err(|e| e.message)?;
        let (agent, session) = self
            .local_recipient_identity(ws, pane)
            .ok_or("missing recipient")?;
        envelope.target_agent = agent.clone();
        envelope.target_session = session.clone();
        with_store(|store| {
            store
                .accept_local(&envelope, CUSTODY_TTL_MS, &agent, &session, now_ms() as i64)
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
        let upstream = edge.node_id.clone().ok_or("mesh edge is not enrolled")?;
        self.import_attested_mesh_mail(delivery, &upstream)
    }

    pub(super) fn import_attested_mesh_mail(
        &mut self,
        delivery: &Deliver,
        upstream: &str,
    ) -> Result<(Accepted, bool), String> {
        let sender_host = self.check_mesh_import(delivery, upstream)?;
        let envelope = &delivery.envelope;
        if (envelope.request_key.is_some() || envelope.kind == crate::mesh::store::Kind::Receipt)
            && self.node_id.as_deref() != Some(envelope.return_binding.recipient_node.as_str())
        {
            return self.accept_forwarded_request(delivery);
        }
        if envelope.kind == crate::mesh::store::Kind::Receipt {
            return self.import_mesh_receipt(envelope);
        }
        if let Some(request) = &envelope.request_key {
            self.import_mesh_answer(request, delivery, None)?;
            return Ok((Accepted::New, true));
        }
        let mut data = payload(envelope).map_err(|_| "invalid_envelope")?;
        if data.message.correlation_id != envelope.correlation_id
            || data.message.from_agent.as_deref().unwrap_or_default() != envelope.sender
            || envelope.return_binding.request != envelope.key
        {
            return Err("invalid_envelope: inconsistent mesh envelope".into());
        }
        if self.node_id.as_deref() != Some(envelope.return_binding.recipient_node.as_str()) {
            return self.accept_forwarded_request(delivery);
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
        if self.removed_agent(&envelope.target_agent)? {
            return Err(crate::mesh::store::Error::RecipientGone.to_string());
        }
        let target = MessageTarget::Agent {
            agent: envelope.target_agent.clone(),
        };
        let (ws, pane) = match self
            .resolve_message_target(&target)
            .map_err(|(code, reason)| format!("{code}: {reason}"))?
        {
            ResolvedTarget::Local(ws, pane) => (ws, pane),
            ResolvedTarget::Remote(_) => return Err("recipient_offline".into()),
        };
        let (agent, session) = self
            .local_recipient_identity(ws, pane)
            .ok_or("missing recipient")?;
        data.message.to_pane = self
            .public_pane_id(ws, pane)
            .ok_or("missing recipient pane")?;
        data.message.from_pane = None;
        data.message.from_host = sender_host;
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
                .accept_local_delivery(delivery, &agent, &session, now_ms() as i64)
                .map_err(|e| e.to_string())
        })?;
        self.route_mesh_receipts();
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
        let mut retry = true;
        match result {
            Ok(true) => match with_store(|store| {
                store
                    .finish(&delivery.envelope.key, Outcome::Delivered, now_ms() as i64)
                    .map_err(|e| e.to_string())
            }) {
                Ok(()) => state = "delivered",
                Err(reason) => warnings.push(reason),
            },
            Ok(false) => {
                retry = false;
                if let Err(reason) = with_store(|store| {
                    store
                        .finish(
                            &delivery.envelope.key,
                            Outcome::Transferred,
                            now_ms() as i64,
                        )
                        .map_err(|e| e.to_string())
                }) {
                    warnings.push(reason);
                }
            }
            Err(crate::peers::PeerMessageFailure::Reroute(reason)) => {
                retry = false;
                if let Err(error) = with_store(|store| {
                    store
                        .set_next_hop(&delivery.envelope.key, "")
                        .map_err(|e| e.to_string())
                }) {
                    warnings.push(error);
                }
                warnings.push(reason);
            }
            Err(failure) if !failure.retryable() => {
                let reason = failure.detail();
                match with_store(|store| {
                    store
                        .refuse(&delivery.envelope.key, reason, now_ms() as i64)
                        .map_err(|e| e.to_string())
                }) {
                    Ok(()) => {
                        state = if reason.split(':').next() == Some("recipient_gone") {
                            "recipient_gone"
                        } else {
                            "refused"
                        };
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
        if state == "queued" && retry {
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
        if state == "queued" && retry && delivery.envelope.request_key.is_some() {
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
        let (rejected, changed) = with_store(|store| {
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
            let rejected = store
                .read_inbox(&keys, now_ms() as i64)
                .map_err(|e| e.to_string())?;
            Ok((rejected, changed))
        })?;
        self.route_mesh_receipts();
        for node in changed {
            self.emit_mesh_wake(&node);
        }
        Ok(rejected)
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

    /// Snapshot native bindings before recovery leaves the app loop.
    pub(crate) fn mesh_recovery_recipients(
        &self,
    ) -> std::collections::HashMap<String, (String, String)> {
        let mut recipients = std::collections::HashMap::new();
        for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
            for pane in ws.tabs.iter().flat_map(|tab| tab.layout.pane_ids()) {
                if let (Some(public), Some(identity)) = (
                    self.public_pane_id(ws_idx, pane),
                    self.local_recipient_identity(ws_idx, pane),
                ) {
                    recipients.insert(public, identity);
                }
            }
        }
        // Legacy queued events may still name an alias from before handoff.
        for message in self.mailboxes.pending_messages() {
            if let Ok((ws, pane)) = self.resolve_pane_target(&message.to_pane) {
                if let Some(identity) = self.local_recipient_identity(ws, pane) {
                    recipients.insert(message.to_pane, identity);
                }
            }
        }
        recipients
    }

    pub(crate) fn restore_mesh_mail(&mut self) -> Result<(), String> {
        let Some(origin) = self.node_id.clone() else {
            return Ok(());
        };
        let records = load_mesh_mail(
            origin,
            self.mailboxes.pending_messages(),
            self.fleet_pause.paused,
            self.mesh_recovery_recipients(),
        )?;
        self.apply_mesh_mail(records);
        Ok(())
    }

    pub(crate) fn apply_mesh_mail(&mut self, records: Vec<RecoveredMessage>) {
        self.mailboxes.clear_queued_projection();
        for mut record in records {
            // Lifecycle removals can arrive while the recovery worker loads rows.
            if self
                .pending_agent_removals
                .iter()
                .any(|removal| removal.agent == record.target)
            {
                continue;
            }
            if !record.target.is_empty() {
                if let Some(location) = self.locate_agent(&record.target).filter(|l| l.local) {
                    record.message.to_pane = location.pane_id;
                }
            }
            if record.message.to_pane.is_empty() {
                continue;
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
        if paused {
            return;
        }
        let route_generation = self.mesh_routes.table.generation();
        if self.mesh_forward_generation != Some(route_generation) {
            let adjacent: Vec<_> = self
                .mesh_routes
                .table
                .routes()
                .into_iter()
                .filter(|route| route.hops == 1)
                .map(|route| route.node)
                .collect();
            let targets: Vec<_> = self
                .mesh_routes
                .table
                .routes()
                .into_iter()
                .map(|route| route.node)
                .collect();
            let after = self
                .mesh_forward_cursor
                .as_ref()
                .filter(|(generation, _)| *generation == route_generation)
                .map(|(_, key)| key);
            let keys = with_store(|store| {
                store
                    .withdraw_request_hops(&adjacent)
                    .map_err(|e| e.to_string())?;
                store
                    .routable_requests(&targets, after, 256)
                    .map_err(|e| e.to_string())
            });
            if let Ok((keys, more)) = keys {
                // Advance even if a downstream refusal clears a just-assigned
                // hop. That row must wait for another generation, not this pass.
                if let Some(last) = keys.last() {
                    self.mesh_forward_cursor = Some((route_generation, last.clone()));
                }
                for key in keys {
                    let record = with_store(|store| {
                        store
                            .collection_record(&key, now_ms() as i64)
                            .map_err(|e| e.to_string())
                    });
                    let Ok(Some(record)) = record else {
                        continue;
                    };
                    let next =
                        self.request_next_hop(&record.envelope.return_binding.recipient_node);
                    if next.node != record.next_hop {
                        let _ = with_store(|store| {
                            store
                                .route_custody(&key, &next.node, next.admission())
                                .map_err(|e| e.to_string())
                        });
                        self.emit_mesh_wake(&next.node);
                    }
                }
                if !more {
                    self.mesh_forward_cursor = None;
                    self.mesh_forward_generation = Some(route_generation);
                }
            }
        }
        // Leave a worker available for new user sends when a retry edge stalls.
        let cap = crate::mesh::delivery::push_concurrency();
        let limit = self
            .message_relays
            .slots(cap)
            .saturating_sub(usize::from(cap > 1));
        if limit == 0 {
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
        let keys = with_store(|store| {
            store
                .push_ready(now_ms() as i64, limit, &pushable)
                .map_err(|e| e.to_string())
        });
        let Ok(keys) = keys else {
            return;
        };
        for key in keys {
            let record = with_store(|store| {
                let Some(mut record) = store
                    .collection_record(&key, now_ms() as i64)
                    .map_err(|e| e.to_string())?
                else {
                    return Ok(None);
                };
                store
                    .seal_local_record(&mut record)
                    .map_err(|e| e.to_string())?;
                Ok(Some(record))
            });
            let Ok(Some(record)) = record else {
                continue;
            };
            let Some(peer) = self
                .outbound_reply_peer(&record.next_hop)
                .filter(|peer| crate::peer_stream::enrollment(peer).state == "pinned")
            else {
                continue;
            };
            let decoded = if record.envelope.kind == crate::mesh::store::Kind::Receipt {
                super::mesh_receipts::decode_receipt(&record.envelope)
                    .map(|_| {
                        (
                            peer.name.clone(),
                            true,
                            record.envelope.sender.clone(),
                            record.envelope.correlation_id.clone(),
                            crate::api::schema::MsgIntent::Fyi,
                        )
                    })
                    .map_err(|e| e.to_string())
            } else {
                payload(&record.envelope).map(|data| {
                    (
                        data.host.unwrap_or_else(|| peer.name.clone()),
                        data.direct,
                        data.message.from_agent.unwrap_or_default(),
                        data.message.correlation_id,
                        data.message.intent,
                    )
                })
            };
            let (host, direct, from_agent, correlation_id, intent) = match decoded {
                Ok(data) => data,
                Err(_) => {
                    let _ = with_store(|store| store.quarantine(&key).map_err(|e| e.to_string()));
                    continue;
                }
            };
            let send = RelaySend {
                mesh: Deliver {
                    envelope: record.envelope.clone(),
                    remaining_ms: record.remaining_ms,
                    hops_left: record.hops_left,
                    visited: record.visited,
                },
                id: String::new(),
                peer,
                to_agent: record.envelope.target_agent,
                host,
                direct,
                from_agent,
                correlation_id,
                intent,
                respond_to: None,
            };
            self.enqueue_message_relay(send.into_work());
        }
    }
}

pub(super) fn error_code(reason: &str) -> &'static str {
    match reason.split(':').next() {
        Some("mailbox_full") => "mailbox_full",
        Some("recipient_gone") => "recipient_gone",
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
    async fn recovery_keeps_native_bindings_and_filters_removed_recipients() {
        let _store = crate::mesh::runtime_store::TestStore::new();
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("recovery")];
        app.state.ensure_test_terminals();
        let pane = app.state.workspaces[0].focused_pane_id().unwrap();
        let terminal_id = app.state.terminal_id_for_pane(0, pane).unwrap();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("fixture".into());
        terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
            source: "flock:codex".into(),
            agent: "codex".into(),
            session_ref: crate::agent_resume::AgentSessionRef::id("native-session").unwrap(),
        });
        let agent = terminal.agent_id.to_string();
        let removal = crate::app::agent_removal::capture(
            terminal,
            crate::app::agent_removal::RemovalEvent::Kill,
        )
        .unwrap();
        let public = app.public_pane_id(0, pane).unwrap();
        let message = PendingMessage {
            message_key: None,
            correlation_id: "legacy".into(),
            body: "pending".into(),
            from_pane: None,
            from_agent: None,
            from_host: None,
            from_repo: None,
            to_pane: public.clone(),
            to_repo: None,
            in_reply_to: None,
            enqueued_at_ms: now_ms(),
            delivery_attempts: 0,
            intent: crate::api::schema::MsgIntent::NeedsReply,
        };
        let loaded = load_mesh_mail(
            "nodea".into(),
            vec![message.clone()],
            false,
            app.mesh_recovery_recipients(),
        )
        .unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].target, agent);
        let key = loaded[0].message.message_key.clone().unwrap();
        // A removal arriving after the worker's read cannot resurrect queued mail.
        app.pending_agent_removals.push_back(removal);
        app.apply_mesh_mail(loaded);
        assert_eq!(app.mailboxes.queued_len(&public), 0);
        let affected = with_store(|store| {
            store
                .tombstone(&agent, "native-session", "killed", now_ms() as i64)
                .map_err(|e| e.to_string())
        })
        .unwrap();
        assert_eq!(
            affected,
            vec![key.clone()],
            "worker backfilled the native binding"
        );
        let data = Payload {
            message,
            peer: None,
            host: None,
            direct: true,
        };
        let mut answer = envelope("nodeb", agent, &data).unwrap();
        answer.request_key = Some(key);
        with_store(|store| {
            store
                .accept(&answer, CUSTODY_TTL_MS, Admission::Inbox, now_ms() as i64)
                .map_err(|e| e.to_string())
        })
        .unwrap();
        let loaded = load_mesh_mail(
            "nodea".into(),
            Vec::new(),
            false,
            app.mesh_recovery_recipients(),
        )
        .unwrap();
        assert!(
            loaded.is_empty(),
            "removed senders' answers stay out of the projection"
        );
        assert!(
            with_store(|store| store.get(&answer.key).map_err(|e| e.to_string()))
                .unwrap()
                .is_some(),
            "the answer remains in durable status"
        );
    }

    #[tokio::test]
    async fn retry_never_leases_mail_without_a_pinned_next_hop() {
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
        assert_eq!(
            app.message_relays.slots(5),
            5,
            "unpinned peer is not pushable"
        );
        let remaining = with_store(|store| {
            store
                .retry_ready_limit(now_ms() as i64, 10)
                .map_err(|e| e.to_string())
        })
        .unwrap();
        assert_eq!(remaining.len(), 5, "no record was leased by the retry pass");
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
    recipients: std::collections::HashMap<String, (String, String)>,
) -> Result<Vec<RecoveredMessage>, String> {
    with_store(|store| {
        store.set_local_node(&origin);
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
        let mut target = record.envelope.target_agent.clone();
        if record.envelope.request_key.is_some() {
            let removed = with_store(|store| {
                store
                    .agent_removed(&target, now_ms() as i64)
                    .map_err(|e| e.to_string())
            })?;
            if removed {
                continue;
            }
        } else if !paused {
            let identity = recipients
                .get(&data.message.to_pane)
                .filter(|(agent, _)| target.is_empty() || target == *agent)
                .or_else(|| recipients.values().find(|(agent, _)| *agent == target));
            if let Some((agent, session)) = identity {
                with_store(|store| {
                    store
                        .bind_local_recipient(&record.envelope.key, agent, session)
                        .map_err(|e| e.to_string())
                })?;
                target.clone_from(agent);
            }
        }
        data.message.message_key = Some(record.envelope.key);
        data.message.enqueued_at_ms =
            now_ms().saturating_sub((record.mailbox_ttl_ms - record.remaining_ms).max(0) as u64);
        loaded.push(RecoveredMessage {
            message: data.message,
            target,
            read: record.state == "read",
        });
    }
    Ok(loaded)
}
