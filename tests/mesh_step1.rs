//! Step-1 acceptance: real sandbox servers and agent CLI calls over one-hop edges.
mod support;

use serde_json::{json, Value};
use std::time::{Duration, Instant};
use support::fleet::{self, Fleet, Node, NodeSpec};

const DEADLINE: Duration = Duration::from_secs(30);
const DIRECT: &[NodeSpec] = &[
    NodeSpec::new("nodea", "accept-origin", &["nodeb"]),
    NodeSpec::new("nodeb", "accept-target", &[]),
];
const SPOKE: &[NodeSpec] = &[
    NodeSpec::new("nodeb", "accept-hub", &["nodea"]),
    NodeSpec::new("nodea", "accept-spoke", &[]),
];

fn api(node: &Node, method: &str, params: Value) -> Value {
    let response: Value = serde_json::from_str(
        &node.api(&json!({"id":"acceptance","method":method,"params":params}).to_string()),
    )
    .unwrap();
    assert!(
        response.get("error").is_none(),
        "{} {method}: {response}",
        node.name
    );
    response["result"].clone()
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

struct Agent {
    id: String,
    pane: String,
}

impl Agent {
    fn start(node: &Node) -> Self {
        let started = api(
            node,
            "agent.start",
            json!({
                "name":"acceptance", "argv":["/bin/sh"], "cwd":node.repo
            }),
        );
        Self {
            id: started["agent"]["agent_id"].as_str().unwrap().into(),
            pane: started["agent"]["pane_id"].as_str().unwrap().into(),
        }
    }

    // Execute inside the real agent ancestry, including on an edge-less spoke.
    fn cli(&self, node: &Node, args: &[&str]) -> Value {
        cli_result(&self.start_cli(node, args))
    }

    fn start_cli(&self, node: &Node, args: &[&str]) -> std::path::PathBuf {
        let output = node.home.join("acceptance-cli.json");
        let _ = std::fs::remove_file(&output);
        let command = std::iter::once(env!("CARGO_BIN_EXE_flk"))
            .chain(args.iter().copied())
            .map(quote)
            .collect::<Vec<_>>()
            .join(" ");
        api(
            node,
            "pane.send_text",
            json!({
                "pane_id":self.pane,
                "text":format!("{command} >{}\n", quote(output.to_str().unwrap()))
            }),
        );
        output
    }

    fn shell_pid(&self, node: &Node) -> Value {
        let output = node.home.join("acceptance-pid.json");
        let _ = std::fs::remove_file(&output);
        api(
            node,
            "pane.send_text",
            json!({
                "pane_id":self.pane,
                "text":format!("printf '{{\"result\":{{\"pid\":%s}}}}\\n' \"$$\" >{}\n", quote(output.to_str().unwrap()))
            }),
        );
        cli_result(&output)["pid"].clone()
    }
}

fn cli_result(output: &std::path::Path) -> Value {
    let response: Value = fleet::wait_until("agent CLI result", DEADLINE, || {
        serde_json::from_slice(&std::fs::read(output).ok()?).ok()
    });
    assert!(response.get("error").is_none(), "{response}");
    response["result"].clone()
}

struct Conversation {
    fleet: Fleet,
    sender: Agent,
    receiver: Agent,
}

impl Conversation {
    fn new(specs: &[NodeSpec]) -> Self {
        let fleet = fleet::spawn("s1", specs);
        let sender = Agent::start(fleet.node("nodea"));
        let receiver = Agent::start(fleet.node("nodeb"));
        fleet::wait_until("recipient discovery", DEADLINE, || {
            api(fleet.node("nodea"), "agent.list", json!({}))["fleet"]
                .as_array()?
                .iter()
                .any(|entry| entry["agent_id"] == receiver.id)
                .then_some(())
        });
        Self {
            fleet,
            sender,
            receiver,
        }
    }

    fn send(&self) -> Value {
        self.sender.cli(
            self.fleet.node("nodea"),
            &[
                "msg",
                "send",
                "--agent",
                &self.receiver.id,
                "--intent",
                "needs-reply",
                "--correlation-id",
                "question",
                "question body",
            ],
        )
    }

    fn reply(&self) -> Value {
        self.receiver.cli(
            self.fleet.node("nodeb"),
            &["msg", "reply", "question", "answer body"],
        )
    }

    fn read_question(&self) {
        let messages = mail(self.fleet.node("nodeb"), &self.receiver.pane);
        assert_eq!(messages.as_array().unwrap().len(), 1, "{messages}");
        assert_eq!(messages[0]["body"], "question body");
        assert_eq!(messages[0]["from_agent"], self.sender.id);
        assert_eq!(messages[0]["reply_contract"], "durable_return_binding");
        state(self.fleet.node("nodeb"), "question", "read");
    }

    fn answer_once(&self) {
        let messages = mail(self.fleet.node("nodea"), &self.sender.pane);
        assert_eq!(messages.as_array().unwrap().len(), 1, "{messages}");
        assert_eq!(messages[0]["body"], "answer body");
        assert_eq!(messages[0]["in_reply_to"], "question");
        let answer = messages[0]["correlation_id"].as_str().unwrap();
        state(self.fleet.node("nodea"), answer, "read");
        let waited = api(
            self.fleet.node("nodea"),
            "msg.wait_reply",
            json!({
                "correlation_id":"question", "timeout_ms":0
            }),
        );
        assert_eq!(waited["outcome"], "replied", "{waited}");
        assert_eq!(waited["reply"]["body"], "answer body");
        assert_eq!(read(self.fleet.node("nodea"), &self.sender.pane), json!([]));
        assert_eq!(
            read(self.fleet.node("nodeb"), &self.receiver.pane),
            json!([])
        );
    }
}

fn status(node: &Node, correlation: &str) -> Value {
    api(node, "msg.status", json!({"correlation_id":correlation}))
}

fn audit_event(node: &Node, event: &str, field: &str, correlation: &str) -> Value {
    fleet::wait_until("server audit event", DEADLINE, || {
        std::fs::read_to_string(node.config_home.join("flock-dev/event-log.jsonl"))
            .ok()?
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|row| {
                row["envelope"]["event"] == event && row["envelope"]["data"][field] == correlation
            })
    })
}

