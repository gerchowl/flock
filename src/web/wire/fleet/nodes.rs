//! Owner-only node projection (ADR-0043 NODE STATUS, M1/M2 and N1b1).
use crate::{
    app::App,
    peers::{PeerSummaryState, SshFailureReason},
    structured::fleet_lite::{wire::FleetNodeError, FleetNodeRow, Row},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// An observation's identity, independent of polling errors and GUI traffic.
type Stamp = (Option<Instant>, Option<u64>, Option<Instant>);
/// Home-clock display time sampled once per successful owner observation.
#[derive(Default)]
pub(in crate::web::wire) struct Nodes(BTreeMap<String, (Stamp, Option<u64>)>);
/// Complete owner cut; absent remote nodes are not an unenrollment assertion.
#[derive(Clone, PartialEq, Eq)]
pub(in crate::web::wire) struct Projection {
    /// Rows from pinned direct owners or owner senders proven at relay ingestion.
    pub nodes: Vec<FleetNodeRow>,
    observed: BTreeSet<String>,
    relayed: BTreeSet<String>,
}
impl Projection {
    /// Retain offline owner facts; an unavailable home sample stays unknown.
    pub(super) fn reconcile(
        &self,
        mut rows: Vec<Row>,
        previous: Vec<Row>,
        home: Option<&str>,
    ) -> Vec<Row> {
        for old in previous {
            let Row::Node(mut old) = old else { continue };
            if let Some(Row::Node(new)) = rows
                .iter_mut()
                .find(|r| matches!(r, Row::Node(n) if n.node_id == old.node_id))
            {
                let remote = Some(new.node_id.as_str()) != home;
                if new.health.is_none() && remote {
                    new.health = old.health;
                }
                if new.version.is_none() {
                    new.version = old.version;
                }
                if new.last_ok.is_none() && remote {
                    new.last_ok = old.last_ok;
                }
                if new.counts.is_none() {
                    new.counts = old.counts;
                }
                if new.latency_ms.is_none() && !self.relayed.contains(&new.node_id) && remote {
                    new.latency_ms = old.latency_ms;
                }
                if !self.observed.contains(&new.node_id)
                    && new.error.as_ref().is_some_and(|e| e.code == "pending")
                {
                    if let Some(error) = old.error {
                        if error.code != "route" || new.route.is_none() {
                            new.error = Some(error);
                        }
                    }
                }
            } else {
                old.online = false;
                old.route = None;
                old.error = old.error.or_else(|| Some(Failure::Route.wire()));
                rows.push(Row::Node(old));
            }
        }
        for row in &mut rows {
            if let Row::Node(row) = row {
                row.health = row.health.take().map(read_health);
            }
        }
        rows
    }
}
/// Classified, bounded failures; never serialize raw poll or enrollment text.
enum Failure {
    Pending,
    Stale,
    Route,
    Incompatible,
    Ssh(SshFailureReason),
}
impl Failure {
    fn wire(self) -> FleetNodeError {
        let (code, message) = match self {
            Self::Pending => ("pending", "Owner summary not yet available"),
            Self::Stale => ("stale", "Owner observation is stale or its age is unknown"),
            Self::Route => ("route", "No live route to owner"),
            Self::Incompatible => (
                "peer_incompatible",
                "Owner protocol is unavailable or incompatible; update the peer",
            ),
            Self::Ssh(reason) => (reason.as_str(), reason.describe()),
        };
        FleetNodeError {
            code: code.into(),
            message: message.into(),
        }
    }
}
/// Exact origin age plus residence; missing residence provenance is unknown.
pub(super) fn age(peer: &PeerSummaryState, now: Instant) -> Option<Duration> {
    match peer.origin_last_ok_secs {
        Some(age) => {
            Duration::from_secs(age).checked_add(now.checked_duration_since(peer.ingested_at?)?)
        }
        None => now.checked_duration_since(peer.last_ok?),
    }
}
impl Nodes {
    /// Test-only visibility into owner-observation cache retention.
    #[cfg(test)]
    pub(in crate::web::wire) fn stamp_count(&self) -> usize {
        self.0.len()
    }
    pub(in crate::web::wire) fn observe(&mut self, app: &App, now: Instant) -> Projection {
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|d| u64::try_from(d.as_millis()).ok());
        let routes: BTreeMap<_, _> = app
            .mesh_routes
            .table
            .routes()
            .into_iter()
            .map(|r| (r.node.clone(), r))
            .collect();
        let pins: BTreeMap<_, _> = app
            .state
            .peers
            .iter()
            .filter_map(|peer| {
                Some((
                    peer.name.as_str(),
                    crate::peer_stream::enrollment(peer).node_id?,
                ))
            })
            .collect();
        let ids: BTreeSet<_> = routes
            .keys()
            .cloned()
            .chain(pins.values().cloned())
            .collect();
        let snapshot = app.state.fleet_snapshot.as_ref();
        let peers: Vec<_> = app
            .state
            .peer_summaries
            .iter()
            .map(|s| (s, true))
            .chain(
                app.state
                    .relayed_fleet_cache
                    .values()
                    .map(|e| (&e.peer, false)),
            )
            .chain(
                snapshot
                    .into_iter()
                    .flat_map(|s| s.peers.iter().chain(s.origin_summary.iter()))
                    .map(|s| (s, false)),
            )
            .filter(|(peer, direct)| {
                peer.node_id.as_ref().is_some_and(|id| {
                    if *direct {
                        pins.get(peer.peer.as_str()) == Some(id)
                    } else {
                        app.state.server_peer_node(peer) == Some(id.as_str())
                    }
                })
            })
            .collect();
        self.0.retain(|id, _| {
            ids.contains(id)
                && app.node_id.as_ref() != Some(id)
                && peers
                    .iter()
                    .any(|(peer, _)| peer.node_id.as_ref() == Some(id))
        });
        let mut projection = Projection {
            nodes: Vec::new(),
            observed: BTreeSet::new(),
            relayed: BTreeSet::new(),
        };
        if let Some(home) = &app.node_id {
            let mut row = crate::structured::fleet_lite::local_node(home);
            row.health = app.fleet_home_health();
            projection.observed.insert(home.clone());
            projection.nodes.push(row);
        }
        for id in ids {
            if app.node_id.as_ref() == Some(&id) {
                continue;
            }
            let summary = peers
                .iter()
                .copied()
                .filter(|(s, _)| s.node_id.as_deref() == Some(&id))
                .min_by_key(|(s, direct)| {
                    (
                        age(s, now).unwrap_or(Duration::MAX),
                        !*direct,
                        s.peer.as_str(),
                    )
                });
            let mut row = crate::structured::fleet_lite::local_node(&id);
            row.version = None;
            row.route = routes.get(&id).cloned().map(|mut route| {
                route.name = row.name.clone();
                Box::new(route)
            });
            let failure = match summary {
                None => Some(Failure::Pending),
                Some((peer, direct)) => {
                    if !direct {
                        projection.relayed.insert(id.clone());
                    }
                    projection.observed.insert(id.clone());
                    let observation_age = age(peer, now);
                    let stamp = (peer.last_ok, peer.origin_last_ok_secs, peer.ingested_at);
                    if observation_age.is_some() {
                        if self.0.get(&id).is_none_or(|cached| cached.0 != stamp) {
                            let captured = wall.and_then(|w| {
                                w.checked_sub(u64::try_from(observation_age?.as_millis()).ok()?)
                            });
                            self.0.insert(id.clone(), (stamp, captured));
                        }
                        row.last_ok = self.0.get(&id).and_then(|cached| cached.1);
                    }
                    row.health = peer.system.clone();
                    row.version = peer.version.clone();
                    if direct && peer.proxy_jump.is_none() && peer.origin_last_ok_secs.is_none() {
                        row.latency_ms = peer.latency_ms;
                    }
                    if peer
                        .protocol
                        .is_some_and(|p| p != crate::protocol::PROTOCOL_VERSION)
                    {
                        Some(Failure::Incompatible)
                    } else if let Some(error) = &peer.error {
                        Some(Failure::Ssh(SshFailureReason::classify(error)))
                    } else if peer.protocol.is_none()
                        && (peer.last_ok.is_some() || peer.origin_last_ok_secs.is_some())
                    {
                        Some(Failure::Incompatible)
                    } else if row.route.is_none() {
                        Some(Failure::Route)
                    } else if peer.last_ok.is_none() && peer.origin_last_ok_secs.is_none() {
                        Some(Failure::Pending)
                    } else if observation_age
                        .is_none_or(|a| a > app.state.config.gossip.stale_after())
                    {
                        Some(Failure::Stale)
                    } else {
                        None
                    }
                }
            };
            row.online = failure.is_none();
            row.error = failure.map(Failure::wire);
            projection.nodes.push(row);
        }
        projection
    }
}

/// Thermal detail is host-authored free text. Only its typed severity/component
/// are read-scoped facts; clearing the label prevents hostname disclosure.
fn read_health(
    mut health: crate::api::schema::PeerSystemSummary,
) -> crate::api::schema::PeerSystemSummary {
    if let Some(thermal) = &mut health.thermal {
        thermal.label.clear();
    }
    health
}
