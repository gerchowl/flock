//! Requests traverse authenticated custody hops in isolated real servers.
mod support;
use serde_json::{json, Value};
use std::time::Duration;
use support::fleet::{self, Fleet, Node, NodeSpec};

const CHAIN: &[NodeSpec] = &[
    NodeSpec::new("nodea", "forward-a", &["nodeb"]),
    NodeSpec::new("nodeb", "forward-b", &["nodec"]),
    NodeSpec::new("nodec", "forward-c", &[]),
];
const HUB: &[NodeSpec] = &[
    NodeSpec::new("nodeb", "forward-hub", &["nodea", "nodec"]),
    NodeSpec::new("nodea", "forward-spoke-a", &[]),
    NodeSpec::new("nodec", "forward-spoke-c", &[]),
];
const WAIT: Duration = Duration::from_secs(40);
fn api(node: &Node, method: &str, params: Value) -> Value {
    serde_json::from_str(
        &node.api(&json!({"id":"forward", "method":method, "params":params}).to_string()),
    )
    .unwrap()
}
fn db(node: &Node) -> rusqlite::Connection {
    let connection =
        rusqlite::Connection::open(node.home.join("state/flock-dev/mesh-mail.sqlite")).unwrap();
    connection.busy_timeout(Duration::from_secs(5)).unwrap();
    connection
}
fn setup(tag: &str, specs: &[NodeSpec]) -> (Fleet, Value, Value) {
    prepare(tag, specs, false)
}
fn prepare(tag: &str, specs: &[NodeSpec], retained_owner: bool) -> (Fleet, Value, Value) {
    let fleet = fleet::spawn(tag, specs);
    let mut agents = Vec::new();
    for name in ["nodea", "nodec"] {
        let node = fleet.node(name);
        let result = api(
            node,
            "agent.start",
            json!({"name":"forward", "argv":["/bin/sh"], "cwd":node.repo}),
        );
        assert!(result.get("error").is_none(), "{result}");
        agents.push(result["result"]["agent"].clone());
    }
    fleet.wait_route("nodea", "nodec", true);
    if retained_owner {
        // This orientation lies outside the display gossip horizon. Seed a
        // retained owner hint, as if this agent was discovered before reconnect.
        db(fleet.node("nodea"))
            .execute(
                "INSERT INTO agent_owners VALUES(?1,?2,'nodec',0)",
                rusqlite::params![
                    agents[1]["agent_id"].as_str().unwrap(),
                    fleet.node_id("nodec")
                ],
            )
            .unwrap();
        return (fleet, agents.remove(0), agents.remove(0));
    }
    fleet::wait_until("owner discovery", WAIT, || {
        let listing = api(fleet.node("nodea"), "agent.list", json!({}));
        listing["result"]["fleet"]
            .as_array()?
            .iter()
            .any(|row| row["agent_id"] == agents[1]["agent_id"])
            .then_some(())
    });
    (fleet, agents.remove(0), agents.remove(0))
}
fn send(fleet: &Fleet, sender: &Value, recipient: &Value, correlation: &str) -> Value {
    api(
        fleet.node("nodea"),
        "msg.send",
        json!({
            "to":{"type":"agent", "agent":recipient["agent_id"]},
            "from_agent":sender["agent_id"], "body":"through custody", "intent":"fyi", "correlation_id":correlation
        }),
    )
}
fn read_once(fleet: &Fleet, recipient: &Value, correlation: &str) {
    let node = fleet.node("nodec");
    let messages = fleet::wait_until("exactly one forwarded message", WAIT, || {
        let response = api(node, "msg.read", json!({"pane":recipient["pane_id"]}));
        let messages = response["result"]["messages"].as_array()?;
        (!messages.is_empty()).then_some(messages.clone())
    });
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0]["correlation_id"], correlation);
    assert_eq!(messages[0]["body"], "through custody");
    assert_eq!(
        api(node, "msg.read", json!({"pane":recipient["pane_id"]}))["result"]["messages"],
        json!([])
    );
}
fn row_state(node: &Node, correlation: &str, expected: &str) {
    fleet::wait_until("custody state", WAIT, || {
        let state: String = db(node)
            .query_row(
                "SELECT state FROM envelopes WHERE correlation=?1",
                [correlation],
                |r| r.get(0),
            )
            .ok()?;
        (state == expected).then_some(())
    });
}
fn due(node: &Node) {
    db(node)
        .execute(
            "UPDATE envelopes SET retry_at=0,lease_until=0 WHERE state IN ('custody','held')",
            [],
        )
        .unwrap();
}

