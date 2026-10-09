//! Step-2 acceptance through isolated real servers, held edges and custody stores.
mod support;

use serde_json::{json, Value};
use std::time::{Duration, Instant};
use support::fleet::{self, Fleet, Node, NodeSpec};

const DEADLINE: Duration = Duration::from_secs(30);
const DIRECT: &[NodeSpec] = fleet::ONE_WAY_PAIR;

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
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let started = api(
            node,
            "agent.start",
            json!({
                "name":format!("acceptance-{}", NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)), "argv":["/bin/sh"], "cwd":node.repo
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
    target: String,
}

impl Conversation {
    fn new(specs: &[NodeSpec]) -> Self {
        Self::to(specs, "nodeb")
    }

    fn to(specs: &[NodeSpec], target: &str) -> Self {
        let fleet = fleet::spawn("s2", specs);
        let sender = Agent::start(fleet.node("nodea"));
        let receiver = Agent::start(fleet.node(target));
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
            target: target.into(),
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
            self.fleet.node(&self.target),
            &["msg", "reply", "question", "answer body"],
        )
    }

    fn read_question(&self) {
        let messages = mail(self.fleet.node(&self.target), &self.receiver.pane);
        assert_eq!(messages.as_array().unwrap().len(), 1, "{messages}");
        assert_eq!(messages[0]["body"], "question body");
        assert_eq!(messages[0]["from_agent"], self.sender.id);
        assert_eq!(messages[0]["reply_contract"], "durable_return_binding");
        state(self.fleet.node(&self.target), "question", "read");
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
            read(self.fleet.node(&self.target), &self.receiver.pane),
            json!([])
        );
    }
}

fn status(node: &Node, correlation: &str) -> Value {
    api(node, "msg.status", json!({"correlation_id":correlation}))
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
        .execute(
            "UPDATE envelopes SET collect_at=0,retry_at=0,lease_until=0",
            [],
        )
        .unwrap();
}

fn scalar(node: &Node, sql: &str) -> i64 {
    db(node).query_row(sql, [], |r| r.get(0)).unwrap()
}

