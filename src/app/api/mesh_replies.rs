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
        let origin = self.node_id.clone().ok_or(UNAVAILABLE)?;
        let original = with_store(|store| store.get(request).map_err(|e| e.to_string()))?
            .ok_or(UNAVAILABLE)?;
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
        let data = Payload {
            message: message.clone(),
            peer: None,
            host: None,
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
        if self.fleet_pause.paused || !self.message_relays.is_idle() {
            return;
        }
        let records = with_store(|store| {
            store
                .collect_ready(
                    &origin,
                    now_ms() as i64,
                    self.state.config.msg.deferral_relay_concurrency.max(1),
                )
                .map_err(|e| e.to_string())
        });
        let Ok(records) = records else { return };
        for record in records {
            let binding = record.envelope.return_binding;
            let peer = self
                .state
                .peers
                .iter()
                .find(|peer| {
                    with_store(|store| store.get_pin(&peer.name).map_err(|e| e.to_string()))
                        .ok()
                        .flatten()
                        .is_some_and(|pin| pin.node_id == binding.recipient_node)
                })
                .cloned();
            let Some(peer) = peer else { continue };
            self.enqueue_message_relay(crate::mesh::collect::work(
                peer,
                Collect {
                    request: record.envelope.key,
                    token: binding.collection_token,
                    ack: Vec::new(),
                },
            ));
        }
    }

    pub(crate) fn finish_mesh_collection(&mut self, completion: Completion) {
        if self.fleet_pause.paused {
            return;
        }
        let Ok(answers) = completion.result else {
            return;
        };
        let mut ack = Vec::new();
        for answer in answers {
            match self.import_mesh_answer(&completion.peer.name, &completion.query.request, &answer)
            {
                Ok(()) => ack.push(answer.envelope.key),
                Err(reason) => crate::logging::mesh_custody_failed(
                    "collect",
                    super::mesh_mail::error_code(&reason),
                ),
            }
        }
        if !ack.is_empty() {
            self.enqueue_message_relay(crate::mesh::collect::work(
                completion.peer,
                Collect {
                    ack,
                    ..completion.query
                },
            ));
        }
    }

    fn import_mesh_answer(
        &mut self,
        peer: &str,
        request: &MessageKey,
        delivery: &Deliver,
    ) -> Result<(), String> {
        let origin = self.node_id.as_deref().ok_or(UNAVAILABLE)?;
        let answer = &delivery.envelope;
        if !self.state.config.msg.accepts_from(Some(peer)) {
            return Err("msg_not_allowed".into());
        }
        let original = with_store(|store| {
            let original = store
                .get(request)
                .map_err(|e| e.to_string())?
                .ok_or(UNAVAILABLE)?;
            let pin = store
                .get_pin(peer)
                .map_err(|e| e.to_string())?
                .ok_or(UNAVAILABLE)?;
            if request.origin_node != origin
                || pin.node_id != original.envelope.return_binding.recipient_node
                || answer.key.origin_node != pin.node_id
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
        data.message.from_host = Some(peer.into());
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
            store
                .accept(
                    answer,
                    delivery.remaining_ms,
                    Admission::Inbox,
                    now_ms() as i64,
                )
                .map_err(|e| e.to_string())
        })?;
        if accepted == Accepted::New {
            self.project_mesh_answer(data.message);
        }
        Ok(())
    }
}
