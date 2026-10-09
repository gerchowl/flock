//! Answers are accepted against the stored request, independent of discovery.
use super::{
    mesh_mail::{envelope, payload, Payload},
    messages::{now_ms, queued_event},
    responses::{encode_error, encode_success},
};
use crate::{
    api::schema::{EventData, EventEnvelope, EventKind, ResponseResult},
    app::{mailboxes::PendingMessage, App},
    mesh::{
        collect::{AnswerCollect, Collect, Completion},
        delivery::Deliver,
        hello::with_store,
        key::MessageKey,
        store::{Accepted, Admission, Envelope, CUSTODY_TTL_MS},
    },
};

pub(super) const UNAVAILABLE: &str = "message has no valid mesh return binding";

impl App {
    pub(super) fn persist_mesh_reply(
        &mut self,
        request: &MessageKey,
        mut message: PendingMessage,
    ) -> Result<(Envelope, &'static str), String> {
        if self.fleet_pause.paused {
            return Err("fleet_paused".into());
        }
        let origin = self
            .node_id
            .clone()
            .ok_or("mesh node identity unavailable")?;
        let original = with_store(|store| {
            store
                .collection_record(request, now_ms() as i64)
                .map_err(|e| e.to_string())
        })?
        .ok_or("message_not_found")?;
        if original.remaining_ms == 0
            || matches!(original.state.as_str(), "expired" | "inbox_expired")
        {
            return Err("message_expired".into());
        }
        if original.envelope.return_binding.request != *request
            || original.envelope.return_binding.recipient_node != origin
            || !matches!(original.state.as_str(), "inbox" | "read")
        {
            return Err(UNAVAILABLE.into());
        }
        if crate::app::mailboxes::is_deferral(&message.correlation_id) {
            let prior = with_store(|store| {
                for key in store.by_request_key(request).map_err(|e| e.to_string())? {
                    if let Some(record) = store.get(&key).map_err(|e| e.to_string())? {
                        if record.envelope.correlation_id == message.correlation_id {
                            return Ok(Some(record.envelope));
                        }
                    }
                }
                Ok(None)
            })?;
            if let Some(answer) = prior {
                return Ok((answer, "held"));
            }
        }
        let local = request.origin_node == origin;
        message.to_pane = if local {
            let data = payload(&original.envelope)?;
            data.message.from_pane.unwrap_or_default()
        } else {
            String::new()
        };
        if local && !original.envelope.sender.is_empty() {
            message.to_pane = self
                .locate_agent(&original.envelope.sender)
                .filter(|location| location.local)
                .map(|location| location.pane_id)
                .unwrap_or_default();
        }
        if !message.to_pane.is_empty()
            && self.mailboxes.queued_len(&message.to_pane)
                >= crate::app::mailboxes::MAX_QUEUED_PER_PANE
        {
            return Err("mailbox_full".into());
        }
        let push_peer = if local {
            None
        } else {
            self.outbound_reply_peer(&request.origin_node)
                .filter(|peer| crate::peer_stream::enrollment(peer).state == "pinned")
        };
        let data = Payload {
            message: message.clone(),
            peer: push_peer.as_ref().map(|peer| peer.name.clone()),
            host: push_peer.as_ref().map(|peer| peer.name.clone()),
            direct: true,
        };
        let mut answer = envelope(&origin, original.envelope.sender.clone(), &data)?;
        answer.request_key = Some(request.clone());
        answer.return_binding.collection_token =
            original.envelope.return_binding.collection_token.clone();
        answer.return_binding.recipient_node = request.origin_node.clone();
        answer.return_binding.collection_peers = vec![request.origin_node.clone()];
        with_store(|store| {
            store
                .accept(
                    &answer,
                    CUSTODY_TTL_MS,
                    if local {
                        Admission::Inbox
                    } else if push_peer.is_some() {
                        Admission::Custody
                    } else {
                        Admission::Held
                    },
                    now_ms() as i64,
                )
                .map_err(|e| e.to_string())?;
            Ok(())
        })?;
        if !local && push_peer.is_none() {
            self.emit_mesh_wake(&request.origin_node);
        }
        message.message_key = Some(answer.key.clone());
        let state = if local && !message.to_pane.is_empty() {
            "queued"
        } else {
            "held"
        };
        if local {
            self.project_mesh_answer(message);
        } else if let Some(peer) = push_peer {
            let send = crate::app::message_relay::RelaySend {
                mesh: Some(Deliver {
                    envelope: answer.clone(),
                    remaining_ms: CUSTODY_TTL_MS,
                }),
                id: answer.key.message_id.clone(),
                host: peer.name.clone(),
                peer,
                to_agent: answer.target_agent.clone(),
                direct: true,
                from_agent: answer.sender.clone(),
                from_host: crate::app::short_host_name(),
                body: message.body,
                correlation_id: answer.correlation_id.clone(),
                in_reply_to: answer.in_reply_to.clone(),
                intent: message.intent,
                settle_original: None,
                respond_to: None,
            };
            self.enqueue_message_relay(send.into_work());
        }
        Ok((answer, state))
    }