fn once(node: &Node, correlation: &str) {
    assert_eq!(
        db(node)
            .query_row(
                "SELECT count(*) FROM envelopes WHERE correlation=?1",
                [correlation],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1,
        "{} {correlation}",
        node.name
    );
}

fn routes(node: &Node) -> Vec<Value> {
    api(node, "peers.enrollment", json!({}))["routes"]
        .as_array()
        .unwrap()
        .clone()
}

fn route(node: &Node, target: &Node) -> Value {
    let id = fleet::node_id(target);
    fleet::wait_until("live route", DEADLINE, || {
        routes(node).into_iter().find(|r| r["node"] == id)
    })
}

fn raw(node: &Node, method: &str, params: Value) -> Value {
    serde_json::from_str(
        &node.api(&json!({"id":"s2", "method":method, "params":params}).to_string()),
    )
    .unwrap()
}

fn send_as(node: &Node, sender: &Agent, receiver: &Agent, correlation: &str) -> Value {
    api(
        node,
        "msg.send",
        json!({"to":{"type":"agent","agent":receiver.id},
        "from_agent":sender.id, "body":"question body", "intent":"needs_reply", "correlation_id":correlation}),
    )
}

fn round_trip(c: &Conversation) {
    let sent = c.send();
    assert!(
        matches!(
            sent["state"].as_str(),
            Some("queued" | "custody" | "delivered")
        ),
        "{sent}"
    );
    state(c.fleet.node("nodea"), "question", "delivered");
    c.read_question();
    state(c.fleet.node("nodea"), "question", "read");
    c.reply();
    c.answer_once();
    once(c.fleet.node(&c.target), "question");
    let result = status(c.fleet.node("nodea"), "question");
    once(
        c.fleet.node("nodea"),
        result["reply"]["correlation_id"].as_str().unwrap(),
    );
}

fn cli_answer(c: &Conversation) {
    for args in [
        vec!["msg", "status", "question"],
        vec!["wait", "reply", "question", "--timeout", "0", "--json"],
    ] {
        let output = operator(c.fleet.node("nodea"), &args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["result"]["reply"]["body"], "answer body");
    }
    let output = operator(
        c.fleet.node("nodea"),
        &["msg", "read", "--pane", &c.sender.pane],
    );
    assert!(output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["result"]["messages"], json!([]));
}

#[test]
#[ignore = "needs 661-2h"]
fn chain_request_reply_and_receipts_across_two_hops() {
    let c = Conversation::to(fleet::CHAIN_ABC, "nodec");
    assert_eq!(
        route(c.fleet.node("nodea"), c.fleet.node("nodec"))["hops"],
        2
    );
    round_trip(&c);
    cli_answer(&c);
}

#[test]
#[ignore = "needs 661-2h"]
fn spoke_without_outbound_edges_sends_and_receives_through_hub() {
    let c = Conversation::to(fleet::HUB_SPOKES, "nodec");
    round_trip(&c);
}

#[test]
fn one_way_edge_carries_both_directions() {
    let c = Conversation::new(DIRECT);
    round_trip(&c);
    cli_answer(&c);
    let sent = c.receiver.cli(
        c.fleet.node("nodeb"),
        &[
            "msg",
            "send",
            "--agent",
            &c.sender.id,
            "--correlation-id",
            "reverse",
            "--intent",
            "needs-reply",
            "question body",
        ],
    );
    assert!(
        matches!(sent["state"].as_str(), Some("queued" | "delivered")),
        "{sent}"
    );
    let messages = mail(c.fleet.node("nodea"), &c.sender.pane);
    assert_eq!(messages[0]["correlation_id"], "reverse");
    api(
        c.fleet.node("nodea"),
        "msg.reply",
        json!({"correlation_id":"reverse", "body":"reverse answer"}),
    );
    let messages = mail(c.fleet.node("nodeb"), &c.receiver.pane);
    assert_eq!(messages[0]["body"], "reverse answer");
    once(c.fleet.node("nodea"), "reverse");
}

#[test]
#[ignore = "needs 661-2h"]
fn laptop_collects_answer_through_a_different_hub() {
    let c = Conversation::to(fleet::LAPTOP_TWO_HUBS, "noded.example");
    cut(&c.fleet, "nodea", "nodec");
    c.send();
    c.read_question();
    cut(&c.fleet, "nodea", "nodeb");
    c.reply();
    reconnect(&c.fleet, "nodea", "nodec");
    for node in &c.fleet.nodes {
        due(node);
    }
    c.answer_once();
    let found = route(c.fleet.node("nodea"), c.fleet.node("noded.example"));
    assert_eq!(found["next_hop"], fleet::node_id(c.fleet.node("nodec")));
    once(c.fleet.node("noded.example"), "question");
}

fn operator(node: &Node, args: &[&str]) -> std::process::Output {
    support::environment::Command::new(env!("CARGO_BIN_EXE_flk"))
        .args(args)
        .env_clear()
        .env("HOME", &node.home)
        .env("XDG_CONFIG_HOME", &node.config_home)
        .env("XDG_RUNTIME_DIR", &node.runtime_dir)
        .env("XDG_STATE_HOME", node.home.join("state"))
        .env("FLOCK_SOCKET_PATH", &node.api_socket)
        .output()
        .unwrap()
}

#[test]
fn two_hubs_hold_edges_to_one_spoke_concurrently() {
    let fleet = fleet::spawn("s2hubs", fleet::TWO_HUBS);
    for hub in ["nodea", "nodeb"] {
        route(fleet.node("nodec"), fleet.node(hub));
    }
    let output = operator(fleet.node("nodec"), &["peers", "status", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    let rows = status.as_array().unwrap();
    assert_eq!(
        rows.iter()
            .filter(|p| p["source"] == "inbound" && p["enrollment"] == "pinned")
            .count(),
        2,
        "{status}"
    );
    cut(&fleet, "nodea", "nodec");
    assert_eq!(route(fleet.node("nodec"), fleet.node("nodeb"))["hops"], 1);
}

// Patch only this sandbox's transport, before its first dial. No live server
// or shared shim is changed. Markers prove the fault traversed the held edge.
fn shim(fleet: &Fleet, marker: &str, source: &str) {
    let path = fleet.base.join("bin/ssh");
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.matches(marker).count(), 1);
    std::fs::write(path, text.replace(marker, &format!("{marker}{source}"))).unwrap();
}

#[test]
#[ignore = "needs 661-2g"]
fn route_cycle_and_exhausted_hop_budget_retain_custody() {
    for (field, value) in [("visited", json!(["receiver"])), ("hops_left", json!(0))] {
        let c = Conversation::to(fleet::CHAIN_ABC, "nodec");
        let capture = c.fleet.base.join("capture-delivery-nodea-nodeb");
        std::fs::write(&capture, "").unwrap();
        assert_eq!(c.send()["state"], "queued");
        let mut delivery: Value =
            serde_json::from_slice(&std::fs::read(&capture).unwrap()).unwrap();
        delivery[field] = if field == "visited" {
            json!([
                fleet::node_id(c.fleet.node("nodea")),
                fleet::node_id(c.fleet.node("nodeb")),
                fleet::node_id(c.fleet.node("nodea"))
            ])
        } else {
            value
        };
        let replay = c.fleet.base.join("replay-delivery-nodea-nodeb");
        std::fs::write(&replay, serde_json::to_vec(&delivery).unwrap()).unwrap();
        std::fs::remove_file(&capture).unwrap();
        due(c.fleet.node("nodea"));
        fleet::wait_until("route refusal retry", DEADLINE, || {
            (delivery_attempts(&c.fleet, "nodea", "nodeb", "question") >= 2).then_some(())
        });
        state(c.fleet.node("nodea"), "question", "queued");
        fleet::wait_until("rejected route releases custody lease", DEADLINE, || {
            (scalar(c.fleet.node("nodea"), "SELECT count(*) FROM envelopes WHERE correlation='question' AND state='custody' AND next_hop='' AND lease_until=0") == 1).then_some(())
        });
        let observed = Instant::now();
        fleet::wait_until(
            "unpushable custody stays unleased across worker ticks",
            DEADLINE,
            || {
                assert_eq!(scalar(c.fleet.node("nodea"), "SELECT count(*) FROM envelopes WHERE next_hop='' AND lease_until>0 AND state IN ('custody','held')"), 0);
                (observed.elapsed() >= Duration::from_secs(2)).then_some(())
            },
        );

        assert_eq!(read(c.fleet.node("nodec"), &c.receiver.pane), json!([]));
        assert_eq!(
            scalar(
                c.fleet.node("nodea"),
                "SELECT count(*) FROM envelopes WHERE state='custody'"
            ),
            1
        );
        std::fs::remove_file(replay).unwrap();
        cut(&c.fleet, "nodeb", "nodec");
        let target = fleet::node_id(c.fleet.node("nodec"));
        fleet::wait_until("bad route withdrawn", DEADLINE, || {
            routes(c.fleet.node("nodea"))
                .iter()
                .all(|r| r["node"] != target)
                .then_some(())
        });
        reconnect(&c.fleet, "nodeb", "nodec");
        route(c.fleet.node("nodea"), c.fleet.node("nodec"));
        due(c.fleet.node("nodea"));
        c.read_question();
        once(c.fleet.node("nodec"), "question");
    }
}

#[test]
#[ignore = "needs 661-2g"]
fn forged_origin_signature_route_and_token_are_refused() {
    for field in ["origin", "visited", "body", "signature", "token"] {
        let c = Conversation::to(fleet::CHAIN_ABC, "nodec");
        fs::write(c.fleet.base.join("tamper-delivery-nodeb-nodec"), field).unwrap();
        c.send();
        let response: Value =
            fleet::wait_until("forwarded forgery refused on the wire", DEADLINE, || {
                serde_json::from_slice(
                    &fs::read(c.fleet.base.join("tampered-result-nodeb-nodec")).ok()?,
                )
                .ok()
            });
        let expected = if field == "visited" {
            "origin_mismatch"
        } else {
            "invalid_signature"
        };
        assert_eq!(
            response["error"]["message"], expected,
            "{field}: {response}"
        );
        let forwarded: Value = serde_json::from_slice(
            &fs::read(c.fleet.base.join("tampered-delivery-nodeb-nodec")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            forwarded["params"]["envelope"]["correlation_id"],
            "question"
        );
        assert_eq!(forwarded["params"]["visited"].as_array().unwrap().len(), 2);
        once(c.fleet.node("nodeb"), "question");
        assert_eq!(
            scalar(
                c.fleet.node("nodec"),
                "SELECT count(*) FROM envelopes WHERE correlation='question'"
            ),
            0,
            "{field} accepted"
        );
        assert_eq!(read(c.fleet.node("nodec"), &c.receiver.pane), json!([]));
    }
}

#[test]
#[ignore = "needs 661-2g"]
fn allow_from_checks_origin_not_the_allowed_forwarding_hub() {
    let specs = [
        fleet::CHAIN_ABC[0].clone(),
        fleet::CHAIN_ABC[1].clone(),
        NodeSpec::new("nodec", "policy", &[]).with_config("\n[msg]\nallow_from=['nodeb']\n"),
    ];
    let c = Conversation::to(&specs, "nodec");
    c.send();
    let refused = state(c.fleet.node("nodea"), "question", "refused");
    assert!(
        refused["detail"]
            .as_str()
            .unwrap()
            .contains("msg_not_allowed"),
        "{refused}"
    );
    assert_eq!(read(c.fleet.node("nodec"), &c.receiver.pane), json!([]));
    let allowed = Agent::start(c.fleet.node("nodeb"));
    send_as(
        c.fleet.node("nodeb"),
        &allowed,
        &c.receiver,
        "allowed-origin",
    );
    assert_eq!(
        mail(c.fleet.node("nodec"), &c.receiver.pane)[0]["from_host"],
        "nodeb"
    );
}
// Record the writer's connection-local total_changes in the transaction that
// mutates each table. A separate observer connection cannot read that counter.
fn trace_writes(node: &Node) {
    let db = db(node);
    let tables = db
        .prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%'")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    db.execute_batch("CREATE TABLE acceptance_writes (total_changes INTEGER NOT NULL)")
        .unwrap();
    for table in tables {
        let table = table.replace('"', "\"\"");
        for op in ["INSERT", "UPDATE", "DELETE"] {
            db.execute_batch(&format!("CREATE TRIGGER \"acceptance_{op}_{table}\" AFTER {op} ON \"{table}\" BEGIN INSERT INTO acceptance_writes VALUES(total_changes()); END;")).unwrap();
        }
    }
}

#[test]
fn quiet_healthy_edge_keeps_routes() {
    let fleet = fleet::spawn("s2idle", fleet::CHAIN_ABC);
    route(fleet.node("nodea"), fleet.node("nodec"));
    route(fleet.node("nodec"), fleet.node("nodea"));
    for node in &fleet.nodes {
        trace_writes(node);
    }
    let databases: Vec<_> = fleet.nodes.iter().map(db).collect();
    let sample =
        || {
            databases
                .iter()
                .zip(&fleet.nodes)
                .map(|(db, node)| {
                    let writes = db.query_row(
                "SELECT count(*), coalesce(max(total_changes),0) FROM acceptance_writes", [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))).unwrap();
                    let generation = api(node, "peers.enrollment", json!({}))["route_generation"]
                        .as_u64()
                        .unwrap();
                    (writes, generation)
                })
                .collect::<Vec<_>>()
        };
    // Initial directory persistence is asynchronous. Observe a quiet window
    // before starting the fixed 30-second acceptance interval.
    let mut baseline = sample();
    let mut quiet = Instant::now();
    fleet::wait_until("startup writes settled", DEADLINE, || {
        let now = sample();
        if now != baseline {
            baseline = now;
            quiet = Instant::now();
        }
        (quiet.elapsed() >= Duration::from_secs(2)).then_some(())
    });
    let start = Instant::now();
    fleet::wait_until(
        "30 seconds with no store mutations",
        Duration::from_secs(35),
        || {
            assert_eq!(
                sample(),
                baseline,
                "idle fleet changed store writes or route generation"
            );
            for (from, to) in [("nodea", "nodec"), ("nodec", "nodea")] {
                let target = fleet::node_id(fleet.node(to));
                assert!(routes(fleet.node(from)).iter().any(|r| r["node"] == target));
            }
            (start.elapsed() >= Duration::from_secs(30)).then_some(())
        },
    );
}

#[test]
fn closed_edge_withdraws_routes() {
    let fleet = fleet::spawn("s2close", fleet::CHAIN_ABC);
    route(fleet.node("nodea"), fleet.node("nodec"));
    route(fleet.node("nodec"), fleet.node("nodea"));
    cut(&fleet, "nodeb", "nodec");
    let target = fleet::node_id(fleet.node("nodec"));
    fleet::wait_until("route withdrawal propagates", DEADLINE, || {
        (routes(fleet.node("nodea"))
            .iter()
            .all(|r| r["node"] != target)
            && routes(fleet.node("nodec")).is_empty())
        .then_some(())
    });
    assert_eq!(route(fleet.node("nodea"), fleet.node("nodeb"))["hops"], 1);
}

#[test]
#[ignore = "needs 661-2g"]
fn stale_directory_vs_authoritative_removal_gives_recipient_gone() {
    let c = Conversation::to(fleet::CHAIN_ABC, "nodec");
    // Discover first so the sender retains a genuine owner hint after close.
    api(
        c.fleet.node("nodec"),
        "pane.close",
        json!({"pane_id":c.receiver.pane}),
    );
    c.send();
    state(c.fleet.node("nodea"), "question", "recipient_gone");
    assert_eq!(
        scalar(
            c.fleet.node("nodec"),
            "SELECT count(*) FROM agent_tombstones"
        ),
        1
    );
    let unknown = raw(
        c.fleet.node("nodea"),
        "msg.send",
        json!({
        "to":{"type":"agent","agent":"agent_unknown.example_neverknown"}, "body":"unknown owner"}),
    );
    assert_eq!(unknown["error"]["code"], "msg_target_not_found");
}

#[test]
fn offline_laptop_reconnects_and_queued_mail_flows_once() {
    let c = Conversation::new(DIRECT);
    cut(&c.fleet, "nodea", "nodeb");
    assert_eq!(c.send()["state"], "queued");
    reconnect(&c.fleet, "nodea", "nodeb");
    due(c.fleet.node("nodea"));
    c.read_question();
    cut(&c.fleet, "nodea", "nodeb");
    c.reply();
    assert_eq!(read(c.fleet.node("nodea"), &c.sender.pane), json!([]));
    reconnect(&c.fleet, "nodea", "nodeb");
    due(c.fleet.node("nodea"));
    c.answer_once();
    once(c.fleet.node("nodeb"), "question");
}

#[test]
#[ignore = "needs 661-2h"]
fn forwarder_and_receiver_restart_mid_conversation() {
    let mut c = Conversation::to(fleet::CHAIN_ABC, "nodec");
    c.send();
    c.read_question();
    cut(&c.fleet, "nodea", "nodeb");
    c.reply();
    for name in ["nodeb", "nodec"] {
        c.fleet.node_mut(name).restart();
    }
    reconnect(&c.fleet, "nodea", "nodeb");
    for node in &c.fleet.nodes {
        due(node);
    }
    c.answer_once();
    once(c.fleet.node("nodec"), "question");
}

#[test]
#[ignore = "needs 661-2h"]
fn lost_ack_at_each_hop_imports_once() {
    for (from, to) in [("nodea", "nodeb"), ("nodeb", "nodec")] {
        let c = Conversation::to(fleet::CHAIN_ABC, "nodec");
        // Include forwarder custody acknowledgements, not just final delivery.
        // Restart this edge after patching its sandbox shim.
        cut(&c.fleet, from, to);
        shim(
            &c.fleet,
            "in {\"delivered\", \"duplicate\"}",
            " | {\"custody\"}",
        );
        reconnect(&c.fleet, from, to);
        let gate = c.fleet.base.join(format!("gate-delivery-ack-{from}-{to}"));
        std::fs::create_dir(&gate).unwrap();
        let output = c.sender.start_cli(
            c.fleet.node("nodea"),
            &[
                "msg",
                "send",
                "--agent",
                &c.receiver.id,
                "--intent",
                "needs-reply",
                "--correlation-id",
                "question",
                "question body",
            ],
        );
        fleet::wait_until("committed hop before ack", DEADLINE, || {
            gate.join("entered").exists().then_some(())
        });
        once(c.fleet.node(to), "question");
        cut(&c.fleet, from, to);
        cli_result(&output);
        std::fs::remove_dir_all(gate).unwrap();
        reconnect(&c.fleet, from, to);
        for node in &c.fleet.nodes {
            due(node);
        }
        c.read_question();
        c.reply();
        c.answer_once();
        for node in &c.fleet.nodes {
            once(node, "question");
        }
    }
}

#[test]
#[ignore = "needs 661-2g"]
fn pause_at_forwarder_freezes_ttl_across_restart_then_resumes() {
    let mut c = Conversation::to(fleet::CHAIN_ABC, "nodec");
    let capture = c.fleet.base.join("capture-delivery-nodeb-nodec");
    fs::write(&capture, "").unwrap();
    c.send();
    fleet::wait_until("forwarder attempted delivery", DEADLINE, || {
        (delivery_attempts(&c.fleet, "nodeb", "nodec", "question") >= 1).then_some(())
    });
    fleet::wait_until("forwarder custody", DEADLINE, || {
        (scalar(
            c.fleet.node("nodeb"),
            "SELECT count(*) FROM envelopes WHERE state='custody'",
        ) == 1)
            .then_some(())
    });
    api(c.fleet.node("nodeb"), "fleet.pause", json!({}));
    fleet::wait_until("durable pause", DEADLINE, || {
        (scalar(c.fleet.node("nodeb"), "SELECT paused FROM clock") == 1).then_some(())
    });
    let snapshot = || {
        db(c.fleet.node("nodeb")).query_row(
        "SELECT elapsed,custody_deadline,lease_until FROM clock,envelopes WHERE correlation='question'", [],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))).unwrap()
    };
    let before = snapshot();
    c.fleet.node_mut("nodeb").restart();
    fs::remove_file(capture).unwrap();
    let start = Instant::now();
    fleet::wait_until("paused across restart and worker ticks", DEADLINE, || {
        let after = db(c.fleet.node("nodeb")).query_row(
            "SELECT elapsed,custody_deadline,lease_until FROM clock,envelopes WHERE correlation='question'", [],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))).unwrap();
        assert_eq!(after, before);
        assert_eq!(read(c.fleet.node("nodec"), &c.receiver.pane), json!([]));
        (start.elapsed() >= Duration::from_secs(2)).then_some(())
    });
    api(c.fleet.node("nodeb"), "fleet.resume", json!({}));
    due(c.fleet.node("nodeb"));
    c.read_question();
    once(c.fleet.node("nodec"), "question");
}

