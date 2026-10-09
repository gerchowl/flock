//! Route discovery and withdrawal over real isolated one-way SSH edges.
mod support;
use serde_json::{json, Value};
use std::time::Duration;
use support::fleet::{self, Fleet, Node, NodeSpec};

const PAIR: &[NodeSpec] = &[
    NodeSpec::new("nodea", "routes-a", &["nodeb"]),
    NodeSpec::new("nodeb", "routes-b", &[]),
];
const HUB: &[NodeSpec] = &[
    NodeSpec::new("nodeb", "routes-hub", &["nodea", "nodec"]),
    NodeSpec::new("nodea", "routes-spoke-a", &[]),
    NodeSpec::new("nodec", "routes-spoke-c", &[]),
];
fn api(node: &Node, method: &str, params: Value) -> Value {
    serde_json::from_str(
        &node.api(&json!({"id":"routes", "method":method, "params":params}).to_string()),
    )
    .unwrap()
}
fn id(node: &Node) -> String {
    api(node, "peers.summary", json!({}))["result"]["node_id"]
        .as_str()
        .unwrap()
        .into()
}
fn routes(node: &Node) -> Vec<Value> {
    api(node, "peers.enrollment", json!({}))["result"]["routes"]
        .as_array()
        .unwrap()
        .clone()
}
fn route(node: &Node, target: &str) -> Value {
    fleet::wait_until("mesh route", Duration::from_secs(45), || {
        routes(node).into_iter().find(|r| r["node"] == target)
    })
}

#[test]
fn quiet_healthy_edge_keeps_routes() {
    let fleet = fleet::spawn("routes-quiet", PAIR);
    let a = fleet.node("nodea");
    let b = fleet.node("nodeb");
    let target = id(b);
    route(a, &target);
    route(b, &id(a));
    // The fixture polls each second. No topology changes during eight intervals.
    std::thread::sleep(Duration::from_secs(8));
    assert!(routes(a).iter().any(|r| r["node"] == target));
    assert!(routes(b).iter().any(|r| r["node"] == id(a)));
}

#[test]
fn closed_edge_withdraws_routes_on_both_sides() {
    let fleet = fleet::spawn("routes-close", PAIR);
    let a = fleet.node("nodea");
    let b = fleet.node("nodeb");
    route(a, &id(b));
    route(b, &id(a));
    fleet.refuse_ssh_to("nodeb");
    fleet.kill_edge("nodea", "nodeb", Duration::from_secs(5));
    fleet::wait_until("both sides withdraw", Duration::from_secs(20), || {
        (routes(a).is_empty() && routes(b).is_empty()).then_some(())
    });
}

#[test]
fn routes_learned_over_an_inbound_edge_are_usable_by_the_spoke() {
    let fleet = fleet::spawn("routes-inbound", HUB);
    let a = fleet.node("nodea");
    let b = fleet.node("nodeb");
    let c = fleet.node("nodec");
    let found = route(a, &id(c));
    assert_eq!(found["next_hop"], id(b));
    assert_eq!(found["hops"], 2);
    assert!(found["name"].as_str().unwrap().ends_with("(advertised)"));
    assert_eq!(route(c, &id(a))["next_hop"], id(b));
}

// Fault injection edits this fixture's copy of the SSH shim before any process
// starts. All enrollment and route handling still runs through the real relay.
fn fault_fleet(tag: &str, source: &str) -> Fleet {
    fleet::spawn_with_startup_probe(tag, PAIR, |fleet, name| {
        if name != "nodea" {
            return;
        }
        let base = fleet.node(name).home.parent().unwrap();
        let shim = base.join("bin/ssh");
        let text = std::fs::read_to_string(&shim).unwrap();
        let marker = "        response = json.loads(line)\n";
        assert!(text.contains(marker));
        std::fs::write(shim, text.replace(marker, &format!("{marker}{source}"))).unwrap();
    })
}

