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
        let node = self.mesh_routes.table.next_hop(owner).unwrap_or_default();
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

    pub(super) fn incompatible_request_peer(&self, owner: &str, next: &str) -> Option<String> {
        self.state.peers.iter().find_map(|peer| {
            let status = crate::peer_stream::enrollment(peer);
            if status.state != "refused" {
                return None;
            }
            let pin = with_store(|store| store.get_pin(&peer.name).map_err(|e| e.to_string()))
                .ok()
                .flatten()?;
            (pin.node_id == owner || pin.node_id == next).then(|| peer.name.clone())
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
