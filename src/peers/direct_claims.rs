use crate::api::schema::RelayedFleetPeer;
use crate::config::PeerConfig;

// Suppression uses only our configured dial set and local enrollment bindings.
// A claim gains no identity proof from being compared with those bindings.
pub(crate) fn filter_direct_claims(
    peers: &[PeerConfig],
    mut entries: Vec<RelayedFleetPeer>,
    via: &str,
) -> Vec<RelayedFleetPeer> {
    let direct: Vec<_> = peers
        .iter()
        .map(|peer| {
            (
                peer.ssh_target(),
                crate::peer_stream::enrollment(peer).node_id,
            )
        })
        .collect();
    entries.retain(|entry| {
        let dialed =
            direct.iter().any(
                |(target, node)| match (entry.node_id.as_deref(), node.as_deref()) {
                    (Some(claimed), Some(bound)) => claimed == bound,
                    _ => entry.ssh_target == *target,
                },
            );
        if dialed {
            tracing::debug!(
                subject = entry.node_id.as_deref().unwrap_or(&entry.ssh_target),
                relaying_peer = via,
                "dropping relayed claim about a configured dial peer"
            );
        }
        !dialed
    });
    entries
}
