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
    remote_state(&fleet, "chain", "read");
    let body: Vec<u8> = db(fleet.node("nodea"))
        .query_row(
            "SELECT body FROM envelopes WHERE correlation='chain'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        body.is_empty(),
        "origin releases its body after final delivery receipt"
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
    // Custody precedes the first downstream result. Wait for both blocked
    // attempts to finish so a late result cannot overwrite the forced retry.
    fleet::wait_until("both initial attempts backed off", WAIT, || {
        let completed: i64 = db(fleet.node("nodeb"))
            .query_row(
                "SELECT count(*) FROM envelopes WHERE correlation IN ('broken','healthy')
             AND retry_at > (SELECT elapsed FROM clock WHERE singleton=1)",
                [],
                |r| r.get(0),
            )
            .ok()?;
        (completed == 2).then_some(())
    });
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

fn audit_store_writes(connection: &rusqlite::Connection) {
    connection.execute_batch(
        "CREATE TABLE idle_write_audit (table_name TEXT, column_name TEXT, row_key TEXT, old_value TEXT, new_value TEXT)"
    ).unwrap();
    let tables: Vec<String> = connection.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name!='idle_write_audit'"
    ).unwrap().query_map([], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
    for table in tables {
        let columns: Vec<String> = connection
            .prepare("SELECT name FROM pragma_table_info(?1)")
            .unwrap()
            .query_map([&table], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for column in columns {
            let key = if table == "envelopes" {
                "NEW.correlation"
            } else {
                "''"
            };
            connection.execute_batch(&format!(
                "CREATE TRIGGER audit_{table}_{column} AFTER UPDATE OF \"{column}\" ON \"{table}\"
                 WHEN OLD.\"{column}\" IS NOT NEW.\"{column}\"
                 BEGIN INSERT INTO idle_write_audit VALUES ('{table}','{column}',{key},quote(OLD.\"{column}\"),quote(NEW.\"{column}\")); END"
            )).unwrap();
        }
        for operation in ["INSERT", "DELETE"] {
            let row = if operation == "INSERT" { "NEW" } else { "OLD" };
            let key = if table == "envelopes" {
                format!("{row}.correlation")
            } else {
                "''".into()
            };
            connection.execute_batch(&format!(
                "CREATE TRIGGER audit_{table}_{operation} AFTER {operation} ON \"{table}\"
                 BEGIN INSERT INTO idle_write_audit VALUES ('{table}','{operation}',{key},NULL,NULL); END"
            )).unwrap();
        }
    }
}

fn audited_writes(connection: &rusqlite::Connection, after: i64) -> Vec<String> {
    connection.prepare(
        "SELECT table_name || '.' || column_name || ' [' || row_key || '] ' || COALESCE(old_value,'') || ' -> ' || COALESCE(new_value,'') FROM idle_write_audit WHERE rowid>?1 ORDER BY rowid"
    ).unwrap().query_map([after], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
}

#[test]
fn idle_forwarder_makes_no_commits() {
    let (fleet, sender, recipient) = setup("forward-idle", CHAIN);
    let connection = db(fleet.node("nodeb"));
    audit_store_writes(&connection);
    send(&fleet, &sender, &recipient, "idle");
    read_once(&fleet, &recipient, "idle");
    row_state(fleet.node("nodeb"), "idle", "delivered");
    // Reading the recipient inbox does not settle the return receipts.
    remote_state(&fleet, "idle", "read");
    fleet::wait_until("both receipt custody legs acknowledged", WAIT, || {
        let forwarded: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM envelopes WHERE kind='receipt' AND correlation LIKE '%:read' AND state='delivered' AND delivered=1 AND length(body)=0)
             AND NOT EXISTS(SELECT 1 FROM envelopes WHERE state IN ('custody','held'))",
            [], |row| row.get(0),
        ).unwrap();
        // The inbox and its receipt_sent mark belong to C, not forwarding B.
        let returned: bool = db(fleet.node("nodec")).query_row(
            "SELECT EXISTS(SELECT 1 FROM envelopes WHERE correlation='idle' AND state='read' AND receipt_sent='read')
             AND NOT EXISTS(SELECT 1 FROM envelopes WHERE kind='receipt' AND state IN ('custody','held'))",
            [], |row| row.get(0),
        ).unwrap();
        (forwarded && returned).then_some(())
    });
    // Observe fresh summaries over the enrolled edges after the acknowledgements.
    // No node should still be advertising custody that wakes its collector.
    for edge in ["nodea-nodeb", "nodeb-nodec"] {
        std::fs::write(fleet.base.join(format!("observe-summary-{edge}")), "").unwrap();
    }
    fleet::wait_until(
        "no pending outbound wake on either return edge",
        WAIT,
        || {
            for edge in ["nodea-nodeb", "nodeb-nodec"] {
                let summary: Value = serde_json::from_slice(
                    &std::fs::read(fleet.base.join(format!("observed-summary-{edge}"))).ok()?,
                )
                .ok()?;
                if summary["outbound_pending"] != false {
                    return None;
                }
            }
            Some(())
        },
    );
    let audit_start: i64 = connection
        .query_row(
            "SELECT COALESCE(MAX(rowid),0) FROM idle_write_audit",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let version: i64 = connection
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .unwrap();
    std::thread::sleep(Duration::from_secs(6));
    let after: i64 = connection
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        after,
        version,
        "idle app passes committed store changes: {:?}",
        audited_writes(&connection, audit_start)
    );
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
        .restart_with_mesh(fleet::MeshMode::VersionMismatch(5));
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

fn reply(fleet: &Fleet, correlation: &str, body: &str) {
    let response = api(
        fleet.node("nodec"),
        "msg.reply",
        json!({"correlation_id":correlation,"body":body}),
    );
    assert!(response.get("error").is_none(), "{response}");
}
fn read_answer(fleet: &Fleet, sender: &Value, body: &str) {
    let messages = fleet::wait_until("routed answer", WAIT, || {
        let response = api(
            fleet.node("nodea"),
            "msg.read",
            json!({"pane":sender["pane_id"]}),
        );
        let messages = response["result"]["messages"].as_array()?;
        (!messages.is_empty()).then_some(messages.clone())
    });
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0]["body"], body);
    assert_eq!(
        api(
            fleet.node("nodea"),
            "msg.read",
            json!({"pane":sender["pane_id"]})
        )["result"]["messages"],
        json!([])
    );
}
fn remote_state(fleet: &Fleet, correlation: &str, expected: &str) {
    fleet::wait_until("routed receipt", WAIT, || {
        let response = api(
            fleet.node("nodea"),
            "msg.status",
            json!({"correlation_id":correlation}),
        );
        (response["result"]["state"] == expected).then_some(())
    });
}

#[test]
fn answer_routes_back_across_two_hops_and_imports_once() {
    let (fleet, sender, recipient) = setup("routed-answer", CHAIN);
    send(&fleet, &sender, &recipient, "routed");
    read_once(&fleet, &recipient, "routed");
    reply(&fleet, "routed", "answer across two hops");
    read_answer(&fleet, &sender, "answer across two hops");
}

#[test]
fn delivered_and_read_receipts_reach_origin_across_hops() {
    let (fleet, sender, recipient) = setup("routed-receipts", CHAIN);
    send(&fleet, &sender, &recipient, "receipts");
    remote_state(&fleet, "receipts", "delivered");
    read_once(&fleet, &recipient, "receipts");
    remote_state(&fleet, "receipts", "read");
}

#[test]
fn answer_held_while_origin_unroutable_moves_when_route_appears() {
    let (fleet, sender, recipient) = setup("routed-offline", CHAIN);
    send(&fleet, &sender, &recipient, "offline-answer");
    read_once(&fleet, &recipient, "offline-answer");
    fleet.refuse_edge("nodea", "nodeb");
    fleet.kill_edge("nodea", "nodeb", WAIT);
    fleet.wait_route("nodec", "nodea", false);
    reply(&fleet, "offline-answer", "held answer");
    let count: i64 = db(fleet.node("nodec")).query_row(
        "SELECT count(*) FROM envelopes WHERE request_origin IS NOT NULL AND kind='message' AND state='held' AND next_hop=''", [], |r|r.get(0)).unwrap();
    assert_eq!(count, 1);
    fleet.allow_edge("nodea", "nodeb");
    fleet.wait_route("nodec", "nodea", true);
    read_answer(&fleet, &sender, "held answer");
}

#[test]
fn follow_up_after_final_answer_on_one_way_edge_is_collected() {
    let (fleet, sender, recipient) = setup("routed-followup", CHAIN);
    send(&fleet, &sender, &recipient, "followup");
    read_once(&fleet, &recipient, "followup");
    reply(&fleet, "followup", "first answer");
    read_answer(&fleet, &sender, "first answer");
    reply(&fleet, "followup", "second answer");
    read_answer(&fleet, &sender, "second answer");
}

#[test]
fn recipient_gone_receipt_reaches_multihop_origin() {
    let (fleet, sender, recipient) = setup("routed-gone", CHAIN);
    send(&fleet, &sender, &recipient, "gone");
    remote_state(&fleet, "gone", "delivered");
    let response = api(
        fleet.node("nodec"),
        "pane.close",
        json!({"pane_id":recipient["pane_id"]}),
    );
    assert!(response.get("error").is_none(), "{response}");
    remote_state(&fleet, "gone", "recipient_gone");
}

#[test]
fn expired_receipt_reaches_multihop_origin() {
    let (fleet, sender, recipient) = setup("routed-expired", CHAIN);
    send(&fleet, &sender, &recipient, "expiring");
    remote_state(&fleet, "expiring", "delivered");
    // Lapse the unread inbox row on the owner instead of waiting a day.
    let lapsed = db(fleet.node("nodec"))
        .execute(
            "UPDATE envelopes SET inbox_deadline=0
             WHERE correlation='expiring' AND kind='message' AND state='inbox'",
            [],
        )
        .unwrap();
    assert_eq!(lapsed, 1);
    remote_state(&fleet, "expiring", "expired");
}

#[test]
fn channel_original_settles_on_reply_custody_multihop() {
    let (fleet, sender, recipient) = setup(
        "routed-channel",
        &[
            CHAIN[0].clone(),
            CHAIN[1].clone(),
            NodeSpec::new("nodec", "channel-c", &[]).with_config("[msg]\nchannel_push = true\n"),
        ],
    );
    send(&fleet, &sender, &recipient, "channel");
    remote_state(&fleet, "channel", "delivered");
    let quote = |text: &str| format!("'{}'", text.replace('\'', "'\\''"));
    let output = fleet.node("nodec").home.join("channel-reply.json");
    let response = api(
        fleet.node("nodec"),
        "pane.send_text",
        json!({
            "pane_id":recipient["pane_id"],
            "text":format!("{} msg reply channel 'channel answer' >{}\n",
                quote(env!("CARGO_BIN_EXE_flk")), quote(output.to_str().unwrap()))
        }),
    );
    assert!(response.get("error").is_none(), "{response}");
    let response: Value = fleet::wait_until("reply from recipient ancestry", WAIT, || {
        serde_json::from_slice(&std::fs::read(&output).ok()?).ok()
    });
    assert!(response.get("error").is_none(), "{response}");
    read_answer(&fleet, &sender, "channel answer");
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
fn laptop_collects_answer_through_a_different_hub() {
    let fleet = fleet::spawn("routed-two-hubs", fleet::LAPTOP_TWO_HUBS);
    let start = |name: &str| {
        let node = fleet.node(name);
        api(
            node,
            "agent.start",
            json!({"name":"roaming", "argv":["/bin/sh"], "cwd":node.repo}),
        )["result"]["agent"]
            .clone()
    };
    let sender = start("nodea");
    let recipient = start("noded.example");
    fleet.wait_route("nodea", "noded.example", true);
    fleet::wait_until("roaming recipient discovery", WAIT, || {
        api(fleet.node("nodea"), "agent.list", json!({}))["result"]["fleet"]
            .as_array()?
            .iter()
            .any(|row| row["agent_id"] == recipient["agent_id"])
            .then_some(())
    });
    fleet.refuse_edge("nodea", "nodec");
    fleet.kill_edge("nodea", "nodec", WAIT);
    send(&fleet, &sender, &recipient, "roaming");
    remote_state(&fleet, "roaming", "delivered");
    let response = api(
        fleet.node("noded.example"),
        "msg.read",
        json!({"pane":recipient["pane_id"]}),
    );
    assert_eq!(response["result"]["messages"].as_array().unwrap().len(), 1);
    fleet.refuse_edge("nodea", "nodeb");
    fleet.kill_edge("nodea", "nodeb", WAIT);
    fleet.wait_route("noded.example", "nodea", false);
    let response = api(
        fleet.node("noded.example"),
        "msg.reply",
        json!({"correlation_id":"roaming", "body":"via second hub"}),
    );
    assert!(response.get("error").is_none(), "{response}");
    fleet.allow_edge("nodea", "nodec");
    fleet.wait_route("noded.example", "nodea", true);
    read_answer(&fleet, &sender, "via second hub");
}

#[test]
fn forged_answer_signature_or_token_is_refused() {
    for wrong_token in [false, true] {
        let (fleet, sender, recipient) = setup(
            if wrong_token {
                "routed-token"
            } else {
                "routed-signature"
            },
            CHAIN,
        );
        send(&fleet, &sender, &recipient, "forged");
        read_once(&fleet, &recipient, "forged");
        fleet.refuse_edge("nodea", "nodeb");
        fleet.kill_edge("nodea", "nodeb", WAIT);
        fleet.wait_route("nodec", "nodea", false);
        if wrong_token {
            // The receiver signs an answer with a token the original sender never granted.
            db(fleet.node("nodec")).execute(
                "UPDATE envelopes SET metadata=json_set(metadata,'$.return_binding.collection_token',json(?1)) WHERE correlation='forged'",
                [serde_json::to_string(&vec![0u8;32]).unwrap()],
            ).unwrap();
        }
        reply(&fleet, "forged", "refuse this answer");
        if !wrong_token {
            db(fleet.node("nodec")).execute(
                "UPDATE envelopes SET metadata=json_set(metadata,'$.signature',json(?1)) WHERE request_origin IS NOT NULL AND kind='message'",
                [serde_json::to_string(&vec![0u8;64]).unwrap()],
            ).unwrap();
        }
        fleet.allow_edge("nodea", "nodeb");
        fleet.wait_route("nodec", "nodea", true);
        let custodian = if wrong_token { "nodeb" } else { "nodec" };
        fleet::wait_until("forged answer refused", WAIT, || {
            let count: i64 = db(fleet.node(custodian)).query_row(
                "SELECT count(*) FROM envelopes WHERE request_origin IS NOT NULL AND kind='message' AND state='refused'", [], |r| r.get(0)).ok()?;
            (count == 1).then_some(())
        });
        assert_eq!(
            api(
                fleet.node("nodea"),
                "msg.read",
                json!({"pane":sender["pane_id"]})
            )["result"]["messages"],
            json!([])
        );
    }
}

// Ported from #852's mesh_step2 regression at 65709cd6, using this suite's helpers.
#[test]
fn route_cycle_and_exhausted_hop_budget_retain_custody() {
    for (field, value) in [("visited", json!(["receiver"])), ("hops_left", json!(0))] {
        let (fleet, sender, recipient) = setup("reroute-lease", CHAIN);
        let origin = fleet.node("nodea");
        let capture = fleet.base.join("capture-delivery-nodea-nodeb");
        std::fs::write(&capture, "").unwrap();
        assert_eq!(
            send(&fleet, &sender, &recipient, "lease-refusal")["result"]["state"],
            "queued"
        );
        let mut delivery: Value = fleet::wait_until("initial delivery captured", WAIT, || {
            serde_json::from_slice(&std::fs::read(&capture).ok()?).ok()
        });
        fleet::wait_until("captured delivery retry scheduled", WAIT, || {
            let ready: bool = db(origin).query_row(
                "SELECT retry_at>0 AND lease_until<=retry_at FROM envelopes WHERE correlation='lease-refusal'",
                [], |r| r.get(0)).ok()?;
            ready.then_some(())
        });
        delivery[field] = if field == "visited" {
            json!([
                fleet.node_id("nodea"),
                fleet.node_id("nodeb"),
                fleet.node_id("nodea")
            ])
        } else {
            value
        };
        let replay = fleet.base.join("replay-delivery-nodea-nodeb");
        std::fs::write(&replay, serde_json::to_vec(&delivery).unwrap()).unwrap();
        std::fs::remove_file(capture).unwrap();
        std::fs::write(fleet.base.join("observe-delivery-nodea-nodeb"), "").unwrap();
        due(origin);
        let refused: Value = fleet::wait_until("route refused on wire", WAIT, || {
            serde_json::from_slice(
                &std::fs::read(fleet.base.join("observed-result-nodea-nodeb")).ok()?,
            )
            .ok()
        });
        assert_eq!(
            refused["error"]["message"],
            if field == "visited" {
                "loop_detected"
            } else {
                "hop_budget_exhausted"
            },
            "{refused}"
        );
        remote_state(&fleet, "lease-refusal", "queued");
        fleet::wait_until("rejected route releases custody lease", WAIT, || {
            let row: (String, String, i64) = db(origin).query_row(
                "SELECT state,next_hop,lease_until FROM envelopes WHERE correlation='lease-refusal'",
                [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
            (row == ("custody".into(), String::new(), 0)).then_some(())
        });
        let attempts = || {
            std::fs::read_to_string(fleet.base.join("delivery-attempts-nodea-nodeb"))
                .unwrap()
                .lines()
                .filter(|line| *line == "lease-refusal")
                .count()
        };
        let before = attempts();
        let observed = std::time::Instant::now();
        fleet::wait_until(
            "unpushable custody stays unleased across worker ticks",
            WAIT,
            || {
                let leased: i64 = db(origin).query_row(
                "SELECT count(*) FROM envelopes WHERE next_hop='' AND lease_until>0 AND state IN ('custody','held')", [], |r|r.get(0)).unwrap();
                assert_eq!(leased, 0);
                assert_eq!(
                    attempts(),
                    before,
                    "same refused route retried without a generation change"
                );
                (observed.elapsed() >= Duration::from_secs(2)).then_some(())
            },
        );
        assert_eq!(
            api(
                fleet.node("nodec"),
                "msg.read",
                json!({"pane":recipient["pane_id"]})
            )["result"]["messages"],
            json!([])
        );
        std::fs::remove_file(replay).unwrap();
        fleet.refuse_edge("nodeb", "nodec");
        fleet.kill_edge("nodeb", "nodec", WAIT);
        fleet.wait_route("nodea", "nodec", false);
        fleet.allow_edge("nodeb", "nodec");
        fleet.wait_route("nodea", "nodec", true);
        due(origin);
        read_once(&fleet, &recipient, "lease-refusal");
        let count: i64 = db(fleet.node("nodec"))
            .query_row(
                "SELECT count(*) FROM envelopes WHERE correlation='lease-refusal'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }
}

/// The origin's view once a hub signs its own outcome (#876).
fn hub_outcome(fleet: &Fleet, correlation: &str, detail: &str) {
    fleet::wait_until("hub-signed outcome at the origin", WAIT, || {
        let response = api(
            fleet.node("nodea"),
            "msg.status",
            json!({"correlation_id":correlation}),
        );
        (response["result"]["state"] == "undeliverable" && response["result"]["detail"] == detail)
            .then_some(())
    });
}

/// Replay the hub's captured push to `next` with one field changed, so
/// `next` refuses the route and the hub keeps custody with no next hop.
fn refuse_at_hub(fleet: &Fleet, next: &str, correlation: &str, field: &str, value: Value) {
    let capture = fleet.base.join(format!("capture-delivery-nodeb-{next}"));
    let mut delivery: Value = fleet::wait_until("hub delivery captured", WAIT, || {
        serde_json::from_slice(&std::fs::read(&capture).ok()?).ok()
    });
    delivery[field] = value;
    std::fs::write(
        fleet.base.join(format!("replay-delivery-nodeb-{next}")),
        serde_json::to_vec(&delivery).unwrap(),
    )
    .unwrap();
    std::fs::remove_file(capture).unwrap();
    due(fleet.node("nodeb"));
    fleet::wait_until("hub retains unrouted custody", WAIT, || {
        let row: (String, String) = db(fleet.node("nodeb"))
            .query_row(
                "SELECT state,next_hop FROM envelopes WHERE correlation=?1",
                [correlation],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok()?;
        (row == ("custody".into(), String::new())).then_some(())
    });
    remote_state(fleet, correlation, "custody");
}

/// Lapse the hub's custody of `correlation` instead of waiting seven days.
fn lapse_hub_custody(fleet: &Fleet, correlation: &str) {
    let lapsed = db(fleet.node("nodeb"))
        .execute(
            "UPDATE envelopes SET custody_deadline=0 WHERE correlation=?1 AND kind='message'",
            [correlation],
        )
        .unwrap();
    assert_eq!(lapsed, 1);
}

#[test]
fn loop_at_a_hub_reaches_the_origin_as_a_hub_signed_outcome() {
    let (fleet, sender, recipient) = setup("hub-loop", CHAIN);
    let capture = fleet.base.join("capture-delivery-nodeb-nodec");
    std::fs::write(&capture, "").unwrap();
    send(&fleet, &sender, &recipient, "hub-loop");
    refuse_at_hub(
        &fleet,
        "nodec",
        "hub-loop",
        "visited",
        json!([
            fleet.node_id("nodea"),
            fleet.node_id("nodec"),
            fleet.node_id("nodeb")
        ]),
    );
    lapse_hub_custody(&fleet, "hub-loop");
    hub_outcome(&fleet, "hub-loop", "loop_detected at nodeb");
}

#[test]
fn exhausted_hop_budget_at_a_hub_reaches_the_origin() {
    let (fleet, sender, recipient) = prepare(
        "hub-hops",
        &[
            NodeSpec::new("nodea", "hops-a", &["nodeb"]),
            NodeSpec::new("nodeb", "hops-b", &["noded"]),
            NodeSpec::new("noded", "hops-d", &["nodec"]),
            NodeSpec::new("nodec", "hops-c", &[]),
        ],
        true,
    );
    let capture = fleet.base.join("capture-delivery-nodeb-noded");
    std::fs::write(&capture, "").unwrap();
    send(&fleet, &sender, &recipient, "hub-hops");
    refuse_at_hub(&fleet, "noded", "hub-hops", "hops_left", json!(0));
    lapse_hub_custody(&fleet, "hub-hops");
    hub_outcome(&fleet, "hub-hops", "hop_budget_exhausted at nodeb");
}

#[test]
fn no_route_at_a_hub_reaches_the_origin_after_its_own_expiry() {
    let (fleet, sender, recipient) = setup("hub-no-route", CHAIN);
    let capture = fleet.base.join("capture-delivery-nodeb-nodec");
    std::fs::write(&capture, "").unwrap();
    send(&fleet, &sender, &recipient, "hub-no-route");
    fleet::wait_until("hub delivery captured", WAIT, || {
        (!std::fs::read(&capture).ok()?.is_empty()).then_some(())
    });
    fleet.refuse_edge("nodeb", "nodec");
    fleet.kill_edge("nodeb", "nodec", WAIT);
    fleet.wait_route("nodeb", "nodec", false);
    fleet::wait_until("hub custody loses its next hop", WAIT, || {
        let hop: String = db(fleet.node("nodeb"))
            .query_row(
                "SELECT next_hop FROM envelopes WHERE correlation='hub-no-route'",
                [],
                |r| r.get(0),
            )
            .ok()?;
        hop.is_empty().then_some(())
    });
    // Both deadlines pass. The origin reports its own expiry first, and the
    // hub's reason replaces it when it arrives.
    db(fleet.node("nodea"))
        .execute(
            "UPDATE envelopes SET custody_deadline=0 WHERE correlation='hub-no-route'",
            [],
        )
        .unwrap();
    remote_state(&fleet, "hub-no-route", "expired");
    lapse_hub_custody(&fleet, "hub-no-route");
    hub_outcome(&fleet, "hub-no-route", "no_route at nodeb");
    std::fs::remove_file(capture).unwrap();
}
