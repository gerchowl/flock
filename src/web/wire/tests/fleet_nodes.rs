//! Real summary ingress and signed routing through the public fleet subscription.
use super::*;
use crate::{
    app::App,
    config::PeerConfig,
    mesh::{
        identity::NodeIdentity,
        routes::{Adjacency, Advert},
    },
};
use std::collections::BTreeMap;

struct Env(Vec<(&'static str, Option<std::ffi::OsString>)>);
impl Env {
    fn new(root: &std::path::Path) -> Self {
        Self(
            [
                ("XDG_CONFIG_HOME", root.join("config")),
                ("XDG_STATE_HOME", root.join("state")),
                (crate::config::CONFIG_PATH_ENV_VAR, root.join("config.toml")),
            ]
            .into_iter()
            .map(|(key, path)| {
                let old = std::env::var_os(key);
                std::env::set_var(key, path);
                (key, old)
            })
            .collect(),
        )
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}
pub(super) struct Case {
    pub server: Server,
    pub app: App,
    _env: Env,
}
impl Case {
    pub async fn new() -> Self {
        let server = Server::new().await;
        let env = Env::new(server._root.path());
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        app.node_id = Some(NodeIdentity::fixture([80; 32]).node_id());
        Self {
            server,
            app,
            _env: env,
        }
    }
    pub fn peer(&mut self, name: &str) -> PeerConfig {
        let peer = PeerConfig {
            name: name.into(),
            ssh: "hidden.example".into(),
            ..Default::default()
        };
        self.app
            .state
            .peer_summaries
            .push(crate::peers::PeerSummaryState::new(&peer));
        self.app.state.peers.push(peer.clone());
        peer
    }
    pub fn success(&mut self, peer: &PeerConfig, node: &str, extra: Value, latency: u64) {
        self.server.clock.advance(1);
        let mut result = json!({"host":"hidden.example","node_id":node,"protocol":crate::protocol::PROTOCOL_VERSION,"version":"fixture-version","workspaces":[]});
        result
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let payload =
            crate::peers::parse_summary_response(&json!({"result":result}).to_string(), latency)
                .unwrap();
        self.app.handle_internal_event_at(
            crate::events::AppEvent::PeerSummaryFetched(crate::peers::PeerSummaryFetch {
                peer: peer.name.clone(),
                result: Ok(payload),
                stream_error: None,
            }),
            self.server.clock.now(),
        );
    }
    pub fn fail(&mut self, peer: &PeerConfig, error: &str) {
        self.server.clock.advance(1);
        self.app.handle_internal_event_at(
            crate::events::AppEvent::PeerSummaryFetched(crate::peers::PeerSummaryFetch {
                peer: peer.name.clone(),
                result: Err(error.into()),
                stream_error: None,
            }),
            self.server.clock.now(),
        );
    }
    pub fn service(&mut self) {
        Arc::get_mut(&mut self.server.core)
            .unwrap()
            .service_app(&mut self.app, self.server._root.path());
    }
    pub async fn frame(&mut self, socket: &mut Socket) -> Value {
        let wake = self.server.wake.clone();
        within(async { loop { tokio::select! {biased; frame=socket.text()=>return frame, _=wake.notified()=>self.service(),} } }).await
    }
    pub async fn open(&mut self) -> Socket {
        self.service();
        let mut socket = Socket::open(&self.server).await;
        socket.hello(1, &"a".repeat(64)).await;
        socket
    }
    pub async fn snapshot(&mut self) -> BTreeMap<String, Value> {
        let mut socket = self.open().await;
        socket
            .send(json!({"t":"sub","sub_id":"nodes","stream":"fleet"}))
            .await;
        let mut rows = BTreeMap::new();
        loop {
            let frame = self.frame(&mut socket).await;
            if frame["t"] == "synced" {
                break;
            }
            if frame["ev"]["type"] == "fleet.node.upsert" {
                let row = frame["ev"]["row"].clone();
                rows.insert(row["node_id"].as_str().unwrap().into(), row);
            }
        }
        rows
    }
    fn direct(&mut self, node: &NodeIdentity) {
        let home = NodeIdentity::fixture([80; 32]);
        self.app
            .mesh_routes
            .table
            .set_own(advert(&home, vec![node.node_id()]));
        self.app
            .mesh_routes
            .table
            .learn(&node.node_id(), vec![advert(node, vec![home.node_id()])]);
    }
}
fn advert(node: &NodeIdentity, neighbors: Vec<String>) -> Advert {
    Advert::signed(
        node,
        "hidden.example".into(),
        (1, 1),
        neighbors
            .into_iter()
            .map(|node_id| Adjacency {
                node_id,
                name: "hidden.example".into(),
            })
            .collect(),
    )
}
#[test]
fn bound_node_status_updates_and_safe_failures_reach_the_public_stream() {
    let _guard = crate::config::test_config_env_guard();
    runtime(async {
        let mut case = Case::new().await;
        let peer = case.peer("peer.example");
        assert_eq!(
            case.snapshot().await.len(),
            1,
            "unbound peers have no provisional identity"
        );
        let remote = NodeIdentity::fixture([81; 32]);
        let id = remote.node_id();
        crate::peer_stream::test_enroll(&peer, &id);
        case.direct(&remote);
        let rows = case.snapshot().await;
        assert!(
            rows.contains_key(&id),
            "authenticated route missing from fleet"
        );
        assert_eq!(rows[&id]["online"], false);
        assert!(rows[&id]["health"].is_null());
        assert!(rows[&id]["last_ok"].is_null());
        case.fail(&peer, "Permission denied (publickey)");
        let first_failure = case.snapshot().await[&id].clone();
        assert_eq!(
            first_failure["error"]["code"], "auth_refused",
            "enrolled owner failure must not remain pending"
        );
        assert!(first_failure["last_ok"].is_null());
        assert!(first_failure["health"].is_null());
        case.success(
            &peer,
            &id,
            json!({"system":{"cpu_percent":23,"mem_used":4096,"mem_total":8192,"disk_free":1024}}),
            17,
        );
        let row = case.snapshot().await[&id].clone();
        assert_eq!(row["online"], true);
        assert_eq!(row["latency_ms"], 17);
        assert_eq!(row["version"], "fixture-version");
        assert_eq!(row["health"]["cpu_percent"], 23);
        assert_eq!(row["health"]["mem_used"], 4096);
        assert_eq!(row["route"]["next_hop"], id);
        assert_eq!(row["route"]["hops"], 1);
        assert_eq!(row["name"], format!("Node {id}"));
        assert_eq!(row["route"]["name"], row["name"]);
        let mut socket = case.open().await;
        socket
            .send(json!({"t":"sub","sub_id":"live","stream":"fleet"}))
            .await;
        while case.frame(&mut socket).await["t"] != "synced" {}
        case.success(
            &peer,
            &id,
            json!({"version":"updated-version","system":{"cpu_percent":42}}),
            29,
        );
        case.service();
        let update = case.frame(&mut socket).await;
        assert_eq!(update["ev"]["row"]["latency_ms"], 29);
        assert_eq!(update["ev"]["row"]["version"], "updated-version");
        assert_eq!(update["ev"]["row"]["health"]["cpu_percent"], 42);
        let last = update["ev"]["row"]["last_ok"].clone();
        assert!(last.as_u64().is_some());
        case.fail(
            &peer,
            "Permission denied (publickey): /private/credential.example",
        );
        case.service();
        let failure = case.frame(&mut socket).await["ev"]["row"].clone();
        assert_eq!(failure["online"], false);
        assert_eq!(failure["error"]["code"], "auth_refused");
        assert_eq!(failure["health"]["cpu_percent"], 42);
        assert_eq!(failure["latency_ms"], 29);
        assert_eq!(failure["last_ok"], last);
        assert!(!failure.to_string().contains("credential.example"));
        assert!(!failure.to_string().contains("hidden.example"));
        case.app.state.peer_summaries.clear();
        case.app
            .mesh_routes
            .table
            .set_live_edges(Default::default());
        let retained = case.snapshot().await[&id].clone();
        assert_eq!(retained["online"], false);
        assert!(retained["route"].is_null());
        assert_eq!(retained["health"]["cpu_percent"], 42);
        assert_eq!(retained["last_ok"], last);
        assert_eq!(retained["error"]["code"], "auth_refused");
        drop(socket);
        case.server.core.shutdown().await.unwrap();
        case.server.task.abort();
        let _ = (&mut case.server.task).await;
        let core = Core::with_clock(
            TokenStore::open(case.server._root.path()).unwrap(),
            Arc::new(case.server.clock.clone()),
            case.server.wake.clone(),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        case.server.bind = listener.local_addr().unwrap();
        let router = core.router();
        case.server.task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        case.server.core = Arc::new(core);
        let restored = case.snapshot().await;
        assert_eq!(
            restored[&id], retained,
            "restart must not erase a known offline owner"
        );
        case.server.finish().await;
    });
}
#[test]
fn diamond_uses_the_actual_next_hop_and_never_joins_by_name() {
    let _guard = crate::config::test_config_env_guard();
    runtime(async {
        let mut case = Case::new().await;
        let peer = case.peer("diamond.example");
        let home = NodeIdentity::fixture([80; 32]);
        let a = NodeIdentity::fixture([81; 32]);
        let b = NodeIdentity::fixture([82; 32]);
        let target = NodeIdentity::fixture([83; 32]);
        let id = target.node_id();
        crate::peer_stream::test_enroll(&peer, &id);
        case.app
            .mesh_routes
            .table
            .set_own(advert(&home, vec![a.node_id(), b.node_id()]));
        case.app.mesh_routes.table.learn(
            &a.node_id(),
            vec![
                advert(&a, vec![home.node_id(), id.clone()]),
                advert(&b, vec![home.node_id(), id.clone()]),
                advert(&target, vec![a.node_id(), b.node_id()]),
            ],
        );
        case.app.mesh_routes.table.learn(
            &b.node_id(),
            vec![
                advert(&a, vec![home.node_id(), id.clone()]),
                advert(&b, vec![home.node_id(), id.clone()]),
                advert(&target, vec![a.node_id(), b.node_id()]),
            ],
        );
        case.success(&peer, &id, json!({}), 9);
        let rows = case.snapshot().await;
        assert_eq!(rows.len(), 4, "same names do not collapse distinct nodes");
        let route = case
            .app
            .mesh_routes
            .table
            .routes()
            .into_iter()
            .find(|r| r.node == id)
            .unwrap();
        assert_eq!(rows[&id]["route"]["next_hop"], route.next_hop);
        assert_eq!(rows[&id]["route"]["hops"], 2);
        case.app.mesh_routes.table.withdraw_edge(&route.next_hop);
        let changed = case.snapshot().await[&id].clone();
        assert_ne!(changed["route"]["next_hop"], route.next_hop);
        assert_eq!(changed["online"], true);
        case.server.finish().await;
    });
}
#[test]
fn rebinding_a_peer_cannot_transfer_health_to_the_new_identity() {
    let _guard = crate::config::test_config_env_guard();
    runtime(async {
        let mut case = Case::new().await;
        let peer = case.peer("rebind.example");
        let a = NodeIdentity::fixture([81; 32]);
        let b = NodeIdentity::fixture([82; 32]);
        crate::peer_stream::test_enroll(&peer, &a.node_id());
        case.direct(&a);
        case.success(&peer, &a.node_id(), json!({"system":{"cpu_percent":71}}), 5);
        assert_eq!(
            case.snapshot().await[&a.node_id()]["health"]["cpu_percent"],
            71
        );
        crate::peer_stream::test_enroll(&peer, &b.node_id());
        case.direct(&b);
        case.fail(&peer, "Permission denied (publickey)");
        let failure = case.snapshot().await[&b.node_id()].clone();
        assert_eq!(failure["error"]["code"], "auth_refused");
        for field in ["health", "last_ok", "version", "latency_ms"] {
            assert!(failure[field].is_null(), "new owner inherited {field}");
        }
        case.success(&peer, &b.node_id(), json!({}), 6);
        let rows = case.snapshot().await;
        assert!(
            rows[&b.node_id()]["health"].is_null(),
            "new owner inherited old health"
        );
        assert_eq!(rows[&b.node_id()]["online"], true);
        assert_eq!(rows[&a.node_id()]["online"], false);
        assert_eq!(rows[&a.node_id()]["health"]["cpu_percent"], 71);
        case.server.finish().await;
    });
}

#[test]
fn proven_owner_relay_keeps_null_latency_stable_time_and_legacy_protocol_refusal() {
    let _guard = crate::config::test_config_env_guard();
    runtime(async {
        let mut case = Case::new().await;
        let hub = NodeIdentity::fixture([81; 32]);
        let leaf = NodeIdentity::fixture([83; 32]);
        let home = NodeIdentity::fixture([80; 32]);
        let peer = case.peer("hub.example");
        let leaf_peer = case.peer("leaf.example");
        crate::peer_stream::test_enroll(&leaf_peer, &leaf.node_id());
        crate::peer_stream::test_enroll(&peer, &hub.node_id());
        case.app
            .mesh_routes
            .table
            .set_own(advert(&home, vec![hub.node_id()]));
        case.app.mesh_routes.table.learn(
            &hub.node_id(),
            vec![
                advert(&hub, vec![home.node_id(), leaf.node_id()]),
                advert(&leaf, vec![hub.node_id()]),
            ],
        );
        case.success(&peer,&hub.node_id(),json!({"relayed_fleet":[{
            "node_id":leaf.node_id(),"name":"leaf.example","ssh_target":"leaf.example","host":"leaf.example","origin":"origin.example",
            "version":"owner-version","protocol":crate::protocol::PROTOCOL_VERSION,"system":{"cpu_percent":14},"latency_ms":9999,"age_secs":5,"origin_last_ok_secs":5,"proxy_jump":"hub.example"
        }]}),2);
        case.success(&peer,&hub.node_id(),json!({"relayed_fleet":[{
            "node_id":hub.node_id(),"name":"hub.example","ssh_target":"hub.example","host":"hub.example","origin":"hub.example",
            "version":"owner-version","protocol":crate::protocol::PROTOCOL_VERSION,"system":{"cpu_percent":14},"latency_ms":9999,"age_secs":5,"origin_last_ok_secs":5,"proxy_jump":"hub.example"
        }]}),2);
        // This local binding is the enrolled immediate channel, not a claim
        // made by the relayed payload. Remove the direct reading to observe it.
        case.app
            .state
            .server_nodes
            .insert(peer.name.clone(), hub.node_id());
        let index = case
            .app
            .state
            .peer_summaries
            .iter()
            .position(|p| p.peer == peer.name)
            .unwrap();
        let direct = case.app.state.peer_summaries.remove(index);
        let capture = case
            .app
            .state
            .relayed_fleet_cache
            .values()
            .find(|e| e.peer.node_id.as_deref() == Some(hub.node_id().as_str()))
            .unwrap()
            .peer
            .ingested_at
            .unwrap();
        case.server.clock.advance_to(capture);
        let rows = case.snapshot().await;
        for field in ["health", "version", "last_ok", "latency_ms"] {
            assert!(
                rows[&leaf.node_id()][field].is_null(),
                "unproven leaf {field}"
            );
        }
        assert_eq!(rows[&leaf.node_id()]["error"]["code"], "pending");
        let row = rows[&hub.node_id()].clone();
        assert_eq!(row["online"], true);
        assert!(row["latency_ms"].is_null());
        assert_eq!(row["health"]["cpu_percent"], 14);
        assert_eq!(row["route"]["next_hop"], hub.node_id());
        assert_eq!(row["route"]["hops"], 1);
        assert!(row["last_ok"].as_u64().is_some());
        case.server.clock.advance(7);
        assert_eq!(
            case.snapshot().await[&hub.node_id()]["last_ok"],
            row["last_ok"],
            "GUI traffic cannot freshen the owner"
        );
        case.app.state.peer_summaries.insert(index, direct);
        case.success(&peer, &hub.node_id(), json!({"protocol":null}), 3);
        let legacy = case.snapshot().await[&hub.node_id()].clone();
        assert_eq!(legacy["online"], false);
        assert_eq!(legacy["error"]["code"], "peer_incompatible");
        case.success(
            &peer,
            &hub.node_id(),
            json!({"protocol":crate::protocol::PROTOCOL_VERSION+1}),
            4,
        );
        let newer = case.snapshot().await[&hub.node_id()].clone();
        assert_eq!(newer["online"], false);
        assert_eq!(newer["error"]["code"], "peer_incompatible");
        case.app.state.relayed_fleet_cache.clear();
        case.success(&peer,&hub.node_id(),json!({"relayed_fleet":[{
            "node_id":hub.node_id(),"name":"hub.example","ssh_target":"hub.example","host":"hub.example","origin":"origin.example", "protocol":crate::protocol::PROTOCOL_VERSION,"latency_ms":9999
        }]}),5);
        case.app.state.peer_summaries.remove(index);
        let capture = case
            .app
            .state
            .relayed_fleet_cache
            .values()
            .next()
            .unwrap()
            .peer
            .ingested_at
            .unwrap();
        case.server.clock.advance_to(capture);
        let unknown_observer = case.snapshot().await[&hub.node_id()].clone();
        assert!(
            unknown_observer["latency_ms"].is_null(),
            "a matching configured name does not establish a home observation"
        );
        assert_eq!(unknown_observer["online"], false);
        case.server.finish().await;
    });
}

async fn assert_unproven_claim_is_ignored(relayed: bool) {
    let mut case = Case::new().await;
    let owner = NodeIdentity::fixture([91; 32]);
    let id = owner.node_id();
    let peer = case.peer("verified.example");
    crate::peer_stream::test_enroll(&peer, &id);
    case.direct(&owner);
    case.success(&peer, &id, json!({"system":{"cpu_percent":17}}), 7);
    let original = case.snapshot().await[&id].clone();
    case.fail(&peer, "Permission denied (publickey)");
    if relayed {
        let hub = NodeIdentity::fixture([92; 32]);
        let via = case.peer("claims.example");
        crate::peer_stream::test_enroll(&via, &hub.node_id());
        case.success(&via,&hub.node_id(),json!({"relayed_fleet":[{"name":"claimed.example","host":"claimed.example","ssh_target":"claimed.example","origin":"claims.example","node_id":id,"age_secs":0,"origin_last_ok_secs":0,"protocol":crate::protocol::PROTOCOL_VERSION,"version":"invented-version","system":{"cpu_percent":99}}]}),1);
        let capture = case
            .app
            .state
            .relayed_fleet_cache
            .values()
            .next()
            .unwrap()
            .peer
            .ingested_at
            .unwrap();
        case.server
            .clock
            .advance_to(capture + Duration::from_millis(1));
    } else {
        // A cached claim survives the source losing its independent binding.
        let unpinned = case.peer("unpinned.example");
        crate::peer_stream::test_enroll(&unpinned, &id);
        case.success(
            &unpinned,
            &id,
            json!({"system":{"cpu_percent":88},"version":"unproven-version"}),
            2,
        );
        crate::peer_stream::reset_enrollment(&unpinned.name);
    }
    let row = case.snapshot().await[&id].clone();
    case.server.finish().await;
    assert_eq!(row["online"], false);
    assert_eq!(row["error"]["code"], "auth_refused");
    for field in ["health", "version", "last_ok", "latency_ms"] {
        assert_eq!(
            row[field], original[field],
            "unproven source replaced {field}"
        );
    }
}
#[test]
fn unproven_relay_claim_cannot_replace_a_pinned_refusal() {
    let _guard = crate::config::test_config_env_guard();
    runtime(assert_unproven_claim_is_ignored(true));
}
#[test]
fn unproven_direct_claim_cannot_replace_a_pinned_refusal() {
    let _guard = crate::config::test_config_env_guard();
    runtime(assert_unproven_claim_is_ignored(false));
}

#[test]
fn a_failed_poll_without_a_pin_preserves_the_previous_owner_observation() {
    let _guard = crate::config::test_config_env_guard();
    runtime(async {
        let mut case = Case::new().await;
        let peer = case.peer("retained.example");
        let owner = NodeIdentity::fixture([93; 32]);
        crate::peer_stream::test_enroll(&peer, &owner.node_id());
        case.success(&peer,&owner.node_id(),json!({"system":{"cpu_percent":31},"icon":"cpu","workspaces":[{"workspace":"retained-project","agent":"claude","status":"working","pane_id":"fixture-pane"}],"structured_agents":[{"agent_id":"retained-agent","runtime":"structured","status":"working"}]}),9);
        let before = case.app.state.peer_summaries[0].clone();
        crate::peer_stream::reset_enrollment(&peer.name);
        assert!(crate::peer_stream::enrollment(&peer).node_id.is_none());
        case.fail(&peer, "Permission denied (publickey)");
        let after = case.app.state.peer_summaries[0].clone();
        case.server.finish().await;
        assert!(before.last_ok.is_some());
        assert!(before.system.is_some());
        assert!(!before.workspaces.is_empty());
        assert!(!before.structured_agents.is_empty());
        assert_eq!(after.node_id, before.node_id);
        assert_eq!(after.last_ok, before.last_ok);
        assert_eq!(after.system, before.system);
        assert_eq!(after.version, before.version);
        assert_eq!(after.icon, before.icon);
        assert_eq!(after.workspaces, before.workspaces);
        assert_eq!(after.structured_agents, before.structured_agents);
        assert_eq!(
            after
                .error
                .as_deref()
                .map(|error| crate::peers::SshFailureReason::classify(error).as_str()),
            Some("auth_refused")
        );
    });
}
#[test]
fn a_missing_home_sample_never_reuses_previous_health_as_current() {
    let _guard = crate::config::test_config_env_guard();
    runtime(async {
        use crate::structured::fleet_lite::{Provider, Row, Store, StructuredAgent};
        struct Previous(Row);
        impl Provider for Previous {
            fn agents(&self) -> std::io::Result<Vec<StructuredAgent<'_>>> {
                Ok(Vec::new())
            }
            fn rows(&self) -> std::io::Result<Vec<Row>> {
                Ok(vec![self.0.clone()])
            }
        }
        let mut case = Case::new().await;
        let home = case.app.node_id.clone().unwrap();
        assert!(case.app.state.system_stats.is_none());
        let mut old = crate::structured::fleet_lite::local_node(&home);
        old.health = Some(serde_json::from_value(json!({"cpu_percent":91})).unwrap());
        let mut store = Store::open(case.server._root.path().join("fleet.json")).unwrap();
        store.refresh(&Previous(Row::Node(old))).unwrap();
        drop(store);
        let row = case.snapshot().await[&home].clone();
        case.server.finish().await;
        assert!(
            row["health"].is_null(),
            "an absent sampler cannot publish old health as current"
        );
    });
}
#[test]
fn thermal_labels_cannot_disclose_a_hostname_at_read_scope() {
    let _guard = crate::config::test_config_env_guard();
    runtime(async {
        let mut case = Case::new().await;
        let owner = NodeIdentity::fixture([94; 32]);
        let id = owner.node_id();
        let peer = case.peer("thermal.example");
        crate::peer_stream::test_enroll(&peer, &id);
        case.direct(&owner);
        case.success(&peer,&id,json!({"system":{"thermal":{"severity":3,"component":"gpu","label":"private.example"}}}),1);
        let row = case.snapshot().await[&id].clone();
        case.server.finish().await;
        assert_eq!(row["health"]["thermal"]["severity"], 3);
        assert_eq!(row["health"]["thermal"]["component"], "gpu");
        assert!(
            row["health"]["thermal"].get("label").is_none(),
            "host-authored detail is not read-scoped metadata"
        );
    });
}
#[test]
fn stamp_cache_drops_rebound_and_absent_owners() {
    let _guard = crate::config::test_config_env_guard();
    runtime(async {
        let mut case = Case::new().await;
        let a = NodeIdentity::fixture([95; 32]);
        let b = NodeIdentity::fixture([96; 32]);
        let peer = case.peer("stamp.example");
        crate::peer_stream::test_enroll(&peer, &a.node_id());
        case.direct(&a);
        case.success(&peer, &a.node_id(), json!({}), 1);
        case.service();
        assert_eq!(case.server.core.fleet_nodes.stamp_count(), 1);
        crate::peer_stream::test_enroll(&peer, &b.node_id());
        case.direct(&b);
        case.fail(&peer, "Permission denied (publickey)");
        case.service();
        assert_eq!(case.server.core.fleet_nodes.stamp_count(), 0);
        case.success(&peer, &b.node_id(), json!({}), 1);
        case.service();
        assert_eq!(case.server.core.fleet_nodes.stamp_count(), 1);
        case.app.state.peer_summaries.clear();
        case.app.state.peers.clear();
        case.app.mesh_routes.table.withdraw_edge(&b.node_id());
        case.service();
        assert_eq!(case.server.core.fleet_nodes.stamp_count(), 0);
        case.server.finish().await;
    });
}
