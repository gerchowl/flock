use super::responses::{encode_error, encode_success};
use crate::api::schema::ResponseResult;
use crate::app::App;
use crate::mesh::{
    hello::{self, Enrollment, Hello, Offer, Pending},
    identity::NodeIdentity,
};

impl App {
    pub(super) fn handle_mesh_hello(&mut self, id: String, hello: Hello) -> String {
        // Attachment verifies sshd ancestry and process start time, including
        // on plain (non-pushing) held relays.
        let attached = self.handle_peers_relay_attach(id.clone());
        if serde_json::from_str::<serde_json::Value>(&attached)
            .is_ok_and(|v| v.get("error").is_some())
        {
            return attached;
        }
        match self.mesh_hello(hello) {
            Ok(Some(challenge)) => encode_success(id, ResponseResult::MeshHello { challenge }),
            Ok(None) => encode_success(id, ResponseResult::Ok {}),
            Err(reason) => {
                if let Some(status) = self.mesh_inbound.as_mut() {
                    self.uplink.reset_enrollment(&status.peer);
                    status.state = "refused".into();
                    status.reason = Some(reason.clone());
                }
                if reason.starts_with("mesh version mismatch:") {
                    return super::responses::encode_error_with_data(
                        id,
                        "mesh_version_mismatch",
                        reason,
                        serde_json::json!({"mesh": hello::VERSION}),
                    );
                }
                encode_error(id, "mesh_refused", reason)
            }
        }
    }

    fn mesh_hello(&mut self, hello: Hello) -> Result<Option<hello::Challenge>, String> {
        match hello {
            Hello::Check => {
                self.uplink
                    .enrolled_hub()
                    .ok_or("mesh edge is not enrolled; repeat mesh.hello")?;
                Ok(None)
            }
            Hello::Begin { offer } => {
                self.mesh_pending = None;
                if let Some(peer) = self.uplink.enrolled_hub() {
                    self.uplink.reset_enrollment(&peer);
                }
                // A configured alias takes precedence over the remote label.
                let peer = hello::with_store(|store| {
                    for peer in &self.state.peers {
                        if store
                            .get_pin(&peer.name)
                            .map_err(|e| e.to_string())?
                            .is_some_and(|pin| pin.node_id == offer.node_id)
                        {
                            return Ok(peer.name.clone());
                        }
                    }
                    Ok(offer.name.clone())
                })?;
                self.mesh_inbound = Some(Enrollment {
                    peer: peer.clone(),
                    node_id: None,
                    state: "pending".into(),
                    reason: None,
                });
                offer.validate(&peer)?;
                hello::check_pin(&peer, &offer, false)?;
                let identity = NodeIdentity::load().map_err(|e| e.to_string())?;
                let acceptor = Offer::new(&identity, crate::app::short_host_name())?;
                let signature = hello::sign(&identity, &offer, &acceptor, "acceptor")?;
                self.mesh_pending = Some(Pending {
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
                    .mesh_pending
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
                let status = self.mesh_inbound.as_mut().ok_or("no pending enrollment")?;
                hello::check_pin(&status.peer, &pending.dialer, true)?;
                status.node_id = Some(pending.dialer.node_id);
                status.state = "pinned".into();
                status.reason = None;
                self.uplink.enroll_hub(status.peer.clone());
                Ok(None)
            }
        }
    }

    pub(super) fn handle_peers_enroll_reset(&mut self, id: String, peer: String) -> String {
        if self.current_api_peer_pid.is_none()
            || self
                .parse_pane_id_or_peer("", self.current_api_peer_pid)
                .is_some()
            || self.uplink.is_relay(
                self.current_api_peer_pid,
                crate::platform::process_start_time,
            )
        {
            return encode_error(
                id,
                "operator_only",
                "enrollment reset requires a local operator outside agent panes and relays",
            );
        }
        match hello::with_store(|store| store.reset_pin(&peer).map_err(|e| e.to_string())) {
            Ok(_) => {
                crate::peer_stream::reset_enrollment(&peer);
                self.uplink.reset_enrollment(&peer);
                self.mesh_pending = None;
                if self
                    .mesh_inbound
                    .as_ref()
                    .is_some_and(|status| status.peer == peer)
                {
                    self.mesh_inbound = None;
                }
                encode_success(id, ResponseResult::Ok {})
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
        if let Some(inbound) = self.mesh_inbound.as_ref() {
            if !peers.iter().any(|p| p.peer == inbound.peer) {
                peers.push(inbound.clone());
            }
        }
        encode_success(id, ResponseResult::PeersEnrollment { peers })
    }
}
