use super::responses::{encode_error, encode_success};
use crate::api::schema::ResponseResult;
use crate::app::App;
use crate::mesh::{
    hello::{self, Enrollment, Hello, Offer, Pending},
    identity::NodeIdentity,
    store::PinSource,
};

impl App {
    /// Inspect every envelope before decoding or acknowledging any part of a batch.
    pub(super) fn handle_mesh_payload(
        &mut self,
        id: String,
        params: serde_json::Value,
        deliver: bool,
    ) -> String {
        if let Some(error) = self.check_mesh_session(&id, &params) {
            return error;
        }
        let response = if deliver {
            match serde_json::from_value(params) {
                Ok(params) => self.handle_mesh_deliver(id.clone(), params),
                Err(error) => encode_error(id.clone(), "invalid_request", error.to_string()),
            }
        } else {
            match serde_json::from_value(params) {
                Ok(params) => self.handle_mesh_collect(id.clone(), params),
                Err(error) => encode_error(id.clone(), "invalid_request", error.to_string()),
            }
        };
        if let Ok(value) = serde_json::from_str(&response) {
            if let Some(error) = self.check_mesh_session(&id, &value) {
                return error;
            }
        }
        response
    }

    /// Gate the current edge at receive and at dequeue, including forwarded mail.
    fn check_mesh_session(&mut self, id: &str, value: &serde_json::Value) -> Option<String> {
        let edge = self
            .inbound
            .edge_mut(
                self.current_api_peer_pid,
                crate::platform::process_start_time,
            )
            .filter(|edge| edge.enrolled())?;
        let version = edge.enrollment.negotiated.unwrap_or(hello::VERSION);
        let kind = crate::mesh::version_gate::check(value, version).err()?;
        let reason = crate::mesh::version_gate::violation(&edge.enrollment.peer, version, &kind);
        edge.clear(&reason);
        Some(encode_error(
            id.to_owned(),
            "mesh_protocol_violation",
            reason,
        ))
    }

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
                        serde_json::json!({"mesh": hello::version(), "mesh_min": hello::window().0}),
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
                    negotiated: None,
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
                    edge.enrollment.peer = peer.clone();
                }
                let identity = NodeIdentity::load().map_err(|e| e.to_string())?;
                let acceptor = Offer::new(&identity, crate::app::short_host_name())?;
                let negotiated = hello::negotiate(&acceptor, &offer, &peer)?;
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
                    negotiated: Some(negotiated),
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
                status.negotiated = Some(hello::negotiate(
                    &pending.acceptor,
                    &pending.dialer,
                    &status.peer,
                )?);
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

    /// ADR-0014's operator boundary for an operator-only verb: a local
    /// caller with no flk pane in its process ancestry that is not an inbound
    /// mesh relay. It fails closed: a caller whose pid the socket could not
    /// read is not the operator.
    ///
    /// Kernel UID is checked before ancestry (ADR-0036 §1a): detached dedicated
    /// agents never become operators. Same-UID PTYs remain cooperative (#1177).
    pub(super) fn caller_is_local_operator(&mut self) -> bool {
        let uid_allowed = self.current_api_peer_uid.is_none_or(|uid| {
            self.runtime_config.uid_policy.classify(uid)
                == crate::structured::identity::Principal::Operator
        });
        uid_allowed && self.local_operator_origin(self.current_api_peer_pid, self.lease_caller())
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
        if !self.caller_is_local_operator() {
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
                mesh_quarantined: crate::mesh::runtime_store::read(|store| {
                    store.quarantined_count().map_err(|e| e.to_string())
                })
                .ok()
                .flatten()
                .unwrap_or(0),
                routes: self.mesh_route_status(),
                route_generation: self.mesh_routes.table.generation(),
                peers,
                mesh_suspended_reason,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::{hello::legacy, runtime_store::TestStore};
    use ed25519_dalek::SigningKey;
    use serde_json::{json, Value};

    fn app() -> App {
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        let pid = std::process::id();
        app.current_api_peer_pid = Some(pid);
        app.inbound
            .attach(
                pid,
                crate::platform::process_start_time(pid).unwrap(),
                crate::platform::process_start_time,
            )
            .unwrap();
        app
    }

    #[tokio::test]
    async fn real_old_dialer_completes_new_acceptor_hello_and_records_five() {
        let _guard = crate::config::test_config_env_guard();
        let _store = TestStore::new();
        let mut app = app();
        let key = SigningKey::from_bytes(&[23; 32]);
        let dialer = legacy::offer(&key, "old.test");
        let begin: Hello =
            serde_json::from_value(json!({"phase":"begin", "offer":dialer})).unwrap();
        let reply = app.mesh_hello(begin).unwrap().unwrap();
        let wire = serde_json::to_value(&reply).unwrap();
        let acceptor: legacy::Offer = serde_json::from_value(wire["offer"].clone()).unwrap();
        let signature: Vec<u8> = serde_json::from_value(wire["signature"].clone()).unwrap();
        legacy::verify(&dialer, &acceptor, "acceptor", &signature);
        let signature = legacy::sign(&key, &dialer, &acceptor, "dialer");
        assert!(app
            .mesh_hello(Hello::Finish { signature })
            .unwrap()
            .is_none());
        let edge = app
            .inbound
            .edge(
                app.current_api_peer_pid,
                crate::platform::process_start_time,
            )
            .unwrap();
        assert!(edge.enrolled());
        assert_eq!(edge.enrollment.negotiated, Some(5));
    }

    #[tokio::test]
    async fn old_dialer_refuses_version_six_acceptor_before_finish() {
        let _guard = crate::config::test_config_env_guard();
        let _store = TestStore::new();
        std::env::set_var("FLOCK_TEST_MESH_VERSION", "6");
        std::env::set_var("FLOCK_TEST_MESH_MIN", "5");
        let mut app = app();
        let dialer = legacy::offer(&SigningKey::from_bytes(&[23; 32]), "old.test");
        let begin: Hello =
            serde_json::from_value(json!({"phase":"begin", "offer":dialer})).unwrap();
        let reply = app.mesh_hello(begin).unwrap().unwrap();
        let wire = serde_json::to_value(reply).unwrap();
        let acceptor: legacy::Offer = serde_json::from_value(wire["offer"].clone()).unwrap();
        assert_eq!(wire["offer"]["mesh"], 6);
        assert_eq!(wire["offer"]["mesh_min"], 5);
        assert_eq!(
            legacy::validate(&acceptor).unwrap_err(),
            "mesh version mismatch: local 5, remote 6; upgrade flk on this node"
        );
        let edge = app
            .inbound
            .edge(
                app.current_api_peer_pid,
                crate::platform::process_start_time,
            )
            .unwrap();
        assert!(!edge.enrolled());
        assert!(edge.pending.is_some());
        assert!(edge.enrollment.negotiated.is_none());
    }

    #[tokio::test]
    async fn protocol_violation_resets_inbound_session_before_batch_import_or_ack() {
        let _guard = crate::config::test_config_env_guard();
        let _store = TestStore::new();
        for (version, kind) in [(5, "future"), (4, "receipt")] {
            let mut app = app();
            let pid = std::process::id();
            let mut status = app
                .inbound
                .edge(Some(pid), crate::platform::process_start_time)
                .unwrap()
                .enrollment
                .clone();
            status.peer = "peer.test".into();
            status.node_id = Some("peer.test".into());
            status.state = "pinned".into();
            status.negotiated = Some(version);
            app.inbound
                .enroll(pid, status, crate::platform::process_start_time)
                .unwrap();
            let response = app.handle_mesh_payload(
                "probe".into(),
                json!({"outbound":{"ack":[{}, {"receipt":{"kind":kind}}]}}),
                false,
            );
            let response: Value = serde_json::from_str(&response).unwrap();
            assert_eq!(response["error"]["code"], "mesh_protocol_violation");
            assert!(response.get("result").is_none());
            let edge = app
                .inbound
                .edge(Some(pid), crate::platform::process_start_time)
                .unwrap();
            assert!(!edge.enrolled());
            assert_eq!(edge.enrollment.negotiated, None);
            assert!(edge
                .enrollment
                .reason
                .as_deref()
                .unwrap()
                .starts_with("mesh_protocol_violation:"));
        }
    }

    #[tokio::test]
    async fn sequence_forms_and_invalid_tags_reset_the_inbound_session() {
        let _guard = crate::config::test_config_env_guard();
        let _store = TestStore::new();
        for (payload, deliver) in crate::mesh::version_gate::tests::noncanonical_payloads() {
            let mut app = app();
            let pid = std::process::id();
            let mut status = app
                .inbound
                .edge(Some(pid), crate::platform::process_start_time)
                .unwrap()
                .enrollment
                .clone();
            status.peer = "peer.test".into();
            status.node_id = Some("peer.test".into());
            status.state = "pinned".into();
            status.negotiated = Some(5);
            app.inbound
                .enroll(pid, status, crate::platform::process_start_time)
                .unwrap();
            let response = app.handle_mesh_payload("probe".into(), payload.clone(), deliver);
            let response: Value = serde_json::from_str(&response).unwrap();
            assert_eq!(
                response["error"]["code"], "mesh_protocol_violation",
                "{payload}: {response}"
            );
            assert!(response.get("result").is_none());
            let edge = app
                .inbound
                .edge(Some(pid), crate::platform::process_start_time)
                .unwrap();
            assert!(!edge.enrolled(), "{payload}");
            assert_eq!(edge.enrollment.negotiated, None);
            assert_eq!(edge.enrollment.state, "refused");
        }
    }
}
