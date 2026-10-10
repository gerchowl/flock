//! Request routing uses node identities and only authenticated adjacent edges.
use super::messages::now_ms;
use crate::{
    app::App,
    mesh::{
        delivery::Deliver,
        hello::with_store,
        store::{Accepted, Admission},
    },
};

pub(super) struct NextHop {
    pub node: String,
    pub name: String,
    pub peer: Option<crate::config::PeerConfig>,
}

impl NextHop {
    pub fn admission(&self) -> Admission {
        if !self.node.is_empty() && self.peer.is_none() {
            Admission::Held
        } else {
            Admission::Custody
        }
    }
}

impl App {
    pub(super) fn request_next_hop(&mut self, owner: &str) -> NextHop {
        self.refresh_mesh_routes();
        // A live authenticated direct edge already proves reachability to its
        // owner. Directory discovery can precede the reciprocal topology advert.
        let node = self
            .mesh_routes
            .table
            .next_hop(owner)
            .or_else(|| {
                self.outbound_reply_peer(owner).and_then(|peer| {
                    let edge = crate::peer_stream::enrollment(&peer);
                    (edge.state == "pinned" && edge.node_id.as_deref() == Some(owner))
                        .then(|| owner.to_owned())
                })
            })
            .unwrap_or_default();
        let peer = self
            .outbound_reply_peer(&node)
            .filter(|peer| crate::peer_stream::enrollment(peer).state == "pinned");
        let name = peer
            .as_ref()
            .map(|p| p.name.clone())
            .or_else(|| {
                self.inbound
                    .live(crate::platform::process_start_time)
                    .find(|e| e.enrolled() && e.enrollment.node_id.as_deref() == Some(&node))
                    .map(|e| e.enrollment.peer.clone())
            })
            .unwrap_or_else(|| node.clone());
        NextHop { node, name, peer }
    }

    /// The upgrade instruction for a refused peer on this request's route.
    pub(super) fn incompatible_request_peer(&self, owner: &str, next: &str) -> Option<String> {
        self.state.peers.iter().find_map(|peer| {
            let status = crate::peer_stream::enrollment(peer);
            if status.state != "refused" {
                return None;
            }
            let pin = with_store(|store| store.get_pin(&peer.name).map_err(|e| e.to_string()))
                .ok()
                .flatten()?;
            let pre_mesh = status
                .reason
                .as_deref()
                .is_some_and(|reason| reason.ends_with(crate::peer_stream::PRE_MESH));
            (pin.node_id == owner || pin.node_id == next).then(|| {
                if pre_mesh {
                    format!(
                        "upgrade flk on {} {}",
                        peer.name,
                        crate::peer_stream::PRE_MESH
                    )
                } else {
                    format!("upgrade flk on {}", peer.name)
                }
            })
        })
    }

    pub(super) fn check_mesh_import(
        &self,
        delivery: &Deliver,
        upstream: &str,
    ) -> Result<Option<String>, String> {
        if self.fleet_pause.paused {
            return Err("fleet_paused".into());
        }
        crate::mesh::sign::verify(&delivery.envelope).map_err(|_| "invalid_signature")?;
        if delivery.visited.first() != Some(&delivery.envelope.key.origin_node)
            || delivery.visited.last().map(String::as_str) != Some(upstream)
        {
            return Err("origin_mismatch".into());
        }
        if self
            .node_id
            .as_ref()
            .is_some_and(|node| delivery.visited.contains(node))
        {
            return Err("loop_detected".into());
        }
        if delivery.visited.len() > 9 || delivery.hops_left > 8 {
            return Err("invalid_envelope".into());
        }
        let name = with_store(|store| {
            store
                .origin_name(&delivery.envelope.key.origin_node)
                .map_err(|e| e.to_string())
        })?;
        if !self.state.config.msg.accepts_origin(name.as_deref()) {
            return Err("msg_not_allowed".into());
        }
        Ok(name)
    }

