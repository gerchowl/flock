use super::responses::{encode_error, encode_success};
use crate::api::schema::ResponseResult;
use crate::app::App;
use crate::mesh::{
    hello::{self, Enrollment, Hello, Offer, Pending},
    identity::NodeIdentity,
    store::PinSource,
};

impl App {
    pub(super) fn handle_mesh_hello(&mut self, id: String, hello: Hello) -> String {
        if matches!(&hello, Hello::Begin { .. }) {
            let attached = self.attach_inbound_edge(id.clone());
            if serde_json::from_str::<serde_json::Value>(&attached)
                .is_ok_and(|v| v.get("error").is_some())
            {
                return attached;
            }
        }
        match self.mesh_hello(hello) {
            Ok(Some(challenge)) => encode_success(id, ResponseResult::MeshHello { challenge }),
            Ok(None) => encode_success(id, ResponseResult::Ok {}),
            Err(reason) => {
                if let Some(edge) = self.inbound.edge_mut(
                    self.current_api_peer_pid,
                    crate::platform::process_start_time,
                ) {
                    edge.clear(&reason);
                }
                if reason.starts_with("mesh version mismatch:") {
                    return super::responses::encode_error_with_data(
                        id,
                        "mesh_version_mismatch",
                        reason,
                        serde_json::json!({"mesh": hello::version()}),
                    );
                }
                encode_error(id, "mesh_refused", reason)
            }
        }
    }

    fn mesh_hello(&mut self, hello: Hello) -> Result<Option<hello::Challenge>, String> {
        match hello {
            Hello::Check => {
                self.inbound
                    .edge(
                        self.current_api_peer_pid,
                        crate::platform::process_start_time,
                    )
                    .filter(|edge| edge.enrolled())
                    .ok_or("mesh hello required; upgrade flk on the dialer or repeat mesh.hello")?;
                Ok(None)
            }
            Hello::Begin { offer } => {
                self.inbound
                    .edge_mut(
                        self.current_api_peer_pid,
                        crate::platform::process_start_time,
                    )
                    .ok_or("unattestable relay")?
                    .clear("mesh handshake in progress");
                let peer = hello::with_store(|store| {
                    store
                        .pin_name_from(PinSource::Configured, &offer.pin())
                        .map_err(|e| e.to_string())
                })?
                .unwrap_or_else(|| offer.name.clone());
                let configured_name = self.state.peers.iter().any(|p| p.name == offer.name);
                self.inbound
                    .edge_mut(
                        self.current_api_peer_pid,
                        crate::platform::process_start_time,
                    )
                    .ok_or("unattestable relay")?
                    .enrollment = Enrollment {
                    peer: if configured_name {
                        "unidentified SSH peer".into()
                    } else {
                        peer.clone()
                    },
                    source: PinSource::Inbound,
                    pin_origin: Default::default(),
                    node_id: None,
                    state: "pending".into(),
                    reason: None,
                };
                offer.validate(&peer)?;
                // The signed remote name stays pinned independently of its local alias.
                hello::check_inbound_pin(&offer, false, configured_name)?;
                if let Some(edge) = self.inbound.edge_mut(
                    self.current_api_peer_pid,
                    crate::platform::process_start_time,
                ) {
                    edge.enrollment.peer = peer;
                }
                let identity = NodeIdentity::load().map_err(|e| e.to_string())?;
                let acceptor = Offer::new(&identity, crate::app::short_host_name())?;
                let signature = hello::sign(&identity, &offer, &acceptor, "acceptor")?;
                self.inbound
                    .edge_mut(
                        self.current_api_peer_pid,
                        crate::platform::process_start_time,
                    )
                    .ok_or("unattestable relay")?
                    .pending = Some(Pending {
                    pid: self.current_api_peer_pid.ok_or("unattestable relay")?,
                    process_started: self
                        .current_api_peer_pid
                        .and_then(crate::platform::process_start_time)
                        .ok_or("unattestable relay start time")?,
                    started: std::time::Instant::now(),
                    dialer: offer,
                    acceptor: acceptor.clone(),
                });
                Ok(Some(hello::Challenge {
                    offer: acceptor,
                    signature,
                }))
            }
            Hello::Finish { signature } => {
                let pending = self
                    .inbound
                    .edge_mut(
                        self.current_api_peer_pid,
                        crate::platform::process_start_time,
                    )
                    .ok_or("unattestable relay")?
                    .pending
                    .take()
                    .ok_or("no outstanding mesh challenge")?;
                if self.current_api_peer_pid != Some(pending.pid)
                    || crate::platform::process_start_time(pending.pid)
                        != Some(pending.process_started)
                    || pending.started.elapsed().as_secs() > 30
                {
                    return Err("mesh challenge expired or belongs to another edge".into());
                }
                hello::verify(&pending.dialer, &pending.acceptor, "dialer", &signature)?;
                let mut status = self
                    .inbound
                    .edge(
                        self.current_api_peer_pid,
                        crate::platform::process_start_time,
                    )
                    .ok_or("no pending enrollment")?
                    .enrollment
                    .clone();
                hello::check_inbound_pin(
                    &pending.dialer,
                    true,
                    self.state
                        .peers
                        .iter()
                        .any(|p| p.name == pending.dialer.name),
                )?;
                status.pin_origin = hello::with_store(|store| {
                    store
                        .pin_origin(PinSource::Inbound, &pending.dialer.name)
                        .map(|origin| origin.unwrap_or_default())
                        .map_err(|e| e.to_string())
                })?;
                status.node_id = Some(pending.dialer.node_id);
                status.state = "pinned".into();
                status.reason = None;
                self.inbound
                    .enroll(pending.pid, status, crate::platform::process_start_time)?;
                self.mesh_retry_at = None;
                Ok(None)
            }
        }
    }

