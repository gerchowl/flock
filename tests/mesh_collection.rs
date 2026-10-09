#[path = "mesh_collection/diagnostics.rs"]
mod diagnostics;
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
                "SELECT count(*) FROM envelopes WHERE state='held'",
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
                    "SELECT count(*) FROM envelopes WHERE state='held'",
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
                "SELECT count(*) FROM envelopes WHERE state='held'",
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

fn enrolled(node: &Node, peer: &str) {
    fleet::wait_until("outbound edge enrolled", Duration::from_secs(30), || {
        request(node, "peers.enrollment", json!({}))["result"]["peers"]
            .as_array()?
            .iter()
            .find(|entry| {
                entry["peer"] == peer
                    && entry["source"] == "configured"
                    && entry["state"] == "pinned"
            })
            .cloned()
    });
}

#[test]
fn bidirectional_answers_push_immediately_without_waiting_for_collection() {
    let fleet = fleet::spawn(
        "mesh-push-answer",
        &[
            NodeSpec::new("nodea", "push-origin", &["nodeb"]),
            NodeSpec::new("nodeb", "push-target", &["nodea"]),
        ],
    );
    let (sender, _) = question(&fleet, "push");
    enrolled(fleet.node("nodeb"), "nodea");
    database(fleet.node("nodea"))
        .execute("UPDATE envelopes SET collect_at=999999999", [])
        .unwrap();
    let start = std::time::Instant::now();
    let sent = reply(fleet.node("nodeb"), "push", "immediate");
    assert!(sent.get("error").is_none(), "{sent}");
    let received = answers(fleet.node("nodea"), &sender["pane_id"]);
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "an enrolled reverse edge must push immediately"
    );
    assert_eq!(received[0]["body"], "immediate");
    fleet::wait_until("pushed reply receipt", Duration::from_secs(5), || {
        (database(fleet.node("nodeb"))
            .query_row(
                "SELECT state FROM envelopes WHERE request_id IS NOT NULL",
                [],
                |r| r.get::<_, String>(0),
            )
            .unwrap()
            == "delivered")
            .then_some(())
    });
}

#[test]
fn one_way_answer_arrives_on_the_first_fast_poll_without_a_waiter() {
    let fleet = fleet::spawn("mesh-fast-collect", PAIR);
    let (sender, _) = question(&fleet, "fast");
    let start = std::time::Instant::now();
    assert_eq!(
        reply(fleet.node("nodeb"), "fast", "five seconds")["result"]["state"],
        "held"
    );
    let received = answers(fleet.node("nodea"), &sender["pane_id"]);
    assert!(
        start.elapsed() < Duration::from_secs(8),
        "first collection must use the fast interval"
    );
    assert_eq!(received[0]["body"], "five seconds");
}

#[test]
fn a_nonresponding_collection_peer_cannot_starve_a_user_send_to_another_peer() {
    let fleet = fleet::spawn(
        "mesh-collect-lane",
        &[
            NodeSpec::new("nodea", "lane-origin", &["nodeb", "nodec"])
                .with_config("[msg]\ndeferral_relay_concurrency=1\n"),
            NodeSpec::new("nodeb", "lane-blocked", &[]),
            NodeSpec::new("nodec", "lane-healthy", &[]),
        ],
    );
    question(&fleet, "blocked-collection");
    let target = agent(fleet.node("nodec"));
    discover(&fleet, &target);
    let hold = fleet.base.join("hold-collect-nodea-nodeb");
    std::fs::create_dir(&hold).unwrap();
    ready(fleet.node("nodea"));
    fleet::wait_until(
        "collection blocked on offline transport",
        Duration::from_secs(10),
        || hold.join("entered").exists().then_some(()),
    );
    let start = std::time::Instant::now();
    let sent = send(&fleet, &target, "user-send");
    assert_eq!(sent["result"]["state"], "delivered", "{sent}");
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "background collection must not occupy the user relay lane"
    );
    std::fs::write(hold.join("release"), "").unwrap();
}