fn delivery_attempts(fleet: &Fleet, from: &str, to: &str, correlation: &str) -> usize {
    std::fs::read_to_string(fleet.base.join(format!("delivery-attempts-{from}-{to}")))
        .unwrap_or_default()
        .lines()
        .filter(|line| *line == correlation)
        .count()
}

fn state(node: &Node, correlation: &str, expected: &str) -> Value {
    fleet::wait_until(
        &format!("{} {correlation} becomes {expected}", node.name),
        DEADLINE,
        || {
            let response: Value = serde_json::from_str(
                &node.api(
                    &json!({
                        "id":"state", "method":"msg.status", "params":{"correlation_id":correlation}
                    })
                    .to_string(),
                ),
            )
            .unwrap();
            if response["error"]["code"] == "message_not_found" {
                return None;
            }
            assert!(response.get("error").is_none(), "{response}");
            let result = response["result"].clone();
            (result["state"] == expected).then_some(result)
        },
    )
}

fn read(node: &Node, pane: &str) -> Value {
    api(node, "msg.read", json!({"pane":pane}))["messages"].clone()
}

fn mail(node: &Node, pane: &str) -> Value {
    fleet::wait_until("inbox delivery", DEADLINE, || {
        let messages = read(node, pane);
        (!messages.as_array()?.is_empty()).then_some(messages)
    })
}

fn db(node: &Node) -> rusqlite::Connection {
    let db =
        rusqlite::Connection::open(node.home.join("state/flock-dev/mesh-mail.sqlite")).unwrap();
    db.busy_timeout(Duration::from_secs(5)).unwrap();
    db
}