#[test]
fn forged_route_from_neighbor_is_rejected() {
    let fleet = fault_fleet(
        "routes-forged",
        r#"        adverts = response.get("result", {}).get("adverts")
        if adverts:
            forged = dict(adverts[0])
            forged["node_id"] = "0" * 64
            forged["name"] = "forged.test"
            adverts.insert(0, forged)
            adverts.insert(0, {"node_id": "undecodable"})
            line = json.dumps(response) + "\n"
            (base / "forged-route-sent").write_text("yes")
"#,
    );
    let a = fleet.node("nodea");
    let b = fleet.node("nodeb");
    route(a, &id(b));
    assert!(a.home.parent().unwrap().join("forged-route-sent").exists());
    assert!(routes(a).iter().all(|r| r["node"] != "0".repeat(64)));
    // A bad first record must not poison the valid neighbor advert after it.
    assert_eq!(route(a, &id(b))["hops"], 1);
}

#[test]
fn summary_node_id_must_match_pinned_edge() {
    let fleet = fault_fleet(
        "routes-summary",
        r#"        result = response.get("result", {})
        if "workspaces" in result and "node_id" in result:
            result["node_id"] = "0" * 64
            line = json.dumps(response) + "\n"
            (base / "forged-summary-sent").write_text("yes")
"#,
    );
    let a = fleet.node("nodea");
    route(a, &id(fleet.node("nodeb")));
    fleet::wait_until(
        "summary received without forged identity",
        Duration::from_secs(10),
        || {
            let summary = api(a, "peers.summary", json!({}));
            let rows = summary["result"]["relayed_fleet"].as_array()?;
            let row = rows
                .iter()
                .find(|r| r["name"] == "nodeb" && r["age_secs"].is_number())?;
            (row.get("node_id").is_none() && a.home.parent()?.join("forged-summary-sent").exists())
                .then_some(())
        },
    );
}

#[test]
fn agent_owner_hint_survives_restart_as_offline() {
    let mut fleet = fleet::spawn("routes-owner", PAIR);
    let b = fleet.node("nodeb");
    let started = api(
        b,
        "agent.start",
        json!({"name":"route-owner", "argv":["/bin/sh"], "cwd":b.repo}),
    );
    let agent = started["result"]["agent"]["agent_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let a = fleet.node("nodea");
    let db = fleet::wait_until("owner hint committed", Duration::from_secs(20), || {
        ["flock", "flock-dev"].into_iter().find_map(|app| {
            let path = a.home.join("state").join(app).join("mesh-mail.sqlite");
            let db = rusqlite::Connection::open_with_flags(
                &path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .ok()?;
            let node: String = db
                .query_row(
                    "SELECT node_id FROM agent_owners WHERE agent_id=?1",
                    [&agent],
                    |r| r.get(0),
                )
                .ok()?;
            (node == id(b)).then_some(path)
        })
    });
    fleet.refuse_ssh_to("nodeb");
    fleet.node_mut("nodea").restart();
    assert!(routes(fleet.node("nodea")).is_empty());
    let connection =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT node_id FROM agent_owners WHERE agent_id=?1",
                [&agent],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        id(fleet.node("nodeb"))
    );
    // The public resolver finds the retained owner and queues for its configured
    // edge, despite having no live route or freshly polled directory.
    let response = api(
        fleet.node("nodea"),
        "msg.send",
        json!({"to":{"type":"agent","agent":agent}, "from_agent":"agent_nodea_routeprobe", "body":"offline owner probe"}),
    );
    assert_eq!(response["result"]["state"], "queued", "{response}");
    assert_eq!(response["result"]["to_host"], "nodeb");
}

#[test]
fn topology_change_on_acceptor_wakes_its_dialers() {
    let specs = [
        NodeSpec::new("nodea", "routes-wake-a", &["nodeb"]),
        NodeSpec::new("nodeb", "routes-wake-b", &[]),
        NodeSpec::new("nodec", "routes-wake-c", &["nodeb"]),
    ];
    let fleet = fleet::spawn_with_startup_probe("routes-wake", &specs, |fleet, name| {
        if name == "nodec" {
            fleet.refuse_edge("nodec", "nodeb");
        }
    });
    let a = fleet.node("nodea");
    let b = fleet.node("nodeb");
    let c = fleet.node("nodec");
    route(a, &id(b));
    // Let the initial exchange converge before changing only the acceptor.
    std::thread::sleep(Duration::from_secs(4));
    assert!(routes(a).iter().all(|r| r["node"] != id(c)));
    fleet.allow_edge("nodec", "nodeb");
    let found = route(a, &id(c));
    assert_eq!(found["next_hop"], id(b));
    assert_eq!(found["hops"], 2);
}
