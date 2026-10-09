//! Answers are accepted against the stored request, independent of discovery.
use super::{
    mesh_mail::{envelope, payload, Payload},
    messages::{now_ms, queued_event},
    responses::{encode_error, encode_success},
};
use crate::{
    api::schema::{EventEnvelope, EventKind, ResponseResult},
    app::{mailboxes::PendingMessage, App},
    mesh::{
        collect::{Collect, Completion},
        delivery::Deliver,
        hello::with_store,
        key::MessageKey,
        store::{Accepted, Admission, Envelope, CUSTODY_TTL_MS},
    },
};

pub(super) const UNAVAILABLE: &str = "origin not mesh-reachable (needs 1-H2)";

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
        answer.return_binding.recipient_node = request.origin_node.clone();
        answer.return_binding.collection_peers = vec![request.origin_node.clone()];
        with_store(|store| {
            store
                .accept(
                    &answer,
                    CUSTODY_TTL_MS,
                    if local {
                        Admission::Inbox
                    } else {
                        Admission::Custody
                    },
                    now_ms() as i64,
                )
                .map_err(|e| e.to_string())?;
            Ok(())
        })?;
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
            if !self.uplink.is_relay(
                self.current_api_peer_pid,
                crate::platform::process_start_time,
            ) || self.uplink.enrolled_hub().is_none()
            {
                return Err("mesh collection requires an authenticated held edge".into());
            }
            let origin = self
                .mesh_inbound
                .as_ref()
                .filter(|edge| edge.state == "pinned")
                .and_then(|edge| edge.node_id.as_deref())
                .ok_or("mesh edge is not enrolled")?;
            with_store(|store| {
                store
                    .collect_answers(origin, &query, now_ms() as i64)
                    .map_err(|e| e.to_string())
            })
        })();
        match result {
            Ok(answers) => encode_success(id, ResponseResult::MeshCollected { answers }),
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
        if self.fleet_pause.paused {
            return;
        }
        let cap = self.state.config.msg.deferral_relay_concurrency.clamp(1, 4);
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
                Collect {
                    request: record.envelope.key,
                    token: binding.collection_token,
                    ack,
                },
            );
        }
    }

    fn outbound_reply_peer(&self, node: &str) -> Option<crate::config::PeerConfig> {
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

    fn start_collection(&mut self, peer: crate::config::PeerConfig, query: Collect) {
        let cap = self.state.config.msg.deferral_relay_concurrency.clamp(1, 4);
        if self.collection_relays.slots(cap) == 0 {
            return;
        }
        let node = with_store(|store| store.get_pin(&peer.name).map_err(|e| e.to_string()))
            .ok()
            .flatten()
            .map(|pin| pin.node_id)
            .unwrap_or_default();
        self.collection_peers.insert(peer.name.clone(), node);
        self.collection_relays.start_bounded(
            crate::mesh::collect::work(peer, query),
            self.event_tx.clone(),
            self.state.config.msg.deferral_relay_concurrency.clamp(1, 4),
        );
    }

    pub(crate) fn finish_mesh_collection(&mut self, completion: Completion) {
        if self.fleet_pause.paused {
            return;
        }
        let answers = match completion.result {
            Ok(answers) => answers,
            Err(reason) => {
                if reason.starts_with("mesh collection refused") {
                    let _ = with_store(|store| {
                        store
                            .collection_failed(&completion.query.request, &reason, false)
                            .map_err(|e| e.to_string())
                    });
                }
                return;
            }
        };
        if with_store(|store| {
            store
                .collection_acked(&completion.query.request, &completion.query.ack)
                .map_err(|e| e.to_string())
        })
        .is_err()
        {
            return;
        }
        for answer in answers {
            if let Err(reason) = self.import_mesh_answer(
                &completion.query.request,
                &answer,
                Some(&completion.peer.name),
            ) {
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
                            .collection_failed(&completion.query.request, &reason, permanent)
                            .map_err(|e| e.to_string())
                    });
                }
            }
        }
        let ack = with_store(|store| {
            store
                .collection_acks(&completion.query.request)
                .map_err(|e| e.to_string())
        })
        .unwrap_or_default();
        if !ack.is_empty() {
            self.start_collection(
                completion.peer,
                Collect {
                    ack,
                    ..completion.query
                },
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
            self.project_mesh_answer(data.message);
        }
        Ok(())
    }
}