#[test]
fn chain_request_is_forwarded_and_imported_once() {
    let (fleet, sender, recipient) = setup("forward-chain", CHAIN);
    let response = send(&fleet, &sender, &recipient, "chain");
    assert_eq!(response["result"]["state"], "queued", "{response}");
    assert_eq!(response["result"]["path"], "via nodeb");
    read_once(&fleet, &recipient, "chain");
    row_state(fleet.node("nodea"), "chain", "transferred");
    let body: Vec<u8> = db(fleet.node("nodea"))
        .query_row(
            "SELECT body FROM envelopes WHERE correlation='chain'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        !body.is_empty(),
        "origin retains its body after custody transfer"
    );
    row_state(fleet.node("nodeb"), "chain", "delivered");
    let body: Vec<u8> = db(fleet.node("nodeb"))
        .query_row(
            "SELECT body FROM envelopes WHERE correlation='chain'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(body.is_empty(), "forwarder deletes its transferred body");
}

#[test]
fn spoke_without_outbound_edges_reaches_a_third_node_through_hub() {
    let (fleet, sender, recipient) = setup("forward-spoke", HUB);
    let response = send(&fleet, &sender, &recipient, "spoke");
    assert_eq!(response["result"]["state"], "queued", "{response}");
    read_once(&fleet, &recipient, "spoke");
    row_state(fleet.node("nodea"), "spoke", "transferred");
}

#[test]
fn allow_from_checks_origin_not_allowed_forwarding_hub() {
    let (fleet, sender, recipient) = setup(
        "forward-policy",
        &[
            CHAIN[0].clone(),
            CHAIN[1].clone(),
            CHAIN[2]
                .clone()
                .with_config("[msg]\nallow_from = [\"nodeb\"]\n"),
        ],
    );
    let response = send(&fleet, &sender, &recipient, "policy");
    assert!(response.get("error").is_none(), "{response}");
    row_state(fleet.node("nodeb"), "policy", "refused");
    let reason: String = db(fleet.node("nodeb"))
        .query_row(
            "SELECT collect_error FROM envelopes WHERE correlation='policy'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reason, "msg_not_allowed");
    assert_eq!(
        api(
            fleet.node("nodec"),
            "msg.read",
            json!({"pane":recipient["pane_id"]})
        )["result"]["messages"],
        json!([])
    );
}

#[test]
fn pause_at_forwarder_refuses_transiently_and_origin_retains() {
    let (fleet, sender, recipient) = setup("forward-pause", CHAIN);
    let response = api(fleet.node("nodeb"), "fleet.pause", json!({}));
    assert!(response.get("error").is_none(), "{response}");
    let response = send(&fleet, &sender, &recipient, "paused");
    assert_eq!(response["result"]["state"], "queued", "{response}");
    row_state(fleet.node("nodea"), "paused", "custody");
    let response = api(fleet.node("nodeb"), "fleet.resume", json!({}));
    assert!(response.get("error").is_none(), "{response}");
    due(fleet.node("nodea"));
    read_once(&fleet, &recipient, "paused");
}

#[test]
fn forwarder_restart_resumes_custody_once() {
    let (mut fleet, sender, recipient) = setup("forward-restart", CHAIN);
    let block = fleet.base.join("transient-delivery-nodeb-nodec");
    std::fs::write(&block, "").unwrap();
    send(&fleet, &sender, &recipient, "restart");
    row_state(fleet.node("nodeb"), "restart", "custody");
    fleet.node_mut("nodeb").restart();
    std::fs::remove_file(block).unwrap();
    due(fleet.node("nodeb"));
    read_once(&fleet, &recipient, "restart");
}

#[test]
fn known_offline_owner_is_queued_unknown_target_refused() {
    let (fleet, sender, recipient) = setup("forward-offline", CHAIN);
    fleet.refuse_ssh_to("nodeb");
    fleet.kill_edge("nodea", "nodeb", WAIT);
    fleet.wait_route("nodea", "nodec", false);
    let response = send(&fleet, &sender, &recipient, "offline");
    assert_eq!(response["result"]["state"], "queued", "{response}");
    let next: String = db(fleet.node("nodea"))
        .query_row(
            "SELECT next_hop FROM envelopes WHERE correlation='offline'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(next.is_empty());
    let response = send(
        &fleet,
        &sender,
        &json!({"agent_id":"agent_unknown.example_missing"}),
        "unknown",
    );
    assert_eq!(
        response["error"]["code"], "msg_target_not_found",
        "{response}"
    );
}

#[test]
fn one_undecodable_record_does_not_stall_the_retry_pass() {
    let (fleet, sender, recipient) = setup("forward-corrupt", CHAIN);
    let block = fleet.base.join("transient-delivery-nodeb-nodec");
    std::fs::write(&block, "").unwrap();
    send(&fleet, &sender, &recipient, "broken");
    send(&fleet, &sender, &recipient, "healthy");
    row_state(fleet.node("nodeb"), "healthy", "custody");
    db(fleet.node("nodeb"))
        .execute(
            "UPDATE envelopes SET metadata='invalid JSON' WHERE correlation='broken'",
            [],
        )
        .unwrap();
    std::fs::remove_file(block).unwrap();
    due(fleet.node("nodeb"));
    read_once(&fleet, &recipient, "healthy");
    row_state(fleet.node("nodeb"), "broken", "quarantined");
}

#[test]
fn idle_forwarder_makes_no_commits() {
    let (fleet, sender, recipient) = setup("forward-idle", CHAIN);
    send(&fleet, &sender, &recipient, "idle");
    read_once(&fleet, &recipient, "idle");
    row_state(fleet.node("nodeb"), "idle", "delivered");
    let connection = db(fleet.node("nodeb"));
    // Let startup topology exchange finish before measuring custody's idle ticks.
    std::thread::sleep(Duration::from_secs(3));
    let version: i64 = connection
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .unwrap();
    std::thread::sleep(Duration::from_secs(3));
    let after: i64 = connection
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(after, version, "idle app passes committed store changes");
}

#[test]
fn incompatible_next_hop_refuses_new_acceptance_existing_custody_kept() {
    let (mut fleet, sender, _) = setup("forward-incompatible", CHAIN);
    let recipient = api(
        fleet.node("nodeb"),
        "agent.start",
        json!({"name":"compatibility", "argv":["/bin/sh"], "cwd":fleet.node("nodeb").repo}),
    )["result"]["agent"]
        .clone();
    fleet::wait_until("direct owner discovery", WAIT, || {
        api(fleet.node("nodea"), "agent.list", json!({}))["result"]["fleet"]
            .as_array()?
            .iter()
            .any(|row| row["agent_id"] == recipient["agent_id"])
            .then_some(())
    });
    std::fs::write(fleet.base.join("transient-delivery-nodea-nodeb"), "").unwrap();
    assert_eq!(
        send(&fleet, &sender, &recipient, "existing")["result"]["state"],
        "queued"
    );
    fleet
        .node_mut("nodeb")
        .restart_with_mesh(fleet::MeshMode::VersionMismatch(0));
    fleet::wait_until("incompatible next hop", WAIT, || {
        api(fleet.node("nodea"), "peers.enrollment", json!({}))["result"]["peers"]
            .as_array()?
            .iter()
            .any(|p| p["state"] == "refused")
            .then_some(())
    });
    // Refusal is known even after the live route is withdrawn.
    let response = api(
        fleet.node("nodea"),
        "msg.send",
        json!({
            "to":{"type":"agent", "agent":recipient["agent_id"]}, "from_agent":sender["agent_id"],
            "body":"new request", "intent":"fyi", "correlation_id":"new"
        }),
    );
    assert_eq!(response["error"]["code"], "peer_incompatible", "{response}");
    assert!(response["error"]["message"]
        .as_str()
        .unwrap()
        .contains("upgrade flk on nodeb"));
    row_state(fleet.node("nodea"), "existing", "custody");
}

fn reroute_after_refusal(tag: &str, looped: bool) {
    let (fleet, sender, recipient) = setup(tag, CHAIN);
    let capture = fleet.base.join("capture-delivery-nodea-nodeb");
    std::fs::write(&capture, "").unwrap();
    send(&fleet, &sender, &recipient, "reroute");
    let mut delivery: Value = serde_json::from_slice(&std::fs::read(&capture).unwrap()).unwrap();
    if looped {
        delivery["visited"] = json!([
            fleet.node_id("nodea"),
            fleet.node_id("nodeb"),
            fleet.node_id("nodea")
        ]);
    } else {
        delivery["hops_left"] = json!(0);
    }
    let replay = fleet.base.join("replay-delivery-nodea-nodeb");
    std::fs::write(&replay, serde_json::to_vec(&delivery).unwrap()).unwrap();
    std::fs::remove_file(capture).unwrap();
    due(fleet.node("nodea"));
    fleet::wait_until("route refusal retains unrouted custody", WAIT, || {
        let row: (String, String) = db(fleet.node("nodea"))
            .query_row(
                "SELECT state,next_hop FROM envelopes WHERE correlation='reroute'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok()?;
        (row == ("custody".into(), String::new())).then_some(())
    });
    let attempts = fleet.base.join("delivery-attempts-nodea-nodeb");
    let before = std::fs::read_to_string(&attempts).unwrap().lines().count();
    due(fleet.node("nodea"));
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        std::fs::read_to_string(&attempts).unwrap().lines().count(),
        before,
        "same route retried without a generation change"
    );
    std::fs::remove_file(replay).unwrap();
    // Withdraw and restore a live edge without resetting app generation state.
    fleet.refuse_edge("nodeb", "nodec");
    fleet.kill_edge("nodeb", "nodec", WAIT);
    fleet.wait_route("nodea", "nodec", false);
    fleet.allow_edge("nodeb", "nodec");
    fleet.wait_route("nodea", "nodec", true);
    read_once(&fleet, &recipient, "reroute");
}

#[test]
fn hop_budget_exhausted_retains_custody_and_reroutes_on_route_change() {
    reroute_after_refusal("forward-hops", false);
}

#[test]
fn route_cycle_is_loop_detected_and_custody_retained() {
    reroute_after_refusal("forward-cycle", true);
}

#[test]
fn slow_next_hop_does_not_starve_user_sends_to_another_peer() {
    let (fleet, sender, recipient) = setup(
        "forward-slow",
        &[
            NodeSpec::new("nodea", "slow-a", &["nodeb", "nodec"]),
            NodeSpec::new("nodeb", "slow-b", &[]),
            NodeSpec::new("nodec", "slow-c", &[]),
        ],
    );
    let other = api(
        fleet.node("nodeb"),
        "agent.start",
        json!({"name":"slow", "argv":["/bin/sh"], "cwd":fleet.node("nodeb").repo}),
    )["result"]["agent"]
        .clone();
    fleet::wait_until("slow owner discovery", WAIT, || {
        api(fleet.node("nodea"), "agent.list", json!({}))["result"]["fleet"]
            .as_array()?
            .iter()
            .any(|row| row["agent_id"] == other["agent_id"])
            .then_some(())
    });
    let transient = fleet.base.join("transient-delivery-nodea-nodeb");
    std::fs::write(&transient, "").unwrap();
    for i in 0..5 {
        assert_eq!(
            send(&fleet, &sender, &other, &format!("slow-{i}"))["result"]["state"],
            "queued"
        );
    }
    let gate = fleet.gate_message_edge("nodea", "nodeb");
    std::fs::remove_file(transient).unwrap();
    due(fleet.node("nodea"));
    gate.wait_entered(WAIT);
    let start = std::time::Instant::now();
    let response = send(&fleet, &sender, &recipient, "fast");
    assert_eq!(response["result"]["state"], "delivered", "{response}");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "user send waited on the slow peer"
    );
    gate.release();
    read_once(&fleet, &recipient, "fast");
}

#[test]
fn forwarded_request_is_collected_over_inbound_only_next_hop() {
    let (fleet, sender, recipient) = prepare(
        "forward-held",
        &[
            NodeSpec::new("nodea", "held-a", &["nodeb"]),
            NodeSpec::new("nodeb", "held-b", &[]),
            NodeSpec::new("nodec", "held-c", &["nodeb"]),
        ],
        true,
    );
    let response = send(&fleet, &sender, &recipient, "forwarded-held");
    assert_eq!(response["result"]["state"], "queued", "{response}");
    read_once(&fleet, &recipient, "forwarded-held");
    row_state(fleet.node("nodeb"), "forwarded-held", "delivered");
}

#[test]
fn offline_requests_do_not_starve_a_live_route_generation() {
    let (fleet, sender, offline) = setup(
        "forward-window",
        &[
            NodeSpec::new("nodea", "window-a", &["nodeb", "nodec"]),
            NodeSpec::new("nodeb", "window-b", &[]),
            NodeSpec::new("nodec", "window-c", &[]),
        ],
    );
    let recipient = api(
        fleet.node("nodeb"),
        "agent.start",
        json!({
            "name":"window", "argv":["/bin/sh"], "cwd":fleet.node("nodeb").repo
        }),
    )["result"]["agent"]
        .clone();
    fleet.wait_route("nodea", "nodeb", true);
    fleet::wait_until("second owner discovery", WAIT, || {
        api(fleet.node("nodea"), "agent.list", json!({}))["result"]["fleet"]
            .as_array()?
            .iter()
            .any(|row| row["agent_id"] == recipient["agent_id"])
            .then_some(())
    });
    for target in ["nodeb", "nodec"] {
        fleet.refuse_edge("nodea", target);
        fleet.kill_edge("nodea", target, WAIT);
        fleet.wait_route("nodea", target, false);
    }
    for index in 0..100 {
        let response = send(&fleet, &sender, &offline, &format!("offline-{index}"));
        assert_eq!(response["result"]["state"], "queued", "{response}");
        assert_eq!(response["result"]["path"], "queued");
    }
    assert_eq!(
        send(&fleet, &sender, &recipient, "later")["result"]["state"],
        "queued"
    );
    fleet.allow_edge("nodea", "nodeb");
    fleet.wait_route("nodea", "nodeb", true);
    fleet::wait_until("later request routed live", WAIT, || {
        let response = api(
            fleet.node("nodeb"),
            "msg.read",
            json!({"pane":recipient["pane_id"]}),
        );
        response["result"]["messages"]
            .as_array()?
            .iter()
            .any(|message| message["correlation_id"] == "later")
            .then_some(())
    });
    let queued: i64 = db(fleet.node("nodea")).query_row(
        "SELECT count(*) FROM envelopes WHERE correlation LIKE 'offline-%' AND state='custody' AND next_hop=''",
        [], |r| r.get(0)).unwrap();
    assert_eq!(queued, 100);
}

#[test]
fn configured_origin_name_is_accepted_by_forwarded_policy() {
    let (fleet, sender, recipient) = setup(
        "forward-allowed",
        &[
            CHAIN[0].clone(),
            CHAIN[1].clone(),
            NodeSpec::new("nodec", "allowed-c", &["nodea"])
                .with_config("[msg]\nallow_from = [\"nodea\"]\n"),
        ],
    );
    // C knows the origin by its configured pin, but A still sends via B.
    fleet.wait_route("nodec", "nodea", true);
    fleet.refuse_edge("nodec", "nodea");
    fleet.kill_edge("nodec", "nodea", WAIT);
    fleet::wait_until("forwarded route after direct withdrawal", WAIT, || {
        let enrollment = api(fleet.node("nodea"), "peers.enrollment", json!({}));
        enrollment["result"]["routes"]
            .as_array()?
            .iter()
            .any(|route| route["node"] == fleet.node_id("nodec") && route["hops"] == 2)
            .then_some(())
    });
    let response = send(&fleet, &sender, &recipient, "allowed");
    assert_eq!(response["result"]["path"], "via nodeb", "{response}");
    read_once(&fleet, &recipient, "allowed");
}
