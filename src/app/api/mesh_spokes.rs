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
        if params.from_host.is_some() || !self.state.peers.is_empty() {
            return None;
        }
        let hub = self.uplink.enrolled_hub()?;
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
        let Ok(deliveries) = completion.result else {
            return;
        };
        let edge = crate::peer_stream::enrollment(&completion.peer);
        if edge.state != "pinned" {
            return;
        }
        let mut ack = Vec::new();
        for delivery in deliveries {
            let mail = &delivery.envelope;
            if edge.node_id.as_deref() != Some(mail.key.origin_node.as_str())
                || self.node_id.as_deref() != Some(mail.return_binding.recipient_node.as_str())
                || mail.request_key.is_some()
            {
                continue;
            }
            let refusal = match self.import_attested_mesh_mail(&delivery) {
                Ok((_, true)) => None,
                Ok((_, false)) => continue,
                Err(reason) if matches!(reason.as_str(), "forward_limit" | "msg_not_allowed") => {
                    Some(reason)
                }
                Err(reason) => {
                    crate::logging::mesh_custody_failed(
                        "collect_outbound",
                        super::mesh_mail::error_code(&reason),
                    );
                    continue;
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
                    outbound: OutboundCollect { ack },
                },
            );
        }
    }
}