fn cut(fleet: &Fleet, from: &str, to: &str) {
    fleet.refuse_edge(from, to);
    fleet.kill_edge(from, to, Duration::from_secs(10));
}

fn reconnect(fleet: &Fleet, from: &str, to: &str) {
    fleet.allow_edge(from, to);
    fleet::wait_until("replacement edge enrolled", DEADLINE, || {
        api(fleet.node(from), "peers.enrollment", json!({}))["peers"]
            .as_array()?
            .iter()
            .any(|peer| peer["peer"] == to && peer["state"] == "pinned")
            .then_some(())
    });
}

// Advance fixture deadlines only after a deliberate partition. Production
// backoff remains unchanged, and every delivery still crosses the real edge.
fn due(node: &Node) {
    db(node)
        .execute("UPDATE envelopes SET collect_at=0,retry_at=0", [])
        .unwrap();
}

#[test]
fn incident_direct_held_collected_read_without_an_active_waiter() {
    let conversation = Conversation::new(DIRECT);
    assert_eq!(conversation.send()["state"], "delivered");
    state(conversation.fleet.node("nodea"), "question", "delivered");
    conversation.read_question();
    let reply = conversation.reply();
    assert_eq!(reply["state"], "held", "{reply}");
    state(
        conversation.fleet.node("nodeb"),
        reply["correlation_id"].as_str().unwrap(),
        "held",
    );
    conversation.answer_once();
    state(
        conversation.fleet.node("nodeb"),
        reply["correlation_id"].as_str().unwrap(),
        "collected",
    );
    state(conversation.fleet.node("nodea"), "question", "read");
}

#[test]
fn incident_reverse_edge_pushes_within_two_seconds() {
    let conversation = Conversation::new(&[
        NodeSpec::new("nodea", "accept-origin", &["nodeb"]),
        NodeSpec::new("nodeb", "accept-target", &["nodea"]),
    ]);
    fleet::wait_until("reverse enrollment", DEADLINE, || {
        api(
            conversation.fleet.node("nodeb"),
            "peers.enrollment",
            json!({}),
        )["peers"]
            .as_array()?
            .iter()
            .any(|p| p["source"] == "configured" && p["state"] == "pinned")
            .then_some(())
    });
    conversation.send();
    conversation.read_question();
    // Disable collection so only the reverse push can satisfy the assertion.
    db(conversation.fleet.node("nodea"))
        .execute("UPDATE envelopes SET collect_at=999999999", [])
        .unwrap();
    let reply = conversation.reply();
    conversation.answer_once();
    let correlation = reply["correlation_id"].as_str().unwrap();
    let accepted = audit_event(
        conversation.fleet.node("nodeb"),
        "message_replied",
        "reply_correlation_id",
        correlation,
    );
    let imported = audit_event(
        conversation.fleet.node("nodea"),
        "message_queued",
        "correlation_id",
        correlation,
    );
    assert!(
        imported["ts_ms"]
            .as_u64()
            .unwrap()
            .saturating_sub(accepted["ts_ms"].as_u64().unwrap())
            < 2_000
    );
}

#[test]
fn incident_spoke_outbox_is_collected_and_answer_is_pushed_down() {
    let conversation = Conversation::new(SPOKE);
    cut(&conversation.fleet, "nodeb", "nodea");
    assert_eq!(conversation.send()["state"], "queued");
    state(conversation.fleet.node("nodea"), "question", "queued");
    conversation.fleet.allow_edge("nodeb", "nodea");
    state(conversation.fleet.node("nodea"), "question", "delivered");
    conversation.read_question();
    state(conversation.fleet.node("nodea"), "question", "read");
    let reply = conversation.reply();
    assert_eq!(reply["state"], "held");
    let correlation = reply["correlation_id"].as_str().unwrap();
    state(conversation.fleet.node("nodeb"), correlation, "collected");
    fleet::wait_until("push-down inbox before read", DEADLINE, || {
        api(
            conversation.fleet.node("nodea"),
            "msg.list",
            json!({"pane":conversation.sender.pane}),
        )["messages"]
            .as_array()?
            .iter()
            .any(|m| m["correlation_id"] == correlation)
            .then_some(())
    });
    conversation.answer_once();
}

