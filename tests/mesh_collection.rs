mod support;

use serde_json::{json, Value};
use std::time::Duration;
use support::fleet::{self, Node, NodeSpec};

const PAIR: &[NodeSpec] = &[
    NodeSpec::new("nodea", "mesh-origin", &["nodeb"]),
    NodeSpec::new("nodeb", "mesh-recipient", &[]),
];

fn request(node: &Node, method: &str, params: Value) -> Value {
    serde_json::from_str(
        &node.api(&json!({"id":"test", "method":method, "params":params}).to_string()),
    )
    .unwrap()
}

fn agent(node: &Node) -> Value {
    let current = request(node, "agent.list", json!({}))["result"]["agents"][0].clone();
    if !current.is_null() {
        return current;
    }
    let started = request(
        node,
        "agent.start",
        json!({
            "name":"mailbox", "argv":["/bin/sh", "-c", "while read line; do :; done"], "cwd":node.repo
        }),
    );
    assert!(started["result"]["agent"].is_object(), "{started}");
    started["result"]["agent"].clone()
}

fn discover(fleet: &fleet::Fleet, recipient: &Value) {
    fleet::wait_until("recipient discovery", Duration::from_secs(90), || {
        request(fleet.node("nodea"), "agent.list", json!({}))["result"]["fleet"]
            .as_array()?
            .iter()
            .find(|row| row["agent_id"] == recipient["agent_id"])
            .cloned()
    });
}

fn send(fleet: &fleet::Fleet, recipient: &Value, correlation: &str) -> Value {
    request(
        fleet.node("nodea"),
        "msg.send",
        json!({
            "to":{"type":"agent", "agent":recipient["agent_id"]},
            "from_agent":agent(fleet.node("nodea"))["agent_id"],
            "body":"durable fleet mail", "correlation_id":correlation, "intent":"needs_reply"
        }),
    )
}

fn read(node: &Node, pane: &Value) -> Value {
    request(node, "msg.read", json!({"pane":pane}))["result"]["messages"].clone()
}

fn database(node: &Node) -> rusqlite::Connection {
    let db =
        rusqlite::Connection::open(node.home.join("state/flock-dev/mesh-mail.sqlite")).unwrap();
    db.busy_timeout(Duration::from_secs(5)).unwrap();
    db
}

fn reply(node: &Node, correlation: &str, body: &str) -> Value {
    request(
        node,
        "msg.reply",
        json!({"correlation_id":correlation,"body":body}),
    )
}

fn ready(node: &Node) {
    // Move only this sandbox's scheduler deadline, not production backoff or timeouts.
    database(node)
        .execute("UPDATE envelopes SET collect_at=0", [])
        .unwrap();
}

fn answers(node: &Node, pane: &Value) -> Value {
    fleet::wait_until("collected answer", Duration::from_secs(30), || {
        let messages = read(node, pane);
        (!messages.as_array()?.is_empty()).then_some(messages)
    })
}

fn question(fleet: &fleet::Fleet, correlation: &str) -> (Value, Value) {
    let sender = agent(fleet.node("nodea"));
    let recipient = agent(fleet.node("nodeb"));
    discover(fleet, &recipient);
    let sent = send(fleet, &recipient, correlation);
    assert_eq!(sent["result"]["state"], "delivered", "{sent}");
    (sender, recipient)
}

