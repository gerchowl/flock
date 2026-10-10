//! Outbound collection imports custody through the authenticated supplying edge.
use crate::{
    app::App,
    mesh::{
        collect::{Collect, Completion, OutboundAck, OutboundCollect},
        hello::with_store,
    },
};

impl App {
    pub(super) fn finish_outbound_collection(&mut self, completion: Completion) {
        let mut failed = completion.result.is_err();
        let mut unacked_receipts = false;
        if let Collect::Outbound { outbound } = &completion.query {
            if let Ok(batch) = &completion.result {
                unacked_receipts = outbound
                    .receipts
                    .iter()
                    .any(|receipt| !batch.receipts_acked.contains(receipt));
                if let Err(reason) = with_store(|store| {
                    store
                        .receipts_sent(
                            &batch
                                .receipts_acked
                                .iter()
                                .filter(|r| outbound.receipts.contains(r))
                                .cloned()
                                .collect::<Vec<_>>(),
                        )
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
            .finished_receipts(std::time::Instant::now(), failed, unacked_receipts);
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
            let Some(upstream) = edge.node_id.as_deref() else {
                continue;
            };
            let result = self.import_attested_mesh_mail(&delivery, upstream);
            let delivered = result.as_ref().is_ok_and(|(_, delivered)| *delivered);
            let refusal = match result {
                Ok(_) => None,
                Err(reason) => {
                    if !crate::mesh::delivery::permanent_refusal(&reason)
                        && !matches!(
                            reason.split(':').next(),
                            Some("loop_detected" | "hop_budget_exhausted")
                        )
                    {
                        continue;
                    }
                    Some(reason.chars().take(512).collect())
                }
            };
            ack.push(OutboundAck {
                delivered,
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