#[test]
fn failed_receipt_commit_backs_off_on_a_healthy_edge() {
    let conversation = Conversation::new(SPOKE);
    conversation.send();
    state(conversation.fleet.node("nodea"), "question", "delivered");
    let hub = db(conversation.fleet.node("nodeb"));
    fleet::wait_until("initial delivery receipt committed", DEADLINE, || {
        hub.query_row(
            "SELECT receipt_sent='delivered' FROM envelopes WHERE correlation='question'",
            [],
            |r| r.get::<_, bool>(0),
        )
        .ok()?
        .then_some(())
    });
    hub.execute_batch(
        "CREATE TRIGGER fail_receipt BEFORE UPDATE OF receipt_sent ON envelopes
        BEGIN SELECT RAISE(FAIL, 'receipt write fault'); END;",
    )
    .unwrap();
    let polls = conversation.fleet.base.join("receipt-polls-nodeb-nodea");
    let count = || {
        std::fs::read_to_string(&polls)
            .unwrap_or_default()
            .lines()
            .count()
    };
    let before = count();
    conversation.read_question();
    state(conversation.fleet.node("nodea"), "question", "read");
    fleet::wait_until(
        "receipt send despite local commit failure",
        DEADLINE,
        || (count() > before).then_some(()),
    );
    let after = count();
    let started = Instant::now();
    fleet::wait_until(
        "failed receipt backoff across two worker ticks",
        DEADLINE,
        || {
            assert_eq!(count(), after);
            assert_eq!(
                status(conversation.fleet.node("nodea"), "question")["state"],
                "read"
            );
            (started.elapsed() >= Duration::from_secs(2)).then_some(())
        },
    );
    assert_eq!(
        hub.query_row(
            "SELECT receipt_sent FROM envelopes WHERE correlation='question'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "delivered"
    );
    hub.execute_batch("DROP TRIGGER fail_receipt").unwrap();
    conversation.reply();
    conversation.answer_once();
}

#[test]
fn queued_request_and_held_answer_survive_offline_reconnect() {
    let mut conversation = Conversation::new(DIRECT);
    cut(&conversation.fleet, "nodea", "nodeb");
    assert_eq!(conversation.send()["state"], "queued");
    state(conversation.fleet.node("nodea"), "question", "queued");
    reconnect(&conversation.fleet, "nodea", "nodeb");
    due(conversation.fleet.node("nodea"));
    conversation.read_question();
    cut(&conversation.fleet, "nodea", "nodeb");
    let reply = conversation.reply();
    assert_eq!(reply["state"], "held");
    conversation.fleet.node_mut("nodeb").restart();
    state(
        conversation.fleet.node("nodeb"),
        reply["correlation_id"].as_str().unwrap(),
        "held",
    );
    reconnect(&conversation.fleet, "nodea", "nodeb");
    due(conversation.fleet.node("nodea"));
    conversation.answer_once();
}

fn restart_role(specs: &[NodeSpec], role: &str) {
    let mut conversation = Conversation::new(specs);
    conversation.send();
    conversation.read_question();
    conversation.fleet.node_mut(role).restart();
    state(conversation.fleet.node("nodeb"), "question", "read");
    // A cold restart restores the conversation, not the old CLI process.
    api(
        conversation.fleet.node("nodeb"),
        "msg.reply",
        json!({
            "correlation_id":"question", "body":"answer body"
        }),
    );
    due(conversation.fleet.node("nodea"));
    conversation.answer_once();
}

#[test]
fn cold_restart_sender_mid_conversation() {
    restart_role(DIRECT, "nodea");
}

#[test]
fn cold_restart_receiver_mid_conversation() {
    restart_role(DIRECT, "nodeb");
}

#[test]
fn cold_restart_hub_mid_conversation() {
    restart_role(SPOKE, "nodeb");
}

#[test]
fn lost_collect_ack_reimports_once_after_edge_is_killed() {
    let mut conversation = Conversation::new(DIRECT);
    conversation.send();
    conversation.read_question();
    let gate = conversation.fleet.base.join("gate-collect-ack-nodea-nodeb");
    std::fs::create_dir(&gate).unwrap();
    conversation.reply();
    due(conversation.fleet.node("nodea"));
    fleet::wait_until("collection ack after import", DEADLINE, || {
        gate.join("entered").exists().then_some(())
    });
    cut(&conversation.fleet, "nodea", "nodeb");
    conversation.answer_once();
    std::fs::remove_dir_all(&gate).unwrap();
    reconnect(&conversation.fleet, "nodea", "nodeb");
    due(conversation.fleet.node("nodea"));
    conversation.fleet.node_mut("nodea").restart();
    let reply = status(conversation.fleet.node("nodea"), "question")["reply"]["correlation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    state(conversation.fleet.node("nodeb"), &reply, "collected");
    assert_eq!(
        read(conversation.fleet.node("nodea"), &conversation.sender.pane),
        json!([])
    );
}

#[test]
fn real_answer_supersedes_a_deferral() {
    let conversation = Conversation::new(DIRECT);
    conversation.send();
    api(
        conversation.fleet.node("nodeb"),
        "msg.mute",
        json!({
            "pane":conversation.receiver.pane,"seconds":600,"reason":"busy"
        }),
    );
    due(conversation.fleet.node("nodea"));
    let deferred = mail(conversation.fleet.node("nodea"), &conversation.sender.pane);
    assert_eq!(deferred.as_array().unwrap().len(), 1);
    let waited = api(
        conversation.fleet.node("nodea"),
        "msg.wait_reply",
        json!({"correlation_id":"question","timeout_ms":0}),
    );
    assert_eq!(waited["outcome"], "deferred", "{waited}");
    conversation.read_question();
    conversation.reply();
    due(conversation.fleet.node("nodea"));
    conversation.answer_once();
}

#[test]
fn lost_delivery_ack_and_duplicate_imports_preserve_one_message() {
    let conversation = Conversation::new(DIRECT);
    let gate = conversation
        .fleet
        .base
        .join("gate-delivery-ack-nodea-nodeb");
    std::fs::create_dir(&gate).unwrap();
    let output = conversation.sender.start_cli(
        conversation.fleet.node("nodea"),
        &[
            "msg",
            "send",
            "--agent",
            &conversation.receiver.id,
            "--intent",
            "needs-reply",
            "--correlation-id",
            "question",
            "question body",
        ],
    );
    fleet::wait_until("delivery ack after inbox commit", DEADLINE, || {
        gate.join("entered").exists().then_some(())
    });
    conversation.read_question();
    cut(&conversation.fleet, "nodea", "nodeb");
    assert_eq!(cli_result(&output)["state"], "queued");
    std::fs::remove_dir_all(gate).unwrap();
    // The origin retries the committed key after the acknowledgement was lost.
    reconnect(&conversation.fleet, "nodea", "nodeb");
    due(conversation.fleet.node("nodea"));
    state(conversation.fleet.node("nodea"), "question", "delivered");
    assert!(conversation
        .fleet
        .base
        .join("delivered-receipt-nodea-nodeb")
        .exists());
    assert_eq!(
        read(
            conversation.fleet.node("nodeb"),
            &conversation.receiver.pane
        ),
        json!([])
    );
    conversation.reply();
    due(conversation.fleet.node("nodea"));
    conversation.answer_once();
}

#[test]
fn multihop_chain_is_refused_with_forward_limit() {
    let fleet = fleet::spawn(
        "s1chain",
        &[
            NodeSpec::new("nodea", "accept-laptop", &["nodeb"]),
            NodeSpec::new("nodeb", "accept-hub", &["nodec"]),
            NodeSpec::new("nodec", "accept-spoke", &[]),
        ],
    );
    let sender = Agent::start(fleet.node("nodea"));
    let recipient = Agent::start(fleet.node("nodec"));
    fleet::wait_until("two-hop recipient discovery", DEADLINE, || {
        api(fleet.node("nodea"), "agent.list", json!({}))["fleet"]
            .as_array()?
            .iter()
            .any(|a| a["agent_id"] == recipient.id)
            .then_some(())
    });
    let sent = sender.cli(
        fleet.node("nodea"),
        &[
            "msg",
            "send",
            "--agent",
            &recipient.id,
            "--intent",
            "needs-reply",
            "--correlation-id",
            "too-far",
            "not forwarded",
        ],
    );
    assert_eq!(sent["state"], "refused", "{sent}");
    assert!(
        sent["warnings"].to_string().contains("forward_limit"),
        "{sent}"
    );
    assert_eq!(
        state(fleet.node("nodea"), "too-far", "refused")["detail"],
        "forward_limit"
    );
    let refused = audit_event(
        fleet.node("nodea"),
        "message_delivered",
        "correlation_id",
        "too-far",
    );
    assert_eq!(refused["envelope"]["data"]["delivered"], false);
    assert_eq!(
        refused["envelope"]["data"]["outcome"],
        "refused: forward_limit"
    );
    let attempts = delivery_attempts(&fleet, "nodea", "nodeb", "too-far");
    assert_eq!(attempts, 1);
    // A retryable probe proves the retry worker actually ran after due().
    std::fs::write(fleet.base.join("transient-delivery-nodea-nodeb"), "").unwrap();
    let probe = api(
        fleet.node("nodea"),
        "msg.send",
        json!({
            "to":{"type":"agent","agent":recipient.id}, "from_agent":sender.id,
            "body":"retry probe", "correlation_id":"retry-probe", "intent":"fyi"
        }),
    );
    assert_eq!(probe["state"], "queued");
    due(fleet.node("nodea"));
    fleet::wait_until("forced retry worker attempt", DEADLINE, || {
        (delivery_attempts(&fleet, "nodea", "nodeb", "retry-probe") >= 2).then_some(())
    });
    assert_eq!(
        delivery_attempts(&fleet, "nodea", "nodeb", "too-far"),
        attempts
    );
    assert_eq!(status(fleet.node("nodea"), "too-far")["state"], "refused");
    assert_eq!(read(fleet.node("nodec"), &recipient.pane), json!([]));
}

#[test]
fn old_mesh_version_retains_origin_custody_without_legacy_delivery() {
    let mut conversation = Conversation::new(DIRECT);
    conversation
        .fleet
        .node_mut("nodeb")
        .restart_with_mesh(fleet::MeshMode::VersionMismatch(0));
    fleet::wait_until("old protocol refused", DEADLINE, || {
        let enrollment = api(
            conversation.fleet.node("nodea"),
            "peers.enrollment",
            json!({}),
        );
        enrollment["peers"]
            .as_array()?
            .iter()
            .any(|peer| {
                peer["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains("upgrade flk on nodeb"))
            })
            .then_some(())
    });
    let sent = conversation.send();
    assert_eq!(sent["state"], "queued", "{sent}");
    assert!(
        sent["warnings"]
            .to_string()
            .contains("upgrade flk on nodeb"),
        "{sent}"
    );
    state(conversation.fleet.node("nodea"), "question", "queued");
    assert_eq!(
        read(
            conversation.fleet.node("nodeb"),
            &conversation.receiver.pane
        ),
        json!([])
    );
    assert!(!conversation
        .fleet
        .base
        .join("legacy-message-nodea-nodeb")
        .exists());
    due(conversation.fleet.node("nodea"));
    fleet::wait_until("old peer forced retry completed", DEADLINE, || {
        let retry: i64 = db(conversation.fleet.node("nodea"))
            .query_row(
                "SELECT retry_at FROM envelopes WHERE correlation='question'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        (retry > 0).then_some(())
    });
    assert_eq!(
        read(
            conversation.fleet.node("nodeb"),
            &conversation.receiver.pane
        ),
        json!([])
    );
    assert!(!conversation
        .fleet
        .base
        .join("legacy-message-nodea-nodeb")
        .exists());
}

struct Replacement(u32);

impl Drop for Replacement {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.0 as libc::pid_t, libc::SIGTERM);
        }
        support::unregister_spawned_flock_pid(Some(self.0));
    }
}

fn handoff_role(specs: &[NodeSpec], role: &str) {
    let conversation = Conversation::new(specs);
    conversation.send();
    conversation.read_question();
    let node = conversation.fleet.node(role);
    let before = api(node, "pane.list", json!({}));
    let agent = if role == "nodea" {
        &conversation.sender
    } else {
        &conversation.receiver
    };
    let shell_pid = agent.shell_pid(node);
    let pane_identity = |panes: &Value| {
        panes["panes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pane| (pane["pane_id"].clone(), pane["workspace_id"].clone()))
            .collect::<Vec<_>>()
    };
    api(node, "server.live_handoff", json!({}));
    let replacement = fleet::wait_until("sandbox handoff replacement", DEADLINE, || {
        support::flock_server_pids_for_runtime_dir(&node.runtime_dir)
            .ok()?
            .into_iter()
            .find(|pid| *pid != node.process_id())
    });
    support::register_spawned_flock_pid(Some(replacement));
    let _replacement = Replacement(replacement);
    // Wait for the replacement's API rather than racing socket takeover.
    fleet::wait_until("handoff API listener", DEADLINE, || {
        use std::io::{BufRead, Write};
        let mut stream = std::os::unix::net::UnixStream::connect(&node.api_socket).ok()?;
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .ok()?;
        writeln!(
            stream,
            "{}",
            json!({"id":"ready","method":"ping","params":{}})
        )
        .ok()?;
        let mut response = String::new();
        std::io::BufReader::new(stream)
            .read_line(&mut response)
            .ok()?;
        serde_json::from_str::<Value>(&response)
            .ok()?
            .get("result")
            .map(|_| ())
    });
    assert_eq!(
        pane_identity(&before),
        pane_identity(&api(node, "pane.list", json!({})))
    );
    assert_eq!(
        agent.shell_pid(node),
        shell_pid,
        "handoff must preserve the running pane process"
    );
    // Prove the retained terminal still runs CLI commands after writer transfer.
    conversation.reply();
    due(conversation.fleet.node("nodea"));
    conversation.answer_once();
}

#[test]
fn live_handoff_sender_keeps_panes_and_delivers_once() {
    handoff_role(DIRECT, "nodea");
}

#[test]
fn live_handoff_receiver_keeps_panes_and_delivers_once() {
    handoff_role(DIRECT, "nodeb");
}

#[test]
fn live_handoff_hub_keeps_panes_and_delivers_once() {
    handoff_role(SPOKE, "nodeb");
}

#[test]
fn audit_rotation_during_held_conversation_loses_nothing() {
    use std::io::Write;
    let mut conversation = Conversation::new(DIRECT);
    conversation.send();
    conversation.read_question();
    cut(&conversation.fleet, "nodea", "nodeb");
    assert_eq!(conversation.reply()["state"], "held");
    conversation.fleet.node_mut("nodeb").stop();
    let log = conversation
        .fleet
        .node("nodeb")
        .config_home
        .join("flock-dev/event-log.jsonl");
    // ADR-0005 has fixed rotation bounds, not a configuration knob. JSON
    // whitespace seeds the real byte threshold without inventing audit events.
    let mut file = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
    // Extend the last valid JSON line, rather than adding a malformed record.
    file.set_len(file.metadata().unwrap().len() - 1).unwrap();
    file.write_all(&vec![b' '; 32 * 1024 * 1024]).unwrap();
    file.write_all(b"\n").unwrap();
    drop(file);
    conversation.fleet.node_mut("nodeb").restart();
    api(
        conversation.fleet.node("nodeb"),
        "workspace.create",
        json!({"cwd":conversation.fleet.node("nodeb").repo}),
    );
    fleet::wait_until("real audit rotation", DEADLINE, || {
        log.with_file_name("event-log.1.jsonl")
            .exists()
            .then_some(())
    });
    reconnect(&conversation.fleet, "nodea", "nodeb");
    due(conversation.fleet.node("nodea"));
    conversation.answer_once();
}

#[test]
fn fleet_pause_freezes_retries_and_ttl_then_resumes() {
    let mut conversation = Conversation::new(DIRECT);
    conversation.send();
    conversation.read_question();
    cut(&conversation.fleet, "nodea", "nodeb");
    let queued = conversation.sender.cli(
        conversation.fleet.node("nodea"),
        &[
            "msg",
            "send",
            "--agent",
            &conversation.receiver.id,
            "--intent",
            "fyi",
            "--correlation-id",
            "retry",
            "retry body",
        ],
    );
    assert_eq!(queued["state"], "queued");
    api(conversation.fleet.node("nodea"), "fleet.pause", json!({}));
    conversation.reply();
    api(conversation.fleet.node("nodeb"), "fleet.pause", json!({}));
    let held_db = db(conversation.fleet.node("nodeb"));
    let held_snapshot = || {
        held_db
            .query_row(
                "SELECT elapsed,custody_deadline FROM clock,envelopes WHERE state='held'",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .unwrap()
    };
    let held_frozen = held_snapshot();
    due(conversation.fleet.node("nodea"));
    let database = db(conversation.fleet.node("nodea"));
    fleet::wait_until("durable pause", DEADLINE, || {
        database
            .query_row("SELECT paused FROM clock", [], |row| row.get::<_, bool>(0))
            .unwrap()
            .then_some(())
    });
    let snapshot = || {
        database.query_row(
        "SELECT elapsed,sum(collect_attempts),sum(custody_deadline),sum(retry_at),sum(lease_until) FROM clock,envelopes",
        [], |row| Ok((row.get::<_, i64>(0)?,row.get::<_, i64>(1)?,row.get::<_, i64>(2)?,row.get::<_, i64>(3)?,row.get::<_, i64>(4)?))
    ).unwrap()
    };
    let frozen = snapshot();
    conversation.fleet.allow_edge("nodea", "nodeb");
    conversation.fleet.node_mut("nodea").restart();
    let started = Instant::now();
    fleet::wait_until("paused clock across two ticks", DEADLINE, || {
        assert_eq!(snapshot(), frozen);
        assert_eq!(held_snapshot(), held_frozen);
        assert_eq!(
            read(conversation.fleet.node("nodea"), &conversation.sender.pane),
            json!([])
        );
        assert_eq!(
            read(
                conversation.fleet.node("nodeb"),
                &conversation.receiver.pane
            ),
            json!([])
        );
        (started.elapsed() >= Duration::from_secs(2)).then_some(())
    });
    api(conversation.fleet.node("nodeb"), "fleet.resume", json!({}));
    api(conversation.fleet.node("nodea"), "fleet.resume", json!({}));
    let retried = mail(
        conversation.fleet.node("nodeb"),
        &conversation.receiver.pane,
    );
    assert_eq!(retried.as_array().unwrap().len(), 1);
    assert_eq!(retried[0]["body"], "retry body");
    conversation.answer_once();
}
