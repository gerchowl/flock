use crate::{app::state::AppState, peers::PeerSummaryState};
use std::time::Instant;

impl AppState {
    // Keep the cached claim: a direct failure must reveal it without another
    // relay delivery. Only a fresh, locally verified direct reading hides it.
    pub(crate) fn relay_claim_is_redundant_at(
        &self,
        claim: &PeerSummaryState,
        now: Instant,
    ) -> bool {
        self.peers.iter().any(|configured| {
            let Some(bound) = self.server_nodes.get(&configured.name) else {
                return false;
            };
            let same_subject = match claim.node_id.as_deref() {
                Some(node) => node == bound,
                None => claim.ssh_target == configured.ssh_target(),
            };
            same_subject
                && self.peer_summaries.iter().any(|direct| {
                    direct.peer == configured.name
                        && direct.node_id.as_deref() == Some(bound.as_str())
                        && direct.last_ok.is_some()
                        && direct.protocol == Some(crate::protocol::PROTOCOL_VERSION)
                        && direct.error.is_none()
                        && direct.stream_error.is_none()
                        && !direct.is_stale_at(now, self.config.gossip.stale_after().as_secs())
                })
        })
    }

    // Log on delivery rather than from the pure display projection.
    pub(crate) fn trace_redundant_relay_claims(&self, via: &str, now: Instant) {
        for entry in self.relayed_fleet_cache.values() {
            if entry.via.as_deref() == Some(via)
                && self.relay_claim_is_redundant_at(&entry.peer, now)
            {
                tracing::debug!(
                    subject = entry
                        .peer
                        .node_id
                        .as_deref()
                        .unwrap_or(&entry.peer.ssh_target),
                    relaying_peer = via,
                    "suppressing relayed claim while verified direct reading is healthy"
                );
            }
        }
    }
}