    pub(super) fn accept_forwarded_request(
        &mut self,
        delivery: &Deliver,
    ) -> Result<(Accepted, bool), String> {
        if delivery.hops_left == 0 {
            return Err("hop_budget_exhausted".into());
        }
        let next = self.request_next_hop(&delivery.envelope.return_binding.recipient_node);
        let mut visited = delivery.visited.clone();
        visited.push(
            self.node_id
                .clone()
                .ok_or("mesh node identity unavailable")?,
        );
        let accepted = with_store(|store| {
            store
                .accept_forward(
                    &delivery.envelope,
                    delivery.remaining_ms,
                    delivery.hops_left - 1,
                    &visited,
                    &next.node,
                    next.admission(),
                    now_ms() as i64,
                )
                .map_err(|e| e.to_string())
        })?;
        self.mesh_retry_at = None;
        self.emit_mesh_wake(&next.node);
        Ok((accepted, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn app() -> App {
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        app.node_id = Some("receiver.example".into());
        app
    }
    fn delivery() -> Deliver {
        let envelope = crate::mesh::sign::tests::signed();
        Deliver {
            visited: vec![envelope.key.origin_node.clone(), "hub.example".into()],
            envelope,
            remaining_ms: 1000,
            hops_left: 7,
        }
    }
    #[tokio::test]
    async fn forger_hub_altering_origin_or_body_is_refused_invalid_signature() {
        let _store = crate::mesh::runtime_store::TestStore::new();
        let app = app();
        for mutation in [0, 1, 2] {
            let mut delivery = delivery();
            match mutation {
                0 => delivery.envelope.key.origin_node = "forged.example".into(),
                1 => delivery.envelope.body.push(1),
                _ => delivery.envelope.signature.clear(),
            }
            assert_eq!(
                app.check_mesh_import(&delivery, "hub.example"),
                Err("invalid_signature".into())
            );
        }
    }
    #[tokio::test]
    async fn visited_not_bound_to_edge_is_origin_mismatch() {
        let _store = crate::mesh::runtime_store::TestStore::new();
        let app = app();
        let mut delivery = delivery();
        assert_eq!(
            app.check_mesh_import(&delivery, "other.example"),
            Err("origin_mismatch".into())
        );
        delivery.visited[0] = "forged.example".into();
        assert_eq!(
            app.check_mesh_import(&delivery, "hub.example"),
            Err("origin_mismatch".into())
        );
    }
    #[tokio::test]
    async fn narrowed_allow_from_refuses_origin_with_no_configured_name() {
        let _store = crate::mesh::runtime_store::TestStore::new();
        let mut app = app();
        app.state.config.msg.allow_from = vec!["hub.example".into()];
        assert_eq!(
            app.check_mesh_import(&delivery(), "hub.example"),
            Err("msg_not_allowed".into())
        );
        app.state.config.msg.allow_from = vec!["*".into()];
        assert_eq!(app.check_mesh_import(&delivery(), "hub.example"), Ok(None));
    }
    #[tokio::test]
    async fn route_cycle_is_loop_detected_and_custody_retained() {
        let _store = crate::mesh::runtime_store::TestStore::new();
        let app = app();
        let mut delivery = delivery();
        delivery.visited.insert(1, "receiver.example".into());
        assert_eq!(
            app.check_mesh_import(&delivery, "hub.example"),
            Err("loop_detected".into())
        );
        let failure = crate::peers::PeerMessageFailure::Reroute("loop_detected".into());
        assert!(!failure.retryable());
    }
    /// This hub's only route to the recipient runs back through the spoke
    /// the mail came from, the topology of #866 after the laptop's direct
    /// edge to this hub drops.
    fn hub_reachable_only_through_spoke() -> (App, Deliver) {
        let mut app = app();
        app.node_id = Some(
            crate::mesh::identity::NodeIdentity::load()
                .unwrap()
                .node_id(),
        );
        let spoke = crate::mesh::identity::NodeIdentity::fixture([7; 32]);
        let laptop = crate::mesh::identity::NodeIdentity::fixture([9; 32]);
        let peer = crate::config::PeerConfig {
            name: "spoke".into(),
            ssh: "loop-reroute-test-nonexistent-host.invalid".into(),
            ..Default::default()
        };
        crate::peer_stream::test_enroll(&peer, &spoke.node_id());
        app.state.peers = vec![peer];
        app.refresh_mesh_routes();
        let adjacency = |node: &crate::mesh::identity::NodeIdentity, name: &str| {
            crate::mesh::routes::Adjacency {
                node_id: node.node_id(),
                name: name.into(),
            }
        };
        let hub = crate::mesh::routes::Adjacency {
            node_id: app.node_id.clone().unwrap(),
            name: "hub".into(),
        };
        app.mesh_routes.table.learn(
            &spoke.node_id(),
            vec![
                crate::mesh::routes::Advert::signed(
                    &spoke,
                    "spoke".into(),
                    (1, 1),
                    vec![hub, adjacency(&laptop, "laptop")],
                ),
                crate::mesh::routes::Advert::signed(
                    &laptop,
                    "laptop".into(),
                    (1, 1),
                    vec![adjacency(&spoke, "spoke")],
                ),
            ],
        );
        assert_eq!(
            app.mesh_routes.table.next_hop(&laptop.node_id()),
            Some(spoke.node_id())
        );
        let mut envelope = crate::mesh::sign::tests::signed();
        envelope.return_binding.recipient_node = laptop.node_id();
        crate::mesh::sign::seal(&mut envelope, &spoke);
        let delivery = Deliver {
            visited: vec![spoke.node_id()],
            envelope,
            remaining_ms: 60_000,
            hops_left: 7,
        };
        (app, delivery)
    }
    #[tokio::test]
    async fn hub_refuses_mail_whose_only_route_is_back_through_its_sender() {
        let _store = crate::mesh::runtime_store::TestStore::new();
        let (mut app, delivery) = hub_reachable_only_through_spoke();
        // The store refuses before taking custody, so the sender keeps it.
        let refusal = app.accept_forwarded_request(&delivery).unwrap_err();
        assert_eq!(
            refusal.split(':').next(),
            Some("loop_detected"),
            "{refusal}"
        );
        let held = with_store(|store| store.get(&delivery.envelope.key).map_err(|e| e.to_string()))
            .unwrap();
        assert!(held.is_none(), "custody stays with the sender");
    }
    #[tokio::test]
    async fn hub_never_routes_custody_to_a_node_it_already_crossed() {
        let _store = crate::mesh::runtime_store::TestStore::new();
        let (mut app, delivery) = hub_reachable_only_through_spoke();
        let hub = app.node_id.clone().unwrap();
        // Custody taken while this hub had no route at all, then the only
        // route that appears leads back through the spoke.
        with_store(|store| {
            store
                .accept_forward(
                    &delivery.envelope,
                    delivery.remaining_ms,
                    delivery.hops_left - 1,
                    &[delivery.visited[0].clone(), hub],
                    "",
                    Admission::Custody,
                    now_ms() as i64,
                )
                .map_err(|e| e.to_string())
        })
        .unwrap();
        // The second pass runs after a route change, which is when an
        // unrouted row is offered again.
        for _ in 0..2 {
            app.mesh_retry_at = None;
            app.retry_mesh_mail();
            app.mesh_routes.table.invalidate();
        }
        let next_hop = with_store(|store| {
            Ok(store
                .collection_record(&delivery.envelope.key, now_ms() as i64)
                .map_err(|e| e.to_string())?
                .map(|record| record.next_hop))
        })
        .unwrap();
        assert_eq!(next_hop.as_deref(), Some(""), "never offered to the spoke");
    }
    /// Custody taken over an edge that has since gone, so the only route
    /// left leads back through the spoke (#928). One retry pass under the
    /// default grace notes the loop without acting on it.
    fn looped_custody() -> (App, Deliver) {
        let (mut app, delivery) = hub_reachable_only_through_spoke();
        let hub = app.node_id.clone().unwrap();
        with_store(|store| {
            store
                .accept_forward(
                    &delivery.envelope,
                    delivery.remaining_ms,
                    delivery.hops_left - 1,
                    &[delivery.visited[0].clone(), hub],
                    "",
                    Admission::Custody,
                    now_ms() as i64,
                )
                .map_err(|e| e.to_string())
        })
        .unwrap();
        app.mesh_retry_at = None;
        app.retry_mesh_mail();
        assert!(app.mesh_looped.contains_key(&delivery.envelope.key));
        (app, delivery)
    }
    fn row_state(key: &crate::mesh::key::MessageKey) -> String {
        with_store(|store| Ok(store.get(key).unwrap().unwrap().state)).unwrap()
    }
    #[tokio::test]
    async fn hub_reports_custody_whose_only_route_stays_looped_past_the_grace() {
        let _store = crate::mesh::runtime_store::TestStore::new();
        let (mut app, delivery) = looped_custody();
        let hub = app.node_id.clone().unwrap();
        assert_eq!(row_state(&delivery.envelope.key), "custody");
        app.settle_looped_custody(std::time::Duration::ZERO);
        assert!(app.mesh_looped.is_empty());
        with_store(|store| {
            let row = store.get(&delivery.envelope.key).unwrap().unwrap();
            assert_eq!(row.state, "refused");
            assert_eq!(row.next_hop, "", "never offered to the spoke");
            let receipts = store.by_request_key(&delivery.envelope.key).unwrap();
            assert_eq!(receipts.len(), 1, "the hub outcome is minted");
            let mail = store.get(&receipts[0]).unwrap().unwrap().envelope;
            assert_eq!(mail.key.origin_node, hub);
            assert_eq!(
                mail.return_binding.recipient_node,
                delivery.envelope.key.origin_node
            );
            assert!(store.hub_outcomes(now_ms() as i64).unwrap().is_empty());
            Ok(())
        })
        .unwrap();
    }
    #[tokio::test]
    async fn fleet_resume_restarts_the_loop_grace() {
        let _store = crate::mesh::runtime_store::TestStore::new();
        let (mut app, delivery) = looped_custody();
        let key = delivery.envelope.key.clone();
        let backdate = |app: &mut App| {
            *app.mesh_looped.get_mut(&key).unwrap() = std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(120))
                .unwrap();
        };
        // The grace ran out while the fleet was paused.
        app.fleet_pause.paused = true;
        app.mesh_retry_at = None;
        app.retry_mesh_mail();
        backdate(&mut app);
        app.fleet_pause.paused = false;
        app.mesh_retry_at = None;
        app.retry_mesh_mail();
        assert_eq!(row_state(&key), "custody", "resume starts the grace over");
        assert!(app.mesh_looped[&key].elapsed() < std::time::Duration::from_secs(60));
        // Without a pause the same lapsed grace is reported.
        backdate(&mut app);
        app.mesh_retry_at = None;
        app.retry_mesh_mail();
        assert_eq!(row_state(&key), "refused");
    }
    #[tokio::test]
    async fn hop_budget_exhausted_refuses_only_forwarding() {
        let _store = crate::mesh::runtime_store::TestStore::new();
        let mut app = app();
        let mut delivery = delivery();
        delivery.hops_left = 0;
        assert_eq!(app.check_mesh_import(&delivery, "hub.example"), Ok(None));
        assert_eq!(
            app.accept_forwarded_request(&delivery),
            Err("hop_budget_exhausted".into())
        );
    }
}
