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
    let response = request(node, "msg.read", json!({"pane":pane}));
    assert!(response["result"]["messages"].is_array(), "{response}");
    response["result"]["messages"].clone()
}

#[test]
fn cross_host_send_and_boot_projection_preserve_exactly_one_inbox_import() {
    let mut fleet = fleet::spawn("mesh-deliver", PAIR);
    let recipient = agent(fleet.node("nodeb"));
    discover(&fleet, &recipient);
    std::fs::write(fleet.base.join("spoof-host-nodea-nodeb"), "spoof").unwrap();
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
    assert_eq!(messages[0]["from_host"], "nodea");
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
    let database_path = node.home.join("state/flock-dev/mesh-mail.sqlite");
    let connection = rusqlite::Connection::open(database_path).unwrap();
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
    // Recreate a crash after import committed but before the migration marker.
    let db = database(fleet.node("nodea"));
    let before: String = db
        .query_row("SELECT metadata FROM envelopes", [], |r| r.get(0))
        .unwrap();
    db.execute("DELETE FROM migrations WHERE name='audit-inbox-v1'", [])
        .unwrap();
    fleet.node_mut("nodea").restart();
    let after: String = db
        .query_row("SELECT metadata FROM envelopes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        before, after,
        "migration retry must preserve key and return token"
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM envelopes", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    std::fs::write(&log, "").unwrap();
    fleet.node_mut("nodea").restart();
    let messages = read(fleet.node("nodea"), &recipient["pane_id"]);
    assert_eq!(messages.as_array().unwrap().len(), 1, "{messages}");
    assert_eq!(messages[0]["body"], "pre-mesh mail");
    fleet.node_mut("nodea").restart();
    assert_eq!(read(fleet.node("nodea"), &recipient["pane_id"]), json!([]));
}

fn database(node: &Node) -> rusqlite::Connection {
    let db =
        rusqlite::Connection::open(node.home.join("state/flock-dev/mesh-mail.sqlite")).unwrap();
    db.busy_timeout(Duration::from_secs(5)).unwrap();
    db
}

#[test]
fn allowed_neighbor_cannot_forge_a_disallowed_origin() {
    let fleet = fleet::spawn(
        "mesh-forgery",
        &[
            NodeSpec::new("nodea", "mesh-origin", &["nodeb"]),
            NodeSpec::new("nodeb", "mesh-recipient", &[])
                .with_config("\n[msg]\nallow_from = [\"nodea\"]\n"),
        ],
    );
    let recipient = agent(fleet.node("nodeb"));
    discover(&fleet, &recipient);
    let attack = fleet.base.join("forge-origin-nodea-nodeb");
    std::fs::write(&attack, "forge").unwrap();
    let sent = send(&fleet, &recipient, "forged-origin");
    assert_eq!(sent["result"]["state"], "refused", "{sent}");
    assert!(
        sent["result"]["warnings"]
            .to_string()
            .contains("origin_mismatch"),
        "{sent}"
    );
    assert_eq!(
        database(fleet.node("nodeb"))
            .query_row("SELECT count(*) FROM envelopes", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    std::fs::remove_file(attack).unwrap();
    let refused = request(
        fleet.node("nodea"),
        "msg.status",
        json!({"correlation_id":"forged-origin"}),
    );
    assert_eq!(refused["result"]["state"], "refused");
    assert_eq!(refused["result"]["detail"], "origin_mismatch");
    // Correcting the transport requires a new send, not revival of refused mail.
    let sent = send(&fleet, &recipient, "authentic-origin");
    assert_eq!(sent["result"]["state"], "delivered", "{sent}");
    let messages = fleet::wait_until(
        "new authentic send after forgery refusal",
        Duration::from_secs(30),
        || {
            let messages = read(fleet.node("nodeb"), &recipient["pane_id"]);
            (!messages.as_array()?.is_empty()).then_some(messages)
        },
    );
    assert_eq!(messages.as_array().unwrap().len(), 1);
    let key = &sent["result"]["message_key"];
    assert_eq!(
        database(fleet.node("nodeb"))
            .query_row(
                "SELECT count(*) FROM envelopes WHERE origin=?1 AND id=?2",
                rusqlite::params![
                    key["origin_node"].as_str().unwrap(),
                    key["message_id"].as_str().unwrap()
                ],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    assert_eq!(messages[0]["from_host"], "nodea");
}

#[test]
fn receiver_mail_store_full_keeps_origin_custody_until_space_frees() {
    let fleet = fleet::spawn("mesh-quota", PAIR);
    let recipient = agent(fleet.node("nodeb"));
    discover(&fleet, &recipient);
    // Inject quota pressure into this isolated store's accounting boundary.
    let db = database(fleet.node("nodeb"));
    db.execute("UPDATE usage SET active=10000", []).unwrap();
    let sent = send(&fleet, &recipient, "quota-refusal");
    assert_eq!(sent["result"]["state"], "queued", "{sent}");
    assert!(
        sent["result"]["warnings"]
            .to_string()
            .contains("mail_store_full"),
        "{sent}"
    );
    db.execute("UPDATE usage SET active=0", []).unwrap();
    database(fleet.node("nodea"))
        .execute(
            "UPDATE envelopes SET retry_at=0,lease_until=0 WHERE state='custody'",
            [],
        )
        .unwrap();
    let messages = fleet::wait_until(
        "delivery after quota frees",
        Duration::from_secs(30),
        || {
            let messages = read(fleet.node("nodeb"), &recipient["pane_id"]);
            (!messages.as_array()?.is_empty()).then_some(messages)
        },
    );
    assert_eq!(messages.as_array().unwrap().len(), 1);
    let key = &sent["result"]["message_key"];
    assert_eq!(
        database(fleet.node("nodeb"))
            .query_row(
                "SELECT count(*) FROM envelopes WHERE origin=?1 AND id=?2",
                rusqlite::params![
                    key["origin_node"].as_str().unwrap(),
                    key["message_id"].as_str().unwrap()
                ],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}

#[test]
fn receiver_mailbox_full_keeps_origin_custody_until_read() {
    let mut fleet = fleet::spawn("mesh-mailbox", PAIR);
    let recipient = agent(fleet.node("nodeb"));
    discover(&fleet, &recipient);
    for index in 0..32 {
        if index == 20 {
            fleet.node_mut("nodea").restart();
            discover(&fleet, &recipient);
        }
        let sent = send(&fleet, &recipient, &format!("fill-{index}"));
        assert_eq!(sent["result"]["state"], "delivered", "{sent}");
    }
    let sent = send(&fleet, &recipient, "mailbox-refusal");
    assert_eq!(sent["result"]["state"], "queued", "{sent}");
    assert!(
        sent["result"]["warnings"]
            .to_string()
            .contains("mailbox_full"),
        "{sent}"
    );
    assert_eq!(
        read(fleet.node("nodeb"), &recipient["pane_id"])
            .as_array()
            .unwrap()
            .len(),
        32
    );
    database(fleet.node("nodea"))
        .execute(
            "UPDATE envelopes SET retry_at=0,lease_until=0 WHERE state='custody'",
            [],
        )
        .unwrap();
    let messages = fleet::wait_until("delivery after inbox read", Duration::from_secs(30), || {
        let messages = read(fleet.node("nodeb"), &recipient["pane_id"]);
        (!messages.as_array()?.is_empty()).then_some(messages)
    });
    assert_eq!(messages.as_array().unwrap().len(), 1);
    let key = &sent["result"]["message_key"];
    assert_eq!(
        database(fleet.node("nodeb"))
            .query_row(
                "SELECT count(*) FROM envelopes WHERE origin=?1 AND id=?2",
                rusqlite::params![
                    key["origin_node"].as_str().unwrap(),
                    key["message_id"].as_str().unwrap()
                ],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}

#[test]
fn another_authenticated_node_cannot_pre_register_the_real_origins_key() {
    let mut fleet = fleet::spawn_with_startup_probe(
        "mesh-key-theft",
        &[
            NodeSpec::new("nodea", "real-origin", &["nodeb"]),
            NodeSpec::new("nodeb", "real-recipient", &[]),
            NodeSpec::new("nodec", "forging-origin", &["nodeb"]),
        ],
        |fleet, name| {
            // Nodes start in reverse order. Partition nodec before its first
            // enrollment so the later reconnect exercises a fresh handshake.
            if name == "nodec" {
                fleet.refuse_edge("nodec", "nodeb");
            }
            if name == "nodea" {
                // Exercise the early dial before starting the real origin, so this
                // test does not rely on winning a startup race against nodec.
                fleet::wait_until("initial nodec partition", Duration::from_secs(30), || {
                    let status = request(fleet.node("nodec"), "peers.enrollment", json!({}));
                    status["result"]["peers"]
                        .as_array()?
                        .iter()
                        .find(|peer| peer["peer"] == "nodeb" && peer["state"] == "retrying")
                        .cloned()
                });
            }
        },
    );
    let recipient = agent(fleet.node("nodeb"));
    discover(&fleet, &recipient);
    let capture = fleet.base.join("capture-delivery-nodea-nodeb");
    std::fs::write(&capture, "capture").unwrap();
    let original = send(&fleet, &recipient, "real-key");
    assert_eq!(original["result"]["state"], "queued", "{original}");
    let params = std::fs::read(&capture).unwrap();
    fleet.refuse_edge("nodea", "nodeb");
    fleet.kill_edge("nodea", "nodeb", Duration::from_secs(10));
    fleet.allow_edge("nodec", "nodeb");
    fleet.node_mut("nodec").restart();
    fleet::wait_until(
        "forging neighbor discovery",
        Duration::from_secs(90),
        || {
            request(fleet.node("nodec"), "agent.list", json!({}))["result"]["fleet"]
                .as_array()?
                .iter()
                .find(|row| row["agent_id"] == recipient["agent_id"])
                .cloned()
        },
    );
    std::fs::write(fleet.base.join("replay-delivery-nodec-nodeb"), params).unwrap();
    let forged = request(
        fleet.node("nodec"),
        "msg.send",
        json!({
            "to":{"type":"agent", "agent":recipient["agent_id"]}, "body":"attempted key theft", "intent":"fyi", "from_agent":agent(fleet.node("nodec"))["agent_id"]
        }),
    );
    assert!(
        forged["result"]["warnings"]
            .to_string()
            .contains("origin_mismatch"),
        "{forged}"
    );
    assert_eq!(
        database(fleet.node("nodeb"))
            .query_row("SELECT count(*) FROM envelopes", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    fleet.refuse_edge("nodec", "nodeb");
    fleet.kill_edge("nodec", "nodeb", Duration::from_secs(10));
    std::fs::remove_file(capture).unwrap();
    fleet.allow_edge("nodea", "nodeb");
    database(fleet.node("nodea"))
        .execute(
            "UPDATE envelopes SET retry_at=0,lease_until=0 WHERE state='custody'",
            [],
        )
        .unwrap();
    fleet.node_mut("nodea").restart();
    let messages = fleet::wait_until(
        "real origin delivery after key theft",
        Duration::from_secs(90),
        || {
            let messages = read(fleet.node("nodeb"), &recipient["pane_id"]);
            (!messages.as_array()?.is_empty()).then_some(messages)
        },
    );
    assert_eq!(messages.as_array().unwrap().len(), 1);
    assert_eq!(messages[0]["correlation_id"], "real-key");
    let key = &original["result"]["message_key"];
    assert_eq!(
        database(fleet.node("nodeb"))
            .query_row(
                "SELECT count(*) FROM envelopes WHERE origin=?1 AND id=?2",
                rusqlite::params![
                    key["origin_node"].as_str().unwrap(),
                    key["message_id"].as_str().unwrap()
                ],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}

#[test]
fn live_handoff_resumes_pending_outbox_once() {
    let mut fleet = fleet::spawn("mesh-handoff", PAIR);
    let recipient = agent(fleet.node("nodeb"));
    discover(&fleet, &recipient);
    fleet.refuse_edge("nodea", "nodeb");
    fleet.kill_edge("nodea", "nodeb", Duration::from_secs(10));
    let sent = send(&fleet, &recipient, "handoff-custody");
    assert_eq!(sent["result"]["state"], "queued", "{sent}");
    let origin = fleet.node("nodea");
    let old_pid = origin.process_id();
    let result = request(origin, "server.live_handoff", json!({}));
    assert!(result.get("error").is_none(), "{result}");
    let replacement = fleet::wait_until("handoff replacement", Duration::from_secs(15), || {
        support::flock_server_pids_for_runtime_dir(&origin.runtime_dir)
            .ok()?
            .into_iter()
            .find(|pid| *pid != old_pid)
    });
    support::register_spawned_flock_pid(Some(replacement));
    // Keep teardown responsible for the replacement even if an assertion fails.
    struct Replacement(u32);
    impl Drop for Replacement {
        fn drop(&mut self) {
            unsafe {
                libc::kill(self.0 as libc::pid_t, libc::SIGTERM);
            }
            support::unregister_spawned_flock_pid(Some(self.0));
        }
    }
    let _replacement = Replacement(replacement);
    fleet.allow_edge("nodea", "nodeb");
    let messages = fleet::wait_until("handoff outbox replay", Duration::from_secs(150), || {
        let messages = read(fleet.node("nodeb"), &recipient["pane_id"]);
        (!messages.as_array()?.is_empty()).then_some(messages)
    });
    assert_eq!(messages.as_array().unwrap().len(), 1);
    assert_eq!(messages[0]["correlation_id"], "handoff-custody");
    let path = fleet
        .node("nodea")
        .home
        .join("state/flock-dev/mesh-mail.sqlite");
    fleet::wait_until(
        "durable handoff delivery receipt",
        Duration::from_secs(30),
        || {
            let db = rusqlite::Connection::open_with_flags(
                &path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .ok()?;
            let delivered: bool = db
                .query_row(
                    "SELECT delivered FROM envelopes WHERE correlation='handoff-custody'",
                    [],
                    |row| row.get(0),
                )
                .ok()?;
            delivered.then_some(())
        },
    );
    fleet.node_mut("nodeb").restart();
    assert_eq!(read(fleet.node("nodeb"), &recipient["pane_id"]), json!([]));
}

#[test]
fn cold_restart_quarantines_undecodable_mail_and_restores_valid_records() {
    let mut fleet = fleet::spawn(
        "mesh-quarantine",
        &[NodeSpec::new("nodea", "quarantine", &[])],
    );
    let recipient = agent(fleet.node("nodea"));
    for correlation in ["bad-metadata", "bad-payload", "valid-record"] {
        let sent = request(
            fleet.node("nodea"),
            "msg.send",
            json!({
                "to":{"type":"pane", "pane":recipient["pane_id"]},
                "body":"retained", "correlation_id":correlation, "intent":"fyi"
            }),
        );
        assert!(sent.get("error").is_none(), "{sent}");
    }
    let path = fleet
        .node("nodea")
        .home
        .join("state/flock-dev/mesh-mail.sqlite");
    {
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute(
            "UPDATE envelopes SET metadata='{}' WHERE correlation='bad-metadata'",
            [],
        )
        .unwrap();
        db.execute(
            "UPDATE envelopes SET body=X'FF' WHERE correlation='bad-payload'",
            [],
        )
        .unwrap();
    }
    fleet.node_mut("nodea").restart();
    let messages = read(fleet.node("nodea"), &recipient["pane_id"]);
    assert_eq!(messages.as_array().unwrap().len(), 1, "{messages}");
    assert_eq!(messages[0]["correlation_id"], "valid-record");
    let db =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let count: i64 = db
        .query_row(
            "SELECT count(*) FROM envelopes WHERE state='quarantined' AND length(body)>0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 2);
}