#[test]
fn expiry_and_outbox_limits_report_honest_outcomes() {
    let c = Conversation::new(DIRECT);
    cut(&c.fleet, "nodea", "nodeb");
    assert_eq!(c.send()["state"], "queued");
    let origin = c.fleet.node("nodea");
    db(origin)
        .execute("UPDATE usage SET active=10000", [])
        .unwrap();
    let full = raw(
        origin,
        "msg.send",
        json!({"to":{"type":"agent","agent":c.receiver.id},
        "from_agent":c.sender.id, "body":"over quota", "correlation_id":"over-quota"}),
    );
    assert!(full.to_string().contains("mail_store_full"), "{full}");
    assert_eq!(
        scalar(
            origin,
            "SELECT count(*) FROM envelopes WHERE correlation='over-quota'"
        ),
        0
    );
    db(origin).execute("UPDATE usage SET active=1", []).unwrap();
    db(origin)
        .execute(
            "UPDATE envelopes SET custody_deadline=0 WHERE correlation='question'",
            [],
        )
        .unwrap();
    state(origin, "question", "expired");
    let wait = api(
        origin,
        "msg.wait_reply",
        json!({"correlation_id":"question","timeout_ms":0}),
    );
    assert_eq!(wait["outcome"], "expired", "{wait}");
    assert_eq!(
        scalar(origin, "SELECT count(*) FROM envelopes WHERE state IN ('custody','held') AND lease_until>retry_at"),
        0
    );
    reconnect(&c.fleet, "nodea", "nodeb");
    assert_eq!(read(c.fleet.node("nodeb"), &c.receiver.pane), json!([]));
}