#[test]
fn invalid_collected_binding_is_terminal_and_visible_in_message_status() {
    let fleet = fleet::spawn("mesh-collect-failed", PAIR);
    question(&fleet, "invalid-answer");
    std::fs::write(fleet.base.join("corrupt-collect-answer-nodea-nodeb"), "").unwrap();
    reply(
        fleet.node("nodeb"),
        "invalid-answer",
        "bad binding in transit",
    );
    ready(fleet.node("nodea"));
    let status = fleet::wait_until("terminal import failure", Duration::from_secs(10), || {
        let status = request(
            fleet.node("nodea"),
            "msg.status",
            json!({"correlation_id":"invalid-answer"}),
        );
        (status["result"]["state"] == "collect_failed").then_some(status)
    });
    assert_eq!(status["result"]["detail"], "invalid reply binding");
    let failures: i64 = database(fleet.node("nodea"))
        .query_row(
            "SELECT collect_failures FROM envelopes WHERE correlation='invalid-answer'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(failures, 1, "permanent failures stop immediately");
}

#[test]
fn local_reply_survives_a_mesh_writer_failure() {
    let fleet = fleet::spawn(
        "mesh-local-writer-failure",
        &[NodeSpec::new("nodea", "local-writer", &[])],
    );
    let node = fleet.node("nodea");
    let target = agent(node);
    let sent = request(
        node,
        "msg.send",
        json!({
            "to":{"type":"pane","pane":target["pane_id"]},"body":"question","correlation_id":"local-outage","intent":"needs_reply"
        }),
    );
    assert!(sent.get("error").is_none(), "{sent}");
    database(node)
        .execute_batch(
            "CREATE TRIGGER refuse_answer BEFORE INSERT ON envelopes
         WHEN json_extract(NEW.metadata,'$.request_key') IS NOT NULL
         BEGIN SELECT RAISE(ABORT,'test mesh writer unavailable'); END;",
        )
        .unwrap();
    let response = reply(node, "local-outage", "local answer survives");
    assert_eq!(response["result"]["state"], "held", "{response}");
    let status = request(node, "msg.status", json!({"correlation_id":"local-outage"}));
    assert_eq!(
        status["result"]["reply"]["body"], "local answer survives",
        "{status}"
    );
}

#[test]
fn collection_policy_uses_the_stored_origin_name_not_the_current_ssh_alias() {
    let fleet = fleet::spawn("mesh-collect-alias",&[
        NodeSpec::new("nodea","alias-origin",&[]).with_config(
            "[msg]\nallow_from=['b-policy.test']\n[[peers]]\nname='b-policy.test'\nssh='nodeb'\n"),
        NodeSpec::new("nodeb","alias-target",&[]),
    ]);
    let (sender, _) = question(&fleet, "alias-answer");
    // The trusted node gains a new transport alias. Its stable policy name
    // remains the earlier configured pin, just as on mesh.deliver ingress.
    for app in ["flock", "flock-dev"] {
        let path = fleet
            .node("nodea")
            .config_home
            .join(app)
            .join("config.toml");
        let config = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            path,
            config.replace("name='b-policy.test'", "name='zz-transport.test'"),
        )
        .unwrap();
    }
    let reload = request(fleet.node("nodea"), "server.reload_config", json!({}));
    assert!(reload.get("error").is_none(), "{reload}");
    enrolled(fleet.node("nodea"), "zz-transport.test");
    reply(fleet.node("nodeb"), "alias-answer", "identity policy");
    ready(fleet.node("nodea"));
    let received = answers(fleet.node("nodea"), &sender["pane_id"]);
    assert_eq!(received[0]["body"], "identity policy");
    assert_eq!(received[0]["from_host"], "b-policy.test");
}

