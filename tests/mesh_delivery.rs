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
            "body":"durable fleet mail", "correlation_id":correlation, "intent":"fyi"
        }),
    )
}

fn read(node: &Node, pane: &Value) -> Value {
    request(node, "msg.read", json!({"pane":pane}))["result"]["messages"].clone()
}

#[test]
fn cross_host_send_and_boot_projection_preserve_exactly_one_inbox_import() {
    let mut fleet = fleet::spawn("mesh-deliver", PAIR);
    let recipient = agent(fleet.node("nodeb"));
    discover(&fleet, &recipient);
    let sent = send(&fleet, &recipient, "delivered-once");
    assert_eq!(sent["result"]["state"], "delivered", "{sent}");
    assert_eq!(
        sent["result"]["message_key"]["message_id"]
            .as_str()
            .unwrap()
            .len(),
        26
    );
    fleet.node_mut("nodeb").restart();
    let messages = read(fleet.node("nodeb"), &recipient["pane_id"]);
    assert_eq!(messages.as_array().unwrap().len(), 1, "{messages}");
    assert_eq!(messages[0]["body"], "durable fleet mail");
    fleet.node_mut("nodeb").restart();
    assert_eq!(read(fleet.node("nodeb"), &recipient["pane_id"]), json!([]));
}

#[test]
fn origin_restart_redelivers_the_same_key_after_a_partition() {
    let mut fleet = fleet::spawn("mesh-outbox", PAIR);
    let recipient = agent(fleet.node("nodeb"));
    discover(&fleet, &recipient);
    fleet.refuse_edge("nodea", "nodeb");
    fleet.kill_edge("nodea", "nodeb", Duration::from_secs(10));
    let sent = send(&fleet, &recipient, "offline-custody");
    assert_eq!(sent["result"]["state"], "queued", "{sent}");
    fleet.node_mut("nodea").restart();
    fleet.allow_edge("nodea", "nodeb");
    let messages = fleet::wait_until("outbox replay", Duration::from_secs(150), || {
        let messages = read(fleet.node("nodeb"), &recipient["pane_id"]);
        (!messages.as_array()?.is_empty()).then_some(messages)
    });
    assert_eq!(messages.as_array().unwrap().len(), 1);
    assert_eq!(messages[0]["correlation_id"], "offline-custody");
}

#[test]
fn lost_receipt_then_receiver_restart_deduplicates_the_retry() {
    let mut fleet = fleet::spawn("mesh-receipt", PAIR);
    let recipient = agent(fleet.node("nodeb"));
    discover(&fleet, &recipient);
    std::fs::write(fleet.base.join("lose-receipt-nodea-nodeb"), "once").unwrap();
    let sent = send(&fleet, &recipient, "lost-receipt");
    assert_eq!(sent["result"]["state"], "queued", "{sent}");
    assert!(fleet.base.join("lost-receipt-nodea-nodeb").exists());
    fleet.node_mut("nodeb").restart();
    fleet::wait_until("delivery receipt replay", Duration::from_secs(150), || {
        let path = fleet.base.join("delivered-receipt-nodea-nodeb");
        path.exists().then_some(())
    });
    let messages = read(fleet.node("nodeb"), &recipient["pane_id"]);
    assert_eq!(messages.as_array().unwrap().len(), 1, "{messages}");
}

#[test]
fn refused_old_peer_retains_mail_and_names_the_upgrade() {
    let fleet = fleet::spawn("mesh-old", PAIR);
    let recipient = agent(fleet.node("nodeb"));
    discover(&fleet, &recipient);
    std::fs::write(fleet.base.join("old-peer-nodeb"), "disabled").unwrap();
    fleet.kill_edge("nodea", "nodeb", Duration::from_secs(10));
    fleet::wait_until("old peer refusal", Duration::from_secs(150), || {
        let status = request(fleet.node("nodea"), "peers.enrollment", json!({}));
        status["result"]["peers"]
            .as_array()?
            .iter()
            .find(|peer| {
                peer["peer"] == "nodeb"
                    && peer["reason"]
                        .as_str()
                        .is_some_and(|r| r.contains("upgrade flk on nodeb"))
            })
            .cloned()
    });
    let sent = send(&fleet, &recipient, "upgrade-queued");
    assert_eq!(sent["result"]["state"], "queued", "{sent}");
    assert!(
        sent["result"]["warnings"]
            .to_string()
            .contains("upgrade flk on nodeb"),
        "{sent}"
    );
    assert!(sent["result"]["message_key"].is_object());
    assert_eq!(read(fleet.node("nodeb"), &recipient["pane_id"]), json!([]));
}

#[test]
fn repeated_correlation_ids_mint_distinct_keys_and_import_both_messages() {
    let fleet = fleet::spawn("mesh-correlation", PAIR);
    let recipient = agent(fleet.node("nodeb"));
    discover(&fleet, &recipient);
    let first = send(&fleet, &recipient, "same-conversation");
    let second = send(&fleet, &recipient, "same-conversation");
    assert_eq!(first["result"]["state"], "delivered", "{first}");
    assert_eq!(second["result"]["state"], "delivered", "{second}");
    assert_ne!(
        first["result"]["message_key"],
        second["result"]["message_key"]
    );
    assert_eq!(
        read(fleet.node("nodeb"), &recipient["pane_id"])
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn legacy_log_inbox_is_imported_once_and_survives_log_removal() {
    use std::io::Write;
    let mut fleet = fleet::spawn(
        "mesh-migration",
        &[NodeSpec::new("nodea", "legacy-mail", &[])],
    );
    let node = fleet.node("nodea");
    let recipient = agent(node);
    let database = node.home.join("state/flock-dev/mesh-mail.sqlite");
    let connection = rusqlite::Connection::open(database).unwrap();
    // Model the pre-custody migration boundary in this isolated database.
    connection
        .execute("DELETE FROM migrations WHERE name='audit-inbox-v1'", [])
        .unwrap();
    drop(connection);
    let log = node.config_home.join("flock-dev/event-log.jsonl");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let event = json!({"seq":1000000, "ts_ms":now, "envelope":{
        "event":"message_queued", "data":{
            "type":"message_queued", "correlation_id":"legacy-unread", "to_pane":recipient["pane_id"],
            "cross_repo":false, "enqueued_at_ms":now, "intent":"fyi", "body":"pre-mesh mail"
        }
    }});
    writeln!(
        std::fs::OpenOptions::new().append(true).open(&log).unwrap(),
        "{event}"
    )
    .unwrap();
    fleet.node_mut("nodea").restart();
    std::fs::write(&log, "").unwrap();
    fleet.node_mut("nodea").restart();
    let messages = read(fleet.node("nodea"), &recipient["pane_id"]);
    assert_eq!(messages.as_array().unwrap().len(), 1, "{messages}");
    assert_eq!(messages[0]["body"], "pre-mesh mail");
    fleet.node_mut("nodea").restart();
    assert_eq!(read(fleet.node("nodea"), &recipient["pane_id"]), json!([]));
}
