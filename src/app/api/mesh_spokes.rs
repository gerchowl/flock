//! Spoke outbox custody collected over the hub's authenticated dialed edge.
use super::{
    mesh_mail::{envelope, Payload},
    messages::now_ms,
    responses::{encode_error, encode_success},
};
use crate::{
    api::schema::{MsgSendParams, ResponseResult},
    app::{mailboxes::PendingMessage, App},
    mesh::{
        collect::{Collect, Completion, OutboundAck, OutboundCollect},
        hello::with_store,
        store::{Admission, CUSTODY_TTL_MS},
    },
};

impl App {
    pub(super) fn try_queue_spoke(
        &mut self,
        id: &str,
        target: &str,
        body: &str,
        params: &MsgSendParams,
    ) -> Option<String> {
        if !self.state.peers.is_empty() {
            return None;
        }
        let hub = self
            .inbound
            .live(crate::platform::process_start_time)
            .filter(|edge| edge.enrolled())
            .min_by_key(|edge| &edge.enrollment.node_id)
            .map(|edge| edge.enrollment.peer.clone())
            .or_else(|| {
                with_store(|store| store.sole_inbound_peer().map_err(|e| e.to_string()))
                    .ok()
                    .flatten()
            })?;
        let result = (|| {
            if self.fleet_pause.paused {
                return Err("fleet_paused".to_string());
            }
            let sender = self.attested_sender_agent().ok_or("sender_unresolved")?;
            let origin = self
                .node_id
                .as_deref()
                .ok_or("mesh node identity unavailable")?;
            let pin = with_store(|store| {
                store
                    .get_pin_from(crate::mesh::store::PinSource::Inbound, &hub)
                    .map_err(|e| e.to_string())
            })?
            .ok_or("mesh edge is not enrolled")?;
            let correlation = params
                .correlation_id
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(super::messages::mint_correlation_id);
            let data = Payload {
                message: PendingMessage {
                    message_key: None,
                    correlation_id: correlation.clone(),
                    body: body.into(),
                    from_pane: None,
                    from_agent: Some(sender),
                    from_host: Some(crate::app::short_host_name()),
                    from_repo: None,
                    to_pane: String::new(),
                    to_repo: None,
                    in_reply_to: params.in_reply_to.clone(),
                    enqueued_at_ms: now_ms(),
                    delivery_attempts: 0,
                    intent: params.intent,
                },
                peer: None,
                host: Some(hub.clone()),
                direct: true,
            };
            let mut mail = envelope(origin, target.into(), &data)?;
            mail.return_binding.recipient_node = pin.node_id.clone();
            mail.return_binding.collection_peers = vec![pin.node_id];
            with_store(|store| {
                store
                    .accept(&mail, CUSTODY_TTL_MS, Admission::Held, now_ms() as i64)
                    .map_err(|e| e.to_string())
            })?;
            self.emit_mesh_wake(&mail.return_binding.recipient_node);
            Ok((mail.key, correlation))
        })();
        Some(match result {
            Ok((key, correlation_id)) => encode_success(
                id.into(),
                ResponseResult::MsgQueued {
                    message_key: Some(key),
                    correlation_id,
                    state: "queued".into(),
                    warnings: Vec::new(),
                    to_host: Some(hub.clone()),
                    path: Some(format!("via {hub}")),
                },
            ),
            Err(reason) => encode_error(
                id.into(),
                if reason == "sender_unresolved" {
                    "sender_unresolved"
                } else {
                    super::mesh_mail::error_code(&reason)
                },
                reason,
            ),
        })
    }

    pub(super) fn finish_outbound_collection(&mut self, completion: Completion) {
        let mut failed = completion.result.is_err();
        if let Collect::Outbound { outbound } = &completion.query {
            if completion.result.is_ok() && !outbound.receipts.is_empty() {
                if let Err(reason) = with_store(|store| {
                    store
                        .receipts_sent(&outbound.receipts)
                        .map_err(|e| e.to_string())
                }) {
                    failed = true;
                    crate::logging::mesh_custody_failed(
                        "receipts_sent",
                        super::mesh_mail::error_code(&reason),
                    );
                }
            }
        }
        self.mesh_outbound_polls
            .entry(completion.peer.name.clone())
            .or_default()
            .finished(std::time::Instant::now(), failed);
        let Ok(batch) = completion.result else {
            return;
        };
        let edge = crate::peer_stream::enrollment(&completion.peer);
        if edge.state != "pinned" {
            return;
        }
        let mut ack: Vec<_> = batch
            .quarantined
            .into_iter()
            .filter(|ack| edge.node_id.as_deref() == Some(ack.key.origin_node.as_str()))
            .collect();
        for delivery in batch.deliveries {
            let mail = &delivery.envelope;
            if edge.node_id.as_deref() != Some(mail.key.origin_node.as_str()) {
                continue;
            }
            let result =
                if self.node_id.as_deref() != Some(mail.return_binding.recipient_node.as_str()) {
                    Err("invalid_envelope".into())
                } else if mail.kind == crate::mesh::store::Kind::Receipt {
                    self.import_mesh_receipt(mail)
                } else if super::mesh_mail::payload(mail).is_err() {
                    Err("invalid_envelope".into())
                } else if let Some(request) = &mail.request_key {
                    // The edge origin was checked above. This lane acknowledges
                    // via OutboundAck, without creating answer-collection debt.
                    self.import_mesh_answer(request, &delivery, None)
                        .map(|()| (crate::mesh::store::Accepted::New, true))
                } else {
                    self.import_attested_mesh_mail(&delivery)
                };
            let refusal = match result {
                Ok(_) => None,
                Err(reason) => {
                    if !permanent_import_error(&reason) {
                        continue;
                    }
                    Some(reason.chars().take(512).collect())
                }
            };
            ack.push(OutboundAck {
                key: mail.key.clone(),
                token: mail.return_binding.collection_token.clone(),
                refusal,
            });
        }
        if !ack.is_empty() {
            self.start_collection(
                completion.peer,
                Collect::Outbound {
                    outbound: OutboundCollect {
                        ack,
                        receipts: Vec::new(),
                    },
                },
            );
        }
    }
}

fn permanent_import_error(reason: &str) -> bool {
    matches!(
        reason.split(':').next().unwrap_or(reason),
        "invalid_envelope"
            | "invalid reply binding"
            | "inconsistent mesh answer"
            | "inconsistent mesh envelope"
            | "msg_not_allowed"
            | "origin_mismatch"
            | "message_key_conflict"
            | "message_expired"
            | "expired"
            | super::mesh_replies::UNAVAILABLE
            | "recipient_gone"
            | "forward_limit"
            | "msg_target_not_found"
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn unknown_import_errors_remain_retryable() {
        for error in [
            "receipt_original_not_ready",
            "database is locked",
            "disk I/O error",
            "mailbox_full",
            "mail_store_full",
            "fleet_paused",
        ] {
            assert!(!super::permanent_import_error(error));
        }
        assert!(super::permanent_import_error("invalid_envelope"));
        assert!(super::permanent_import_error("msg_target_not_found: gone"));
    }
}
