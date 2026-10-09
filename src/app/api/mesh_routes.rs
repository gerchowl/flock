//! Topology exchange on a separate bounded pool, independent of mail custody.
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use super::responses::{encode_error, encode_success};
use crate::api::schema::ResponseResult;
use crate::app::{message_relay::RelayWork, App};
use crate::events::AppEvent;
use crate::mesh::routes::{Adjacency, Advert, Route, RouteTable};

#[derive(Default)]
pub(crate) struct Routes {
    pub table: RouteTable,
    workers: crate::app::message_relay::MessageRelays,
    live: BTreeMap<String, String>,
    sent: BTreeMap<String, (u64, u64)>,
    busy: BTreeSet<String>,
    retries: BTreeMap<String, Retry>,
    changed_at: Option<Instant>,
    generation: u64,
    boot_ms: Option<u64>,
    seq: u32,
    cursor: usize,
    tick_at: Option<Instant>,
}

struct Retry {
    enrollment: u64,
    generation: u64,
    attempts: u32,
    at: Instant,
}

impl Retry {
    fn superseded(&self, enrollment: u64, generation: u64, awakened: bool) -> bool {
        awakened || self.enrollment != enrollment || self.generation != generation
    }

    fn failed(previous: Option<&Self>, enrollment: u64, generation: u64, now: Instant) -> Self {
        let attempts = previous
            .filter(|r| r.enrollment == enrollment && r.generation == generation)
            .map_or(0, |r| r.attempts.saturating_add(1));
        let base = match attempts {
            0 => 5,
            1 => 60,
            _ => 300,
        };
        let mut bytes = [0; 8];
        let jitter = if getrandom::fill(&mut bytes).is_ok() {
            u64::from_ne_bytes(bytes) % (base * 200 + 1)
        } else {
            0
        };
        // Jitter stays within the five-minute cap and avoids retry bursts.
        let delay = Duration::from_millis(base * 800 + jitter);
        Self {
            enrollment,
            generation,
            attempts,
            at: now + delay,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Completion {
    peer: crate::config::PeerConfig,
    node: String,
    enrollment: u64,
    generation: u64,
    result: Result<Vec<Advert>, String>,
}

impl App {
    pub(super) fn refresh_mesh_routes(&mut self) {
        for peer in crate::peer_stream::take_closed_edges() {
            let nodes: Vec<_> = self
                .mesh_routes
                .live
                .iter()
                .filter(|(_, name)| **name == peer)
                .map(|(node, _)| node.clone())
                .collect();
            for node in nodes {
                self.mesh_routes.table.withdraw_edge(&node);
            }
            self.mesh_routes.sent.remove(&peer);
        }
        for node in self.inbound.prune(crate::platform::process_start_time) {
            self.mesh_routes.table.withdraw_edge(&node);
        }
        let mut live = BTreeMap::new();
        for edge in self
            .inbound
            .live(crate::platform::process_start_time)
            .filter(|e| e.enrolled())
        {
            if let Some(node) = &edge.enrollment.node_id {
                live.insert(node.clone(), edge.enrollment.peer.clone());
            }
        }
        for peer in &self.state.peers {
            let status = crate::peer_stream::enrollment(peer);
            if status.state == "pinned" {
                if let Some(node) = status.node_id {
                    live.insert(node, peer.name.clone());
                }
            }
        }
        for node in self.mesh_routes.live.keys() {
            if !live.contains_key(node) {
                self.mesh_routes.table.withdraw_edge(node);
            }
        }
        self.mesh_routes
            .table
            .set_live_edges(live.keys().cloned().collect());
        if self.mesh_routes.table.own().is_some() && live == self.mesh_routes.live {
            return;
        }
        if self.node_id.is_none() {
            return;
        }
        let result = (|| {
            let boot = match self.mesh_routes.boot_ms {
                Some(boot) => boot,
                None => crate::mesh::hello::with_store(|store| {
                    store
                        .reserve_route_boot(super::messages::now_ms() as i64)
                        .map_err(|e| e.to_string())
                })?,
            };
            let identity =
                crate::mesh::identity::NodeIdentity::load().map_err(|e| e.to_string())?;
            let seq = self
                .mesh_routes
                .seq
                .checked_add(1)
                .ok_or("mesh advert sequence exhausted")?;
            let adjacencies = live
                .iter()
                .take(crate::mesh::routes::MAX_ADJACENCIES)
                .map(|(node, name)| Adjacency {
                    node_id: node.clone(),
                    name: name.clone(),
                })
                .collect();
            let advert = Advert::signed(
                &identity,
                crate::app::short_host_name(),
                (boot, seq),
                adjacencies,
            );
            advert.verify().map_err(str::to_owned)?;
            self.mesh_routes.table.set_own(advert);
            self.mesh_routes.boot_ms = Some(boot);
            self.mesh_routes.seq = seq;
            self.mesh_routes.live = live;
            Ok::<_, String>(())
        })();
        if let Err(reason) = result {
            crate::logging::mesh_routing_failed("build_advert", "", &reason);
        }
    }

    pub(super) fn handle_mesh_routes(
        &mut self,
        id: String,
        adverts: Vec<serde_json::Value>,
    ) -> String {
        if let Some(refusal) = self.refuse_unless_edge(&id, "mesh.routes") {
            return refusal;
        }
        let supplier = self
            .inbound
            .edge(
                self.current_api_peer_pid,
                crate::platform::process_start_time,
            )
            .filter(|e| e.enrolled())
            .and_then(|e| e.enrollment.node_id.clone());
        let Some(supplier) = supplier else {
            return encode_error(
                id,
                "mesh_not_enrolled",
                "mesh.routes requires an enrolled edge",
            );
        };
        if self.fleet_pause.paused {
            return encode_error(
                id,
                "fleet_paused",
                "route exchange paused; retry after resume",
            );
        }
        self.refresh_mesh_routes();
        self.mesh_routes
            .table
            .learn(&supplier, crate::mesh::routes::decode_adverts(adverts));
        encode_success(
            id,
            ResponseResult::MeshRoutes {
                adverts: self.mesh_routes.table.adverts_for_peer(&supplier),
            },
        )
    }

    pub(crate) fn tick_mesh_routes(&mut self) {
        let now = Instant::now();
        if self.mesh_routes.tick_at.is_some_and(|at| now < at) {
            return;
        }
        self.mesh_routes.tick_at = Some(now + Duration::from_secs(1));
        self.refresh_mesh_routes();
        if self.fleet_pause.paused {
            return;
        }
        let now = Instant::now();
        let generation = self.mesh_routes.table.generation();
        if self.mesh_routes.generation != generation {
            self.mesh_routes.generation = generation;
            self.mesh_routes.changed_at = Some(now);
            self.emit_event(crate::api::schema::EventEnvelope {
                event: crate::api::schema::EventKind::MeshRoutesChanged,
                data: crate::api::schema::EventData::MeshRoutesChanged {},
            });
        }
        let debounced = self
            .mesh_routes
            .changed_at
            .is_none_or(|at| now.duration_since(at) >= Duration::from_secs(1));
        let mut peers = self.state.peers.clone();
        if !peers.is_empty() {
            let count = peers.len();
            peers.rotate_left(self.mesh_routes.cursor % count);
            self.mesh_routes.cursor = (self.mesh_routes.cursor + 2) % count;
        }
        for peer in peers {
            if self.mesh_routes.workers.slots(2) == 0 {
                break;
            }
            let status = crate::peer_stream::enrollment(&peer);
            let Some(node) = status.node_id.filter(|_| status.state == "pinned") else {
                continue;
            };
            let enrollment = crate::peer_stream::peer_enrollment_generation(&peer);
            if self.mesh_routes.busy.contains(&peer.name) {
                continue;
            }
            let awakened = crate::peer_stream::take_route_wake(&peer);
            if self
                .mesh_routes
                .retries
                .get(&peer.name)
                .is_some_and(|r| r.superseded(enrollment, generation, awakened))
            {
                self.mesh_routes.retries.remove(&peer.name);
            }
            if self
                .mesh_routes
                .retries
                .get(&peer.name)
                .is_some_and(|r| now < r.at)
            {
                continue;
            }
            let previous = self.mesh_routes.sent.get(&peer.name).copied();
            let reconnected = previous.is_none_or(|(old, _)| old != enrollment);
            let changed = previous.is_none_or(|(_, old)| old != generation);
            if !(reconnected || (changed && debounced) || awakened) {
                continue;
            }
            self.mesh_routes.busy.insert(peer.name.clone());
            let adverts = self.mesh_routes.table.adverts_for_peer(&node);
            let failure = AppEvent::MeshRoutesCompleted(Box::new(Completion {
                peer: peer.clone(),
                node: node.clone(),
                enrollment,
                generation,
                result: Err("route worker panicked".into()),
            }));
            self.mesh_routes.workers.start_bounded(
                RelayWork {
                    failure,
                    run: Box::new(move || {
                        let result = crate::peer_stream::request(
                            &peer,
                            "mesh.routes",
                            serde_json::json!({"adverts": adverts}),
                        )
                        .and_then(|line| {
                            let value: serde_json::Value =
                                serde_json::from_str(&line).map_err(|e| e.to_string())?;
                            value["result"]["adverts"]
                                .as_array()
                                .cloned()
                                .map(crate::mesh::routes::decode_adverts)
                                .ok_or_else(|| "invalid mesh routes response".into())
                        });
                        AppEvent::MeshRoutesCompleted(Box::new(Completion {
                            peer,
                            node,
                            enrollment,
                            generation,
                            result,
                        }))
                    }),
                },
                self.event_tx.clone(),
                2,
            );
        }
    }

    pub(super) fn finish_mesh_routes(&mut self, completion: Completion) {
        self.mesh_routes.workers.complete();
        self.mesh_routes.busy.remove(&completion.peer.name);
        let status = crate::peer_stream::enrollment(&completion.peer);
        if self.fleet_pause.paused {
            self.mesh_routes.sent.remove(&completion.peer.name);
            return;
        }
        if status.state != "pinned"
            || status.node_id.as_deref() != Some(&completion.node)
            || crate::peer_stream::peer_enrollment_generation(&completion.peer)
                != completion.enrollment
            || !self.state.peers.iter().any(|p| p == &completion.peer)
        {
            return;
        }
        match completion.result {
            Ok(adverts) => {
                self.mesh_routes.sent.insert(
                    completion.peer.name.clone(),
                    (completion.enrollment, completion.generation),
                );
                self.mesh_routes.retries.remove(&completion.peer.name);
                self.mesh_routes.table.learn(
                    &completion.node,
                    adverts.into_iter().take(crate::mesh::routes::MAX_RECORDS),
                );
            }
            Err(reason) => {
                self.mesh_routes.sent.remove(&completion.peer.name);
                let retry = Retry::failed(
                    self.mesh_routes.retries.get(&completion.peer.name),
                    completion.enrollment,
                    completion.generation,
                    Instant::now(),
                );
                self.mesh_routes
                    .retries
                    .insert(completion.peer.name.clone(), retry);
                crate::logging::mesh_routing_failed("exchange", &completion.peer.name, &reason)
            }
        }
    }

    pub(super) fn mesh_route_status(&mut self) -> Vec<Route> {
        self.refresh_mesh_routes();
        let mut names = self.mesh_routes.live.clone();
        for peer in &self.state.peers {
            if let Some(node) = crate::peer_stream::enrollment(peer).node_id {
                names.insert(node, peer.name.clone());
            }
        }
        self.mesh_routes
            .table
            .routes()
            .into_iter()
            .map(|mut route| {
                route.name = names
                    .get(&route.node)
                    .cloned()
                    .unwrap_or_else(|| format!("{} {} (advertised)", route.name, &route.node[..8]));
                route
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_or_new_generation_supersedes_even_the_longest_backoff() {
        let now = Instant::now();
        let mut retry = Retry::failed(None, 1, 7, now);
        for _ in 0..2 {
            retry = Retry::failed(Some(&retry), 1, 7, now);
        }
        assert!(retry.at > now + Duration::from_secs(60));
        assert!(!retry.superseded(1, 7, false));
        assert!(retry.superseded(1, 7, true));
        assert!(retry.superseded(1, 8, false));
        assert!(retry.superseded(2, 7, false));
        assert_eq!(Retry::failed(Some(&retry), 1, 8, now).attempts, 0);
    }

    #[test]
    fn retries_back_off_and_reset_on_new_enrollment() {
        let now = Instant::now();
        let mut retry = Retry::failed(None, 1, 7, now);
        for (attempt, seconds) in [5, 60, 300, 300].into_iter().enumerate() {
            if attempt != 0 {
                retry = Retry::failed(Some(&retry), 1, 7, now);
            }
            let delay = retry.at.duration_since(now);
            assert!(delay >= Duration::from_millis(seconds * 800));
            assert!(delay <= Duration::from_secs(seconds));
        }
        let reset = Retry::failed(Some(&retry), 2, 7, now);
        assert_eq!(reset.attempts, 0);
        assert!(reset.at.duration_since(now) <= Duration::from_secs(5));
    }
}