    pub(super) fn handle_peers_enroll_reset(
        &mut self,
        id: String,
        params: crate::api::schema::PeersEnrollResetParams,
    ) -> String {
        let crate::api::schema::PeersEnrollResetParams {
            peer,
            source,
            preview,
            expected_node_id,
        } = params;
        if self.current_api_peer_pid.is_none()
            || self
                .parse_pane_id_or_peer("", self.current_api_peer_pid)
                .is_some()
            || self
                .inbound
                .edge(
                    self.current_api_peer_pid,
                    crate::platform::process_start_time,
                )
                .is_some()
        {
            return encode_error(
                id,
                "operator_only",
                "enrollment reset requires a local operator outside agent panes and relays",
            );
        }
        match hello::with_store(|store| {
            let mut pin_peer = peer.clone();
            if source == PinSource::Inbound {
                if let Some(configured) = store.get_pin(&peer).map_err(|e| e.to_string())? {
                    if let Some(name) = store
                        .pin_name_from(PinSource::Inbound, &configured)
                        .map_err(|e| e.to_string())?
                    {
                        pin_peer = name;
                    }
                }
            }
            let node_id = store
                .get_pin_from(source, &pin_peer)
                .map_err(|e| e.to_string())?
                .map(|pin| pin.node_id);
            if !preview {
                if node_id != expected_node_id {
                    return Err("pin changed since preview; inspect it before resetting".into());
                }
                store
                    .reset_pin_from(source, &pin_peer)
                    .map_err(|e| e.to_string())?;
            }
            Ok((pin_peer, node_id))
        }) {
            Ok((pin_peer, node_id)) => {
                if !preview {
                    if source == PinSource::Configured {
                        crate::peer_stream::reset_enrollment(&peer);
                    } else {
                        self.inbound.reset(&pin_peer, node_id.as_deref());
                    }
                }
                encode_success(
                    id,
                    ResponseResult::PeersEnrollReset {
                        peer,
                        source,
                        node_id,
                    },
                )
            }
            Err(reason) => encode_error(id, "mesh_store_unavailable", reason),
        }
    }

    pub(super) fn handle_peers_enrollment(&mut self, id: String) -> String {
        let mut peers: Vec<_> = self
            .state
            .peers
            .iter()
            .map(crate::peer_stream::enrollment)
            .collect();
        let mesh_suspended_reason = crate::mesh::runtime_store::recovery_reason();
        if !peers.is_empty() && mesh_suspended_reason.is_none() {
            let result = hello::with_store(|store| {
                for peer in &mut peers {
                    peer.pin_origin = store
                        .pin_origin(peer.source, &peer.peer)
                        .map_err(|e| e.to_string())?
                        .unwrap_or_default();
                    if peer.node_id.is_none() {
                        peer.node_id = store
                            .get_pin(&peer.peer)
                            .map_err(|e| e.to_string())?
                            .map(|pin| pin.node_id);
                    }
                }
                Ok(())
            });
            if let Err(reason) = result {
                return encode_error(id, "mesh_store_unavailable", reason);
            }
        }
        for edge in self.inbound.live(crate::platform::process_start_time) {
            let mut inbound = edge.enrollment.clone();
            if let Some(configured) = peers
                .iter()
                .find(|peer| inbound.node_id.is_some() && peer.node_id == inbound.node_id)
            {
                inbound.peer = configured.peer.clone();
            }
            peers.push(inbound);
        }
        encode_success(
            id,
            ResponseResult::PeersEnrollment {
                routes: self.mesh_route_status(),
                peers,
                mesh_suspended_reason,
            },
        )
    }
}