#[test]
fn an_active_reply_wait_shortens_a_backed_off_collection_deadline() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    let fleet = fleet::spawn("mesh-collect-wait", PAIR);
    question(&fleet, "waiting");
    database(fleet.node("nodea"))
        .execute(
            "UPDATE envelopes SET collect_at=300000,collect_attempts=3",
            [],
        )
        .unwrap();
    let mut waiter = UnixStream::connect(&fleet.node("nodea").api_socket).unwrap();
    waiter
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    writeln!(waiter,"{}",json!({"id":"wait","method":"msg.wait_reply","params":{"correlation_id":"waiting","timeout_ms":10000}})).unwrap();
    fleet::wait_until(
        "waiter requests fast collection",
        Duration::from_secs(5),
        || {
            (database(fleet.node("nodea"))
                .query_row(
                    "SELECT collect_at FROM envelopes WHERE correlation='waiting'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap()
                < 300000)
                .then_some(())
        },
    );
    reply(fleet.node("nodeb"), "waiting", "wait accelerated");
    let mut response = String::new();
    BufReader::new(waiter).read_line(&mut response).unwrap();
    let response: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["result"]["outcome"], "replied", "{response}");
    assert_eq!(response["result"]["reply"]["body"], "wait accelerated");
}

#[test]
fn reenrolling_the_edge_restores_fast_collection_after_backoff() {
    let fleet = fleet::spawn("mesh-collect-reenroll", PAIR);
    let (sender, _) = question(&fleet, "reenroll");
    database(fleet.node("nodea"))
        .execute(
            "UPDATE envelopes SET collect_at=300000,collect_attempts=3",
            [],
        )
        .unwrap();
    fleet.kill_edge("nodea", "nodeb", Duration::from_secs(10));
    fleet::wait_until(
        "reenrollment restores fast polling",
        Duration::from_secs(30),
        || {
            (database(fleet.node("nodea"))
                .query_row(
                    "SELECT collect_at FROM envelopes WHERE correlation='reenroll'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap()
                < 300000)
                .then_some(())
        },
    );
    reply(fleet.node("nodeb"), "reenroll", "edge is back");
    let start = std::time::Instant::now();
    let received = answers(fleet.node("nodea"), &sender["pane_id"]);
    assert!(start.elapsed() < Duration::from_secs(8));
    assert_eq!(received[0]["body"], "edge is back");
}

#[test]
fn idle_held_answers_make_no_commits_and_push_when_a_reverse_edge_enrolls() {
    let fleet = fleet::spawn("mesh-held-enroll", PAIR);
    let (sender, _) = question(&fleet, "held-enroll");
    // Both collection lanes must be unavailable to exercise an idle holder.
    fleet.refuse_edge("nodea", "nodeb");
    fleet.kill_edge("nodea", "nodeb", Duration::from_secs(10));
    let sent = reply(
        fleet.node("nodeb"),
        "held-enroll",
        "waiting for an outbound edge",
    );
    assert_eq!(sent["result"]["state"], "held", "{sent}");
    let db = database(fleet.node("nodeb"));
    assert_eq!(
        db.query_row(
            "SELECT state FROM envelopes WHERE request_id IS NOT NULL",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "held"
    );
    let version: i64 = db
        .pragma_query_value(None, "data_version", |r| r.get(0))
        .unwrap();
    let idle = std::time::Instant::now();
    fleet::wait_until("three idle retry ticks", Duration::from_secs(5), || {
        assert_eq!(
            db.pragma_query_value(None, "data_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            version,
            "held mail must not commit retry leases on an idle target"
        );
        (idle.elapsed() >= Duration::from_secs(3)).then_some(())
    });
    for app in ["flock", "flock-dev"] {
        let path = fleet
            .node("nodeb")
            .config_home
            .join(app)
            .join("config.toml");
        let mut config = std::fs::read_to_string(&path).unwrap();
        config.push_str("\n[[peers]]\nname='nodea'\nssh='nodea'\n");
        std::fs::write(path, config).unwrap();
    }
    let reload = request(fleet.node("nodeb"), "server.reload_config", json!({}));
    assert!(reload.get("error").is_none(), "{reload}");
    enrolled(fleet.node("nodeb"), "nodea");
    let start = std::time::Instant::now();
    let received = answers(fleet.node("nodea"), &sender["pane_id"]);
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "new enrollment must activate held answers promptly"
    );
    assert_eq!(received[0]["body"], "waiting for an outbound edge");
}

#[test]
fn many_held_answers_do_not_delay_the_targets_own_outbox_retry() {
    let mut fleet = fleet::spawn(
        "mesh-held-outbox",
        &[
            NodeSpec::new("nodea", "held-origin", &["nodeb"]),
            NodeSpec::new("nodeb", "held-target", &["nodec"])
                .with_config("\n[msg]\ndeferral_relay_concurrency=1\n"),
            NodeSpec::new("nodec", "outbox-recipient", &[]),
        ],
    );
    question(&fleet, "many-held");
    database(fleet.node("nodea"))
        .execute("UPDATE envelopes SET collect_at=999999999", [])
        .unwrap();
    fleet.refuse_edge("nodea", "nodeb");
    fleet.kill_edge("nodea", "nodeb", Duration::from_secs(10));
    for n in 0..130 {
        // Each sandbox boot has a fresh per-caller rate window. Keep the
        // production limiter intact while building a durable backlog.
        if n > 0 && n % 20 == 0 {
            fleet.node_mut("nodeb").restart();
        }
        let response = reply(fleet.node("nodeb"), "many-held", &format!("held {n}"));
        assert_eq!(response["result"]["state"], "held", "{response}");
    }
    let recipient = agent(fleet.node("nodec"));
    fleet::wait_until(
        "target discovers its own recipient",
        Duration::from_secs(90),
        || {
            request(fleet.node("nodeb"), "agent.list", json!({}))["result"]["fleet"]
                .as_array()?
                .iter()
                .find(|row| row["agent_id"] == recipient["agent_id"])
                .cloned()
        },
    );
    std::fs::write(fleet.base.join("capture-delivery-nodeb-nodec"), "drop").unwrap();
    let sent = request(
        fleet.node("nodeb"),
        "msg.send",
        json!({
            "to":{"type":"agent","agent":recipient["agent_id"]},
            "body":"own outbox", "correlation_id":"own-outbox", "intent":"fyi",
            "from_agent":agent(fleet.node("nodeb"))["agent_id"]
        }),
    );
    assert_eq!(sent["result"]["state"], "queued", "{sent}");
    std::fs::remove_file(fleet.base.join("capture-delivery-nodeb-nodec")).unwrap();
    database(fleet.node("nodeb"))
        .execute(
            "UPDATE envelopes SET retry_at=0,lease_until=0 WHERE correlation='own-outbox'",
            [],
        )
        .unwrap();
    let start = std::time::Instant::now();
    let delivered = answers(fleet.node("nodec"), &recipient["pane_id"]);
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "collect-only answers must not occupy the outbox retry lease"
    );
    assert_eq!(delivered[0]["body"], "own outbox");
    assert_eq!(
        database(fleet.node("nodeb"))
            .query_row(
                "SELECT count(*) FROM envelopes WHERE state='held'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        130
    );
}

#[test]
fn a_failed_reply_push_returns_to_held_without_idle_retry_commits() {
    let fleet = fleet::spawn(
        "mesh-push-hold",
        &[
            NodeSpec::new("nodea", "push-hold-origin", &["nodeb"]),
            NodeSpec::new("nodeb", "push-hold-target", &["nodea"]),
        ],
    );
    question(&fleet, "push-fails");
    enrolled(fleet.node("nodeb"), "nodea");
    database(fleet.node("nodea"))
        .execute("UPDATE envelopes SET collect_at=999999999", [])
        .unwrap();
    let db = database(fleet.node("nodeb"));
    let mut timeline = diagnostics::WriteTimeline::install(&db);
    timeline.sample(&db, "before reply");
    std::fs::write(fleet.base.join("capture-delivery-nodeb-nodea"), "capture").unwrap();
    let response = reply(fleet.node("nodeb"), "push-fails", "held after refusal");
    timeline.sample(&db, "reply returned");
    assert!(
        response.get("error").is_none(),
        "reply RPC failed: {response}"
    );
    fleet::wait_until(
        "failed push returns to held",
        Duration::from_secs(5),
        || {
            timeline.sample(&db, "waiting for held");
            (db.query_row(
                "SELECT state FROM envelopes WHERE request_id IS NOT NULL",
                [],
                |r| r.get::<_, String>(0),
            )
            .unwrap()
                == "held")
                .then_some(())
        },
    );
    // The question's read receipt rides nodeb's outbound collection from
    // nodea and is marked sent once. That is a delivery transition, not an
    // idle retry, so let it land before the idle window is measured.
    fleet::wait_until(
        "question receipt marked sent",
        Duration::from_secs(10),
        || {
            timeline.sample(&db, "waiting for receipt sent");
            db.query_row(
                "SELECT receipt_sent IS NOT NULL FROM envelopes WHERE request_id IS NULL",
                [],
                |r| r.get::<_, bool>(0),
            )
            .unwrap()
            .then_some(())
        },
    );
    let version = timeline.sample(&db, "idle baseline");
    let idle = std::time::Instant::now();
    fleet::wait_until(
        "failed answer remains collect-only",
        Duration::from_secs(5),
        || {
            assert_eq!(
                timeline.sample(&db, "idle observation"),
                version,
                "idle database committed after failed reply returned to held (see write timeline)"
            );
            assert_eq!(
                db.query_row(
                    "SELECT lease_until FROM envelopes WHERE request_id IS NOT NULL",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                0,
                "held reply acquired an idle retry lease (see write timeline)"
            );
            (idle.elapsed() >= Duration::from_secs(3)).then_some(())
        },
    );
}

#[test]
fn custody_status_read_receipt_and_wait_survive_audit_rotation_and_restart() {
    use std::io::{BufRead, Write};
    let mut fleet = fleet::spawn("mesh-status", PAIR);
    let (_, recipient) = question(&fleet, "durable-status");
    let origin = fleet.node("nodea");
    let status = request(
        origin,
        "msg.status",
        json!({"correlation_id":"durable-status"}),
    );
    assert_eq!(status["result"]["state"], "delivered", "{status}");
    let reference = status["result"]["reference"].clone();
    assert_eq!(
        cli_status(origin, "durable-status")["result"]["state"],
        "delivered"
    );
    let inbox = read(fleet.node("nodeb"), &recipient["pane_id"]);
    assert_eq!(inbox[0]["replyable"], true);
    assert_eq!(inbox[0]["reply_contract"], "durable_return_binding");
    ready(origin);
    fleet::wait_until("read receipt at origin", Duration::from_secs(30), || {
        let status = request(
            origin,
            "msg.status",
            json!({"correlation_id":"durable-status"}),
        );
        (status["result"]["state"] == "read").then_some(())
    });
    assert_eq!(
        cli_status(origin, "durable-status")["result"]["state"],
        "read"
    );
    let mut stream = std::os::unix::net::UnixStream::connect(&origin.api_socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(35)))
        .unwrap();
    writeln!(
        stream,
        "{}",
        json!({"id":"waiting","method":"msg.wait_reply","params":{
            "correlation_id":"durable-status","timeout_ms":30000,"reference":reference
        }})
    )
    .unwrap();
    let result = reply(fleet.node("nodeb"), "durable-status", "durable answer");
    assert_eq!(result["result"]["state"], "held", "{result}");
    ready(origin);
    let mut line = String::new();
    std::io::BufReader::new(stream)
        .read_line(&mut line)
        .unwrap();
    let waited: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(waited["result"]["outcome"], "replied", "{waited}");
    assert_eq!(waited["result"]["reply"]["body"], "durable answer");
    fleet.node_mut("nodea").stop();
    let log = fleet
        .node("nodea")
        .config_home
        .join("flock-dev/event-log.jsonl");
    assert!(log.exists(), "{}", log.display());
    std::fs::remove_file(log).unwrap();
    fleet.node_mut("nodea").restart();
    let restored = request(
        fleet.node("nodea"),
        "msg.status",
        json!({
            "correlation_id":"durable-status","reference":reference
        }),
    );
    assert_eq!(
        restored["result"]["reply"]["body"], "durable answer",
        "{restored}"
    );
}

// This integration probe executes the real CLI against only its isolated server.
#[allow(clippy::disallowed_methods)]
fn cli_status(node: &Node, correlation: &str) -> Value {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_flk"))
        .env_clear()
        .env("HOME", &node.home)
        .env("XDG_CONFIG_HOME", &node.config_home)
        .env("FLOCK_SOCKET_PATH", &node.api_socket)
        .args(["msg", "status", correlation])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