#[test]
#[ignore = "needs 661-2h"]
fn durable_channel_original_settles_on_reply_custody() {
    let specs = [
        fleet::CHAIN_ABC[0].clone(),
        fleet::CHAIN_ABC[1].clone(),
        NodeSpec::new("nodec", "channel", &[]).with_config("\n[msg]\nchannel_push=true\n"),
    ];
    let c = Conversation::to(&specs, "nodec");
    c.send();
    fleet::wait_until("unread channel original", DEADLINE, || {
        (api(
            c.fleet.node("nodec"),
            "msg.list",
            json!({"pane":c.receiver.pane}),
        )["messages"]
            .as_array()?
            .len()
            == 1)
            .then_some(())
    });
    cut(&c.fleet, "nodeb", "nodec");
    let reply = c.reply();
    assert_eq!(reply["state"], "held");
    assert_eq!(read(c.fleet.node("nodec"), &c.receiver.pane), json!([]));
    once(
        c.fleet.node("nodec"),
        reply["correlation_id"].as_str().unwrap(),
    );
    reconnect(&c.fleet, "nodeb", "nodec");
    for node in &c.fleet.nodes {
        due(node);
    }
    c.answer_once();
}

#[test]
#[ignore = "needs 661-2h"]
fn audit_rotation_during_multihop_conversation_loses_nothing() {
    use std::io::Write;
    let mut conversation = Conversation::to(fleet::CHAIN_ABC, "nodec");
    conversation.send();
    conversation.read_question();
    cut(&conversation.fleet, "nodea", "nodeb");
    assert_eq!(conversation.reply()["state"], "held");
    conversation.fleet.node_mut("nodec").stop();
    let log = conversation
        .fleet
        .node("nodec")
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
    conversation.fleet.node_mut("nodec").restart();
    api(
        conversation.fleet.node("nodec"),
        "workspace.create",
        json!({"cwd":conversation.fleet.node("nodec").repo}),
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
fn follow_up_after_final_answer_on_one_way_edge_is_collected() {
    let conversation = Conversation::new(DIRECT);
    conversation.send();
    conversation.read_question();
    conversation.reply();
    conversation.answer_once();
    // Let the idle outbound poll enter its long backoff after the final answer.
    let start = Instant::now();
    fleet::wait_until("post-answer idle interval", DEADLINE, || {
        assert_eq!(
            read(conversation.fleet.node("nodea"), &conversation.sender.pane),
            json!([])
        );
        (start.elapsed() >= Duration::from_secs(7)).then_some(())
    });
    let reply = conversation.receiver.cli(
        conversation.fleet.node("nodeb"),
        &["msg", "reply", "question", "follow-up body"],
    );
    let origin = conversation.fleet.node("nodea");
    let messages = fleet::wait_until("follow-up wake collection", Duration::from_secs(4), || {
        let messages = read(origin, &conversation.sender.pane);
        (!messages.as_array()?.is_empty()).then_some(messages)
    });
    assert_eq!(messages[0]["body"], "follow-up body");
    assert_eq!(messages[0]["correlation_id"], reply["correlation_id"]);
    assert_eq!(read(origin, &conversation.sender.pane), json!([]));
}

use std::{fs, os::unix::fs::PermissionsExt, path::Path};

fn start_at(node: &Node, cwd: &Path, executable: &Path) -> Value {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let name = format!(
        "fixture-{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    api(
        node,
        "agent.start",
        json!({"name":name, "cwd":cwd, "argv":[executable]}),
    )["agent"]
        .clone()
}
fn native_executable(node: &Node) -> std::path::PathBuf {
    let executable = node.home.join("claude");
    let report = format!("printf '%s\\n' '{{\"session_id\":\"retained-session\"}}' | {0} hook claude session\n{0} pane report-agent --source flock:claude --agent claude --state idle --agent-session-id retained-session", quote(env!("CARGO_BIN_EXE_flk")));
    let stop = format!("if [ \"$line\" = fixture-stop ]; then printf '%s\\n' '{{\"hook_event_name\":\"Stop\",\"last_assistant_message\":\"※ recap: Done. Next: wait.\"}}' | {} hook claude stop > {}; touch {}; fi", quote(env!("CARGO_BIN_EXE_flk")), quote(node.home.join("stop-output").to_str().unwrap()), quote(node.home.join("stop-ready").to_str().unwrap()));
    fs::write(&executable, format!("#!/bin/sh\n{report}\nprintf 'Claude Code\\nTask complete.\\n─────────────\\n❯ \\n─────────────\\n'\nwhile IFS= read -r line\ndo\n{report}\n{stop}\ndone\n")).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    executable
}
fn native_agent(node: &Node) -> Value {
    let executable = native_executable(node);
    let agent = start_at(node, &node.repo, &executable);
    fleet::wait_until("native session hook", DEADLINE, || {
        let current = api(node, "agent.get", json!({"target":agent["pane_id"]}))["agent"].clone();
        (current["agent_session"]["value"] == "retained-session").then_some(())
    });
    agent
}

fn removals(node: &Node) -> i64 {
    db(node)
        .query_row("SELECT count(*) FROM agent_tombstones", [], |r| r.get(0))
        .unwrap()
}
#[test]
fn restart_resume_582_keeps_identity_without_tombstone() {
    let fleet = fleet::spawn("ts-restart", &[NodeSpec::new("nodea", "restart", &[]).with_config("\n[session.restart]\nrestart_grace_secs=0\nkill_grace_secs=0\noperator_quiet_ms=0\nflush_wait_ms=0\nsettle_ms=0\nverify_timeout_secs=20\n")]);
    let node = fleet.node("nodea");
    let target = native_agent(node);
    let sender = Agent::start(node);
    api(
        node,
        "msg.send",
        json!({"to":{"type":"agent","agent":target["agent_id"]},"from_agent":sender.id,"body":"restart message","correlation_id":"restart-mail"}),
    );
    api(
        node,
        "agent.restart",
        json!({"target":target["pane_id"], "reason":"fixture restart"}),
    );
    fleet::wait_until("restart completed", DEADLINE, || {
        let log = fs::read_to_string(node.config_home.join("flock-dev/event-log.jsonl")).ok()?;
        log.lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .any(|e| {
                e["envelope"]["event"] == "agent_restart"
                    && e["envelope"]["data"]["phase"] == "restarted"
            })
            .then_some(())
    });
    let current = api(node, "agent.get", json!({"target":target["pane_id"]}))["agent"].clone();
    assert_eq!(current["agent_id"], target["agent_id"]);
    assert_eq!(current["agent_session"]["value"], "retained-session");
    assert_eq!(removals(node), 0);
    state(node, "restart-mail", "delivered");
    let messages = api(node, "msg.read", json!({"pane":target["pane_id"]}));
    assert!(messages["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["correlation_id"] == "restart-mail"));
}

fn rejected_config(node: &Node, content: &str, key: &str) {
    let path = node.config_home.join("flock-dev/config.toml");
    let original = fs::read_to_string(&path).unwrap();
    fs::write(&path, content).unwrap();
    let output = operator(node, &["server"]);
    fs::write(path, original).unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains(key) && error.contains("was removed") && error.contains("delete this line"),
        "{error}"
    );
}

#[test]
fn mismatched_protocol_and_custom_summary_refused_with_upgrade_message() {
    let mut c = Conversation::new(DIRECT);
    c.fleet
        .node_mut("nodeb")
        .restart_with_mesh(fleet::MeshMode::VersionMismatch(0));
    fleet::wait_until("protocol upgrade diagnostic", DEADLINE, || {
        api(c.fleet.node("nodea"), "peers.enrollment", json!({}))["peers"]
            .as_array()?
            .iter()
            .any(|p| {
                p["reason"]
                    .as_str()
                    .is_some_and(|s| s.contains("upgrade flk on nodeb"))
            })
            .then_some(())
    });
    assert_eq!(c.send()["state"], "queued");
    assert_eq!(read(c.fleet.node("nodeb"), &c.receiver.pane), json!([]));
    rejected_config(
        c.fleet.node("nodea"),
        "[[peers]]\nname='nodeb'\nsummary_command='false'\n",
        "summary_command",
    );
}

#[test]
fn removed_config_keys_rejected_with_migration_instructions() {
    let fleet = fleet::spawn("s2config", &[NodeSpec::new("nodea", "config", &[])]);
    for key in [
        "uplink_timeout_secs",
        "uplink_heartbeat_secs",
        "deferral_relay_concurrency",
    ] {
        rejected_config(fleet.node("nodea"), &format!("[msg]\n{key}=20\n"), key);
    }
}

#[test]
fn mailbox_full_retry_vs_24h_inbox_expiry() {
    let c = Conversation::new(DIRECT);
    let owner = c.fleet.node("nodeb");
    // Distinct attested senders avoid asserting against the sender rate limit.
    let second = Agent::start(c.fleet.node("nodea"));
    for index in 0..32 {
        let sender = if index < 16 { &c.sender } else { &second };
        assert_eq!(
            send_as(
                c.fleet.node("nodea"),
                sender,
                &c.receiver,
                &format!("fill-{index}")
            )["state"],
            "delivered"
        );
    }
    let queued = c.send();
    assert_eq!(queued["state"], "queued", "{queued}");
    assert!(
        queued["warnings"].to_string().contains("mailbox_full"),
        "{queued}"
    );
    assert_eq!(
        scalar(
            owner,
            "SELECT count(*) FROM envelopes WHERE correlation='question'"
        ),
        0
    );
    // Shorten only the fixture's inbox deadline: seven-day custody remains.
    assert_eq!(
        scalar(owner, "SELECT min(mailbox_ttl_ms) FROM envelopes"),
        86_400_000
    );
    db(owner)
        .execute(
            "UPDATE envelopes SET inbox_deadline=0 WHERE correlation LIKE 'fill-%'",
            [],
        )
        .unwrap();
    assert_eq!(read(owner, &c.receiver.pane), json!([]));
    state(c.fleet.node("nodea"), "fill-0", "expired");
    due(c.fleet.node("nodea"));
    c.read_question();
    once(owner, "question");
    assert_eq!(
        scalar(
            c.fleet.node("nodea"),
            "SELECT count(*) FROM envelopes WHERE state IN ('custody','held') AND lease_until>retry_at"
        ),
        0
    );
}

#[test]
fn wedged_peer_does_not_starve_a_user_send() {
    let specs = [
        NodeSpec::new("nodea", "sender", &["nodeb", "nodec"]),
        NodeSpec::new("nodeb", "wedged", &[]),
        NodeSpec::new("nodec", "healthy", &[]),
    ];
    let c = Conversation::to(&specs, "nodec");
    let blocked = Agent::start(c.fleet.node("nodeb"));
    fleet::wait_until("wedged target discovery", DEADLINE, || {
        api(c.fleet.node("nodea"), "agent.list", json!({}))["fleet"]
            .as_array()?
            .iter()
            .any(|a| a["agent_id"] == blocked.id)
            .then_some(())
    });
    let gate = c.fleet.gate_message_edge("nodea", "nodeb");
    let output = c.sender.start_cli(
        c.fleet.node("nodea"),
        &["msg", "send", "--agent", &blocked.id, "blocked"],
    );
    gate.wait_entered(DEADLINE);
    let start = Instant::now();
    let sent = send_as(
        c.fleet.node("nodea"),
        &c.sender,
        &c.receiver,
        "healthy-send",
    );
    assert_eq!(sent["state"], "delivered", "{sent}");
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(
        mail(c.fleet.node("nodec"), &c.receiver.pane)[0]["correlation_id"],
        "healthy-send"
    );
    gate.release();
    cli_result(&output);
}

#[test]
fn store_fault_keeps_panes_and_unknown_refusals_stay_queued() {
    let c = Conversation::new(DIRECT);
    let origin = c.fleet.node("nodea");
    let owner = c.fleet.node("nodeb");
    db(owner).execute_batch("CREATE TRIGGER deny_import BEFORE INSERT ON envelopes BEGIN SELECT RAISE(FAIL, 'acceptance store fault'); END;").unwrap();
    assert_eq!(c.send()["state"], "queued");
    for (node, pane) in [(origin, &c.sender.pane), (owner, &c.receiver.pane)] {
        api(node, "pane.get", json!({"pane_id":pane}));
    }
    assert_eq!(read(owner, &c.receiver.pane), json!([]));
    db(owner).execute_batch("DROP TRIGGER deny_import").unwrap();
    assert_eq!(
        c.receiver.cli(owner, &["msg", "read"])["messages"],
        json!([])
    );
    cut(&c.fleet, "nodea", "nodeb");
    let path = c.fleet.base.join("bin/ssh");
    let source = fs::read_to_string(&path).unwrap();
    assert!(source.contains("\"code\": \"held_by_test\""));
    fs::write(
        path,
        source.replace(
            "\"code\": \"held_by_test\"",
            "\"code\": \"mesh_delivery_refused\"",
        ),
    )
    .unwrap();
    // The standard refusal code carries an unknown reason. Keep origin custody.
    let capture = c.fleet.base.join("capture-delivery-nodea-nodeb");
    fs::write(&capture, "").unwrap();
    reconnect(&c.fleet, "nodea", "nodeb");
    due(origin);
    fleet::wait_until("unknown refusal returned", DEADLINE, || {
        serde_json::from_slice::<Value>(&fs::read(&capture).ok()?).ok()
    });
    state(origin, "question", "queued");
    fleet::wait_until(
        "failed attempt scheduled beyond its lease",
        DEADLINE,
        || {
            (scalar(origin, "SELECT count(*) FROM envelopes WHERE state IN ('custody','held') AND lease_until>retry_at") == 0).then_some(())
        },
    );
    once(origin, "question");
    fs::remove_file(capture).unwrap();
    due(origin);
    c.read_question();
    once(owner, "question");
}

#[test]
#[ignore = "needs 661-2g"]
fn corrupt_row_at_each_hop_does_not_block_healthy_mail_or_strand_leases() {
    let mut c = Conversation::to(fleet::CHAIN_ABC, "nodec");
    for name in ["nodea", "nodeb", "nodec"] {
        let node = c.fleet.node(name);
        let local = Agent::start(node);
        api(
            node,
            "msg.send",
            json!({"to":{"type":"agent","agent":local.id},
            "body":"corrupt fixture", "correlation_id":"corrupt-at-hop"}),
        );
        c.fleet.node_mut(name).stop();
        db(c.fleet.node(name))
            .execute(
                "UPDATE envelopes SET body=X'00' WHERE correlation='corrupt-at-hop'",
                [],
            )
            .unwrap();
        c.fleet.node_mut(name).restart();
    }
    send_as(
        c.fleet.node("nodea"),
        &c.sender,
        &c.receiver,
        "healthy-after-corruption",
    );
    let messages = mail(c.fleet.node("nodec"), &c.receiver.pane);
    assert_eq!(messages[0]["correlation_id"], "healthy-after-corruption");
    once(c.fleet.node("nodec"), "healthy-after-corruption");
    for node in &c.fleet.nodes {
        let status = api(node, "peers.enrollment", json!({}));
        assert!(
            status["mesh_quarantined"].as_u64().unwrap() >= 1,
            "{status}"
        );
        fleet::wait_until("no stranded leases", DEADLINE, || {
            (scalar(node, "SELECT count(*) FROM envelopes WHERE state IN ('custody','held') AND lease_until>retry_at") == 0).then_some(())
        });
    }
}