    fn project_mesh_answer(&mut self, message: PendingMessage) {
        if message.to_pane.is_empty() {
            self.emit_event(EventEnvelope {
                event: EventKind::MessageQueued,
                data: queued_event(&message),
            });
        } else {
            self.queue_message_tiered(String::new(), message, Vec::new(), "unattested");
        }
    }

    pub(super) fn handle_mesh_collect(&mut self, id: String, query: Collect) -> String {
        let result = (|| {
            if self.fleet_pause.paused {
                return Err("fleet_paused".into());
            }
            let origin = self
                .inbound
                .edge(
                    self.current_api_peer_pid,
                    crate::platform::process_start_time,
                )
                .map(|edge| &edge.enrollment)
                .filter(|edge| edge.state == "pinned")
                .and_then(|edge| edge.node_id.as_deref())
                .ok_or("mesh edge is not enrolled")?;
            with_store(|store| {
                let mut refusals = Vec::new();
                if let Collect::Outbound { outbound } = &query {
                    for ack in &outbound.ack {
                        if let Some(reason) = &ack.refusal {
                            if let Some(record) = store
                                .collection_record(&ack.key, now_ms() as i64)
                                .map_err(|e| e.to_string())?
                            {
                                if record.state == "held" && record.remaining_ms > 0 {
                                    refusals.push((record.envelope.correlation_id, reason.clone()));
                                }
                            }
                        }
                    }
                }
                let answers = match &query {
                    Collect::Answers(query) => {
                        store.collect_answers(origin, query, now_ms() as i64)
                    }
                    Collect::Outbound { outbound } => store.collect_outbound(
                        crate::mesh::store::Offer::All,
                        self.node_id
                            .as_deref()
                            .ok_or("mesh node identity unavailable")?,
                        origin,
                        outbound,
                        now_ms() as i64,
                    ),
                }
                .map_err(|e| e.to_string())?;
                let receipt = match &query {
                    Collect::Answers(query) => store
                        .receipt(&query.request, now_ms() as i64)
                        .map_err(|e| e.to_string())?,
                    Collect::Outbound { .. } => None,
                };
                Ok((answers, refusals, receipt))
            })
        })();
        match result {
            Ok((answers, refusals, receipt)) => {
                for (correlation_id, reason) in refusals {
                    self.emit_event(EventEnvelope {
                        event: EventKind::MessageDelivered,
                        data: EventData::MessageDelivered {
                            correlation_id,
                            delivered: false,
                            outcome: format!("refused: {reason}"),
                            delivery_attempts: 0,
                            latency_ms: 0,
                        },
                    });
                }
                encode_success(id, ResponseResult::MeshCollected { answers, receipt })
            }
            Err(reason) => encode_error(id, "mesh_collection_refused", reason),
        }
    }