#[test]
fn incident_direct_reply_is_held_and_collected_without_a_waiter() {
    let fleet = fleet::spawn("mesh-collect", PAIR);
    let (sender, recipient) = question(&fleet, "incident");
    assert_eq!(
        read(fleet.node("nodeb"), &recipient["pane_id"])
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let response = reply(fleet.node("nodeb"), "incident", "answer to the laptop");
    assert_eq!(response["result"]["state"], "held", "{response}");
    assert!(response["result"]["message_key"].is_object());
    ready(fleet.node("nodea"));
    let messages = answers(fleet.node("nodea"), &sender["pane_id"]);
    assert_eq!(messages.as_array().unwrap().len(), 1);
    assert_eq!(messages[0]["body"], "answer to the laptop");
    assert_eq!(messages[0]["in_reply_to"], "incident");
    assert_eq!(messages[0]["from_host"], "nodeb");
}

#[test]
fn offline_origin_and_receiver_restart_preserve_held_answer() {
    let mut fleet = fleet::spawn("mesh-collect-restart", PAIR);
    let (sender, recipient) = question(&fleet, "offline");
    read(fleet.node("nodeb"), &recipient["pane_id"]);
    fleet.refuse_edge("nodea", "nodeb");
    fleet.kill_edge("nodea", "nodeb", Duration::from_secs(10));
    fleet.node_mut("nodea").stop();
    let response = reply(fleet.node("nodeb"), "offline", "kept across restart");
    assert_eq!(response["result"]["state"], "held", "{response}");
    fleet.node_mut("nodeb").restart();
    ready(fleet.node("nodea"));
    fleet.allow_edge("nodea", "nodeb");
    fleet.node_mut("nodea").restart();
    let messages = answers(fleet.node("nodea"), &sender["pane_id"]);
    assert_eq!(messages[0]["body"], "kept across restart");
    assert_eq!(messages.as_array().unwrap().len(), 1);
}

#[test]
fn lost_ack_recollects_the_same_key_and_imports_it_only_once() {
    let mut fleet = fleet::spawn("mesh-collect-ack", PAIR);
    let (sender, _) = question(&fleet, "lost-ack");
    let gate = fleet.base.join("gate-collect-ack-nodea-nodeb");
    std::fs::create_dir(&gate).unwrap();
    assert_eq!(
        reply(fleet.node("nodeb"), "lost-ack", "once")["result"]["state"],
        "held"
    );
    ready(fleet.node("nodea"));
    fleet::wait_until("ack held after import", Duration::from_secs(30), || {
        gate.join("entered").exists().then_some(())
    });
    assert_eq!(
        read(fleet.node("nodea"), &sender["pane_id"])
            .as_array()
            .unwrap()
            .len(),
        1
    );
    fleet.refuse_edge("nodea", "nodeb");
    fleet.kill_edge("nodea", "nodeb", Duration::from_secs(10));
    let receiver = database(fleet.node("nodeb"));
    assert_eq!(
        receiver
            .query_row(
                "SELECT count(*) FROM envelopes WHERE state='custody'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    std::fs::remove_dir_all(gate).unwrap();
    ready(fleet.node("nodea"));
    fleet.allow_edge("nodea", "nodeb");
    fleet.node_mut("nodea").restart();
    fleet::wait_until(
        "duplicate collected and acknowledged",
        Duration::from_secs(30),
        || {
            (receiver
                .query_row(
                    "SELECT count(*) FROM envelopes WHERE state='custody'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .ok()?
                == 0)
                .then_some(())
        },
    );
    assert_eq!(read(fleet.node("nodea"), &sender["pane_id"]), json!([]));
    assert_eq!(
        database(fleet.node("nodea"))
            .query_row("SELECT count(*) FROM inbox_imports", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn real_answer_after_a_deferral_is_collected_independently() {
    let fleet = fleet::spawn("mesh-collect-defer", PAIR);
    let (sender, recipient) = question(&fleet, "deferred");
    let muted = request(
        fleet.node("nodeb"),
        "msg.mute",
        json!({"pane":recipient["pane_id"],"seconds":600,"reason":"busy"}),
    );
    assert_eq!(muted["result"]["deferred"], 1, "{muted}");
    ready(fleet.node("nodea"));
    let messages = answers(fleet.node("nodea"), &sender["pane_id"]);
    assert_eq!(messages[0]["correlation_id"], "deferred:deferred");
    assert_eq!(
        reply(fleet.node("nodeb"), "deferred", "finished")["result"]["state"],
        "held"
    );
    ready(fleet.node("nodea"));
    let messages = answers(fleet.node("nodea"), &sender["pane_id"]);
    assert_eq!(messages.as_array().unwrap().len(), 1);
    assert_eq!(messages[0]["body"], "finished");
}

#[test]
fn pause_freezes_collection_and_custody_clock_across_restart() {
    let mut fleet = fleet::spawn("mesh-collect-pause", PAIR);
    let (sender, _) = question(&fleet, "paused");
    assert!(request(fleet.node("nodea"), "fleet.pause", json!({}))
        .get("error")
        .is_none());
    let origin = database(fleet.node("nodea"));
    fleet::wait_until("persisted pause", Duration::from_secs(5), || {
        origin
            .query_row("SELECT paused FROM clock", [], |r| r.get::<_, bool>(0))
            .ok()?
            .then_some(())
    });
    assert_eq!(
        reply(fleet.node("nodeb"), "paused", "after pause")["result"]["state"],
        "held"
    );
    ready(fleet.node("nodea"));
    let clock: i64 = origin
        .query_row("SELECT elapsed FROM clock", [], |r| r.get(0))
        .unwrap();
    fleet.node_mut("nodea").restart();
    assert_eq!(
        origin
            .query_row("SELECT elapsed FROM clock", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        clock
    );
    assert_eq!(
        origin
            .query_row("SELECT count(*) FROM inbox_imports", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert!(request(fleet.node("nodea"), "fleet.resume", json!({}))
        .get("error")
        .is_none());
    assert_eq!(
        answers(fleet.node("nodea"), &sender["pane_id"])[0]["body"],
        "after pause"
    );
}

#[test]
fn another_enrolled_node_cannot_collect_even_with_the_origins_token() {
    let mut fleet = fleet::spawn_with_startup_probe(
        "mesh-collect-auth",
        &[
            NodeSpec::new("nodea", "collection-origin", &["nodeb"]),
            NodeSpec::new("nodeb", "collection-recipient", &[]),
            NodeSpec::new("nodec", "collection-intruder", &["nodeb"]),
        ],
        |fleet, name| {
            if name == "nodec" {
                fleet.refuse_edge("nodec", "nodeb");
            }
        },
    );
    let (_, recipient) = question(&fleet, "private-answer");
    fleet.refuse_edge("nodea", "nodeb");
    fleet.kill_edge("nodea", "nodeb", Duration::from_secs(10));
    fleet.node_mut("nodea").stop();
    assert_eq!(
        reply(fleet.node("nodeb"), "private-answer", "only for the origin")["result"]["state"],
        "held"
    );
    let metadata: String = database(fleet.node("nodea"))
        .query_row(
            "SELECT metadata FROM envelopes WHERE correlation='private-answer'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let original: Value = serde_json::from_str(&metadata).unwrap();
    let forged = json!({"request":original["key"],"token":original["return_binding"]["collection_token"],"ack":[]});
    std::fs::write(
        fleet.base.join("replay-collect-nodec-nodeb"),
        forged.to_string(),
    )
    .unwrap();
    fleet.allow_edge("nodec", "nodeb");
    fleet.node_mut("nodec").restart();
    fleet::wait_until("intruder enrolled", Duration::from_secs(30), || {
        request(fleet.node("nodec"), "peers.enrollment", json!({}))["result"]["peers"]
            .as_array()?
            .iter()
            .any(|p| p["peer"] == "nodeb" && p["state"] == "pinned")
            .then_some(())
    });
    let sent = request(
        fleet.node("nodec"),
        "msg.send",
        json!({
            "to":{"type":"agent","agent":recipient["agent_id"]},
            "from_agent":agent(fleet.node("nodec"))["agent_id"],
            "body":"own conversation", "correlation_id":"intruder-request","intent":"needs_reply"
        }),
    );
    assert_eq!(sent["result"]["state"], "delivered", "{sent}");
    ready(fleet.node("nodec"));
    let refused = fleet.base.join("collect-refused-nodec-nodeb");
    fleet::wait_until("foreign origin refused", Duration::from_secs(30), || {
        refused.exists().then_some(())
    });
    assert!(std::fs::read_to_string(refused)
        .unwrap()
        .contains("mesh_collection_refused"));
    assert_eq!(
        database(fleet.node("nodec"))
            .query_row("SELECT count(*) FROM inbox_imports", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        database(fleet.node("nodeb"))
            .query_row(
                "SELECT count(*) FROM envelopes WHERE state='custody'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}

#[test]
fn local_held_answer_and_deferral_use_custody_and_keep_existing_wait_semantics() {
    let fleet = fleet::spawn(
        "mesh-local-answers",
        &[NodeSpec::new("nodea", "local-custody", &[])],
    );
    let node = fleet.node("nodea");
    let recipient = agent(node);
    let sent = request(
        node,
        "msg.send",
        json!({
            "to":{"type":"pane","pane":recipient["pane_id"]}, "body":"question",
            "correlation_id":"anonymous", "intent":"needs_reply"
        }),
    );
    assert_eq!(sent["result"]["state"], "queued", "{sent}");
    let muted = request(
        node,
        "msg.mute",
        json!({"pane":recipient["pane_id"],"seconds":600,"reason":"deep work"}),
    );
    assert_eq!(muted["result"]["deferred"], 1, "{muted}");
    assert_eq!(
        request(
            node,
            "msg.mute",
            json!({"pane":recipient["pane_id"],"seconds":900})
        )["result"]["deferred"],
        0
    );
    let status = request(node, "msg.status", json!({"correlation_id":"anonymous"}));
    assert_eq!(status["result"]["reply"]["kind"], "deferral", "{status}");
    assert!(status["result"]["reply"]["body"]
        .as_str()
        .unwrap()
        .contains("deep work"));
    let response = reply(node, "anonymous", "real answer");
    assert_eq!(response["result"]["state"], "held", "{response}");
    let status = request(node, "msg.status", json!({"correlation_id":"anonymous"}));
    assert_eq!(status["result"]["reply"]["body"], "real answer", "{status}");
    assert_eq!(status["result"]["reply"]["held"], true);
    assert_eq!(
        database(node)
            .query_row("SELECT count(*) FROM envelopes", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
}