    pub(crate) fn tick_mesh_collections(&mut self) {
        let Some(origin) = self.node_id.clone() else {
            return;
        };
        let now = std::time::Instant::now();
        if self.mesh_collect_at.is_some_and(|deadline| deadline > now) {
            return;
        }
        self.mesh_collect_at = Some(now + std::time::Duration::from_secs(1));
        self.inbound.prune(crate::platform::process_start_time);
        if self.fleet_pause.paused {
            return;
        }
        let cap = crate::mesh::collect::POLL_CONCURRENCY;
        let slots = self.collection_relays.slots(cap);
        if slots == 0 {
            return;
        }
        let generation = crate::peer_stream::enrollment_generation();
        let reconnected: Vec<String> = if generation != self.collection_generation {
            self.state
                .peers
                .iter()
                .filter_map(|peer| {
                    let status = crate::peer_stream::enrollment(peer);
                    (status.state == "pinned")
                        .then_some(status.node_id)
                        .flatten()
                })
                .collect()
        } else {
            Vec::new()
        };
        self.collection_generation = generation;
        let busy: Vec<String> = self.collection_peers.values().cloned().collect();
        let records = with_store(|store| {
            store
                .collect_fast(
                    &origin,
                    &self.event_hub.reply_waits(),
                    &reconnected,
                    now_ms() as i64,
                )
                .map_err(|e| e.to_string())?;
            store
                .collect_ready(&origin, now_ms() as i64, slots, &busy)
                .map_err(|e| e.to_string())
        });
        let Ok(records) = records else { return };
        for record in records {
            let binding = record.envelope.return_binding;
            let Some(peer) = self.outbound_reply_peer(&binding.recipient_node) else {
                continue;
            };
            let ack = with_store(|store| {
                store
                    .collection_acks(&record.envelope.key)
                    .map_err(|e| e.to_string())
            })
            .unwrap_or_default();
            self.start_collection(
                peer,
                Collect::Answers(AnswerCollect {
                    request: record.envelope.key,
                    token: binding.collection_token,
                    ack,
                }),
            );
        }
        let mut peers = self.state.peers.clone();
        if !peers.is_empty() {
            let count = peers.len();
            peers.rotate_left(self.mesh_outbound_cursor % count);
            self.mesh_outbound_cursor = (self.mesh_outbound_cursor + slots) % count;
        }
        for peer in peers {
            let edge = crate::peer_stream::enrollment(&peer);
            if let Some(node) = edge.node_id.as_deref().filter(|_| edge.state == "pinned") {
                if !self.collection_peers.contains_key(&peer.name)
                    && crate::peer_stream::take_wake(&peer)
                {
                    self.mesh_outbound_polls
                        .entry(peer.name.clone())
                        .or_default()
                        .note_wake();
                }
                // The hub's own read receipts are work even after the spoke
                // has drained its outbox and stopped advertising pending mail.
                let receipts_pending = with_store(|store| {
                    store
                        .pending_receipts(node, now_ms() as i64)
                        .map(|receipts| !receipts.is_empty())
                        .map_err(|e| e.to_string())
                })
                .unwrap_or(false);
                self.mesh_outbound_polls
                    .entry(peer.name.clone())
                    .or_default()
                    .note_receipts(receipts_pending);
            }
            if !self.collection_peers.contains_key(&peer.name)
                && self
                    .mesh_outbound_polls
                    .entry(peer.name.clone())
                    .or_default()
                    .ready(
                        now,
                        edge.state == "pinned",
                        crate::peer_stream::peer_enrollment_generation(&peer),
                    )
            {
                self.start_collection(
                    peer,
                    Collect::Outbound {
                        outbound: Default::default(),
                    },
                );
            }
        }
    }

    pub(super) fn outbound_reply_peer(&self, node: &str) -> Option<crate::config::PeerConfig> {
        self.state
            .peers
            .iter()
            .find(|peer| {
                with_store(|store| store.get_pin(&peer.name).map_err(|e| e.to_string()))
                    .ok()
                    .flatten()
                    .is_some_and(|pin| pin.node_id == node)
            })
            .cloned()
    }

    pub(super) fn start_collection(&mut self, peer: crate::config::PeerConfig, mut query: Collect) {
        let cap = crate::mesh::collect::POLL_CONCURRENCY;
        if self.collection_relays.slots(cap) == 0 {
            return;
        }
        let node = with_store(|store| store.get_pin(&peer.name).map_err(|e| e.to_string()))
            .ok()
            .flatten()
            .map(|pin| pin.node_id)
            .unwrap_or_default();
        if let Collect::Outbound { outbound } = &mut query {
            outbound.receipts = with_store(|store| {
                store
                    .pending_receipts(&node, now_ms() as i64)
                    .map_err(|e| e.to_string())
            })
            .unwrap_or_default();
        }
        self.collection_peers.insert(peer.name.clone(), node);
        self.collection_relays.start_bounded(
            crate::mesh::collect::work(peer, query),
            self.event_tx.clone(),
            crate::mesh::collect::POLL_CONCURRENCY,
        );
    }

    pub(crate) fn finish_mesh_collection(&mut self, completion: Completion) {
        if self.fleet_pause.paused {
            return;
        }
        let Collect::Answers(query) = completion.query else {
            self.finish_outbound_collection(completion);
            return;
        };
        let answers = match completion.result {
            Ok(answers) => answers,
            Err(reason) => {
                if reason.starts_with("mesh collection refused") {
                    let _ = with_store(|store| {
                        store
                            .collection_failed(&query.request, &reason, false)
                            .map_err(|e| e.to_string())
                    });
                }
                return;
            }
        };
        if with_store(|store| {
            store
                .collection_acked(&query.request, &query.ack)
                .map_err(|e| e.to_string())
        })
        .is_err()
        {
            return;
        }
        if let Some(receipt) = &answers.receipt {
            let imported = with_store(|store| {
                let original = store
                    .get(&query.request)
                    .map_err(|e| e.to_string())?
                    .ok_or("message_not_found")?;
                let peer = store
                    .get_pin(&completion.peer.name)
                    .map_err(|e| e.to_string())?
                    .ok_or("origin_mismatch")?;
                if original.envelope.return_binding.collection_token != query.token
                    || original.envelope.return_binding.recipient_node != peer.node_id
                {
                    return Err("invalid reply binding".into());
                }
                store
                    .import_receipt(&query.request, receipt)
                    .map_err(|e| e.to_string())
            });
            // A receipt is advisory: refusing it must not stall the answers
            // that arrived in the same batch.
            if let Err(reason) = imported {
                crate::logging::mesh_custody_failed(
                    "import_receipt",
                    super::mesh_mail::error_code(&reason),
                );
            }
        }
        for answer in answers.deliveries {
            if let Err(reason) =
                self.import_mesh_answer(&query.request, &answer, Some(&completion.peer.name))
            {
                crate::logging::mesh_custody_failed(
                    "collect",
                    super::mesh_mail::error_code(&reason),
                );
                if !matches!(
                    reason.as_str(),
                    "mailbox_full" | "mail_store_full" | "fleet_paused"
                ) && crate::mesh::runtime_store::recovery_reason().is_none()
                {
                    let permanent = matches!(
                        reason.as_str(),
                        "invalid reply binding" | "inconsistent mesh answer" | "msg_not_allowed"
                    );
                    let _ = with_store(|store| {
                        store
                            .collection_failed(&query.request, &reason, permanent)
                            .map_err(|e| e.to_string())
                    });
                }
            }
        }
        let ack = with_store(|store| {
            store
                .collection_acks(&query.request)
                .map_err(|e| e.to_string())
        })
        .unwrap_or_default();
        if !ack.is_empty() {
            self.start_collection(
                completion.peer,
                Collect::Answers(AnswerCollect { ack, ..query }),
            );
        }
    }

    pub(super) fn import_mesh_answer(
        &mut self,
        request: &MessageKey,
        delivery: &Deliver,
        collecting_peer: Option<&str>,
    ) -> Result<(), String> {
        let origin = self
            .node_id
            .as_deref()
            .ok_or("mesh node identity unavailable")?;
        let answer = &delivery.envelope;
        let sender_host = with_store(|store| {
            store
                .origin_name(&answer.key.origin_node)
                .map_err(|e| e.to_string())
        })?
        .ok_or("origin_mismatch")?;
        if !self.state.config.msg.accepts_from(Some(&sender_host)) {
            return Err("msg_not_allowed".into());
        }
        let original = with_store(|store| {
            if let Some(peer) = collecting_peer {
                if store
                    .get_pin(peer)
                    .map_err(|e| e.to_string())?
                    .is_none_or(|pin| pin.node_id != answer.key.origin_node)
                {
                    return Err("invalid reply binding".into());
                }
            }
            let original = store
                .get(request)
                .map_err(|e| e.to_string())?
                .ok_or(UNAVAILABLE)?;
            if request.origin_node != origin
                || answer.key.origin_node != original.envelope.return_binding.recipient_node
                || answer.request_key.as_ref() != Some(request)
                || answer.target_agent != original.envelope.sender
                || answer.in_reply_to.as_deref() != Some(original.envelope.correlation_id.as_str())
                || answer.return_binding.request != answer.key
                || answer.return_binding.recipient_node != origin
                || answer.return_binding.collection_token
                    != original.envelope.return_binding.collection_token
            {
                return Err("invalid reply binding".into());
            }
            Ok(original)
        })?;
        let mut data = payload(answer)?;
        if data.message.correlation_id != answer.correlation_id
            || data.message.in_reply_to != answer.in_reply_to
            || data.message.from_agent.as_deref().unwrap_or_default() != answer.sender
        {
            return Err("inconsistent mesh answer".into());
        }
        data.message.to_pane = self
            .locate_agent(&original.envelope.sender)
            .filter(|location| location.local)
            .map(|location| location.pane_id)
            .unwrap_or_default();
        data.message.from_pane = None;
        data.message.from_host = Some(sender_host);
        data.message.message_key = Some(answer.key.clone());
        data.message.enqueued_at_ms = now_ms();
        let accepted = with_store(|store| {
            if store.get(&answer.key).map_err(|e| e.to_string())?.is_none()
                && !data.message.to_pane.is_empty()
                && self.mailboxes.queued_len(&data.message.to_pane)
                    >= crate::app::mailboxes::MAX_QUEUED_PER_PANE
            {
                return Err("mailbox_full".into());
            }
            if collecting_peer.is_some() {
                store.accept_collected(answer, delivery.remaining_ms, now_ms() as i64)
            } else {
                store.accept(
                    answer,
                    delivery.remaining_ms,
                    Admission::Inbox,
                    now_ms() as i64,
                )
            }
            .map_err(|e| e.to_string())
        })?;
        if accepted == Accepted::New {
            self.emit_mesh_wake(&answer.key.origin_node);
            self.project_mesh_answer(data.message);
        }
        Ok(())
    }
}
