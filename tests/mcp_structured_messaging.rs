//! Messaging closure through real servers, attested MCP children and fake harnesses.
mod support;
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};
use support::fleet::{self, Node, NodeSpec};
const LIMIT: Duration = Duration::from_secs(15);
const LOCAL: &[NodeSpec] = &[NodeSpec::new("nodea", "alpha", &[])];
const NODES: &[NodeSpec] = &[
    NodeSpec::new("nodea", "alpha", &["nodeb"]),
    NodeSpec::new("nodeb", "beta", &["nodea"]),
];
fn python() -> std::path::PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|p| p.join("python3"))
        .find(|p| p.is_file())
        .unwrap()
}
fn api(node: &Node, method: &str, params: Value) -> Value {
    let value: Value = serde_json::from_str(
        &node.api(&json!({"id":"fixture", "method":method, "params":params}).to_string()),
    )
    .unwrap();
    assert!(value.get("error").is_none(), "{value}");
    value["result"].clone()
}
struct Bridge {
    stream: BufReader<UnixStream>,
    agent: String,
    seq: u64,
    inputs: Option<PathBuf>,
}
impl Bridge {
    fn start(node: &Node, name: &str, structured: bool) -> Self {
        let path = node.home.join(name);
        let listener = UnixListener::bind(&path).unwrap();
        let params = if structured {
            json!({"runtime":"structured", "name":name, "model":name, "cwd":node.repo, "argv":["claude"]})
        } else {
            json!({"name":name, "cwd":node.repo, "argv":[python(), concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mcp-messaging/claude.py"), env!("CARGO_BIN_EXE_flk"), path]})
        };
        let started = api(node, "agent.start", params);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(listener.accept());
        });
        let (stream, _) = rx
            .recv_timeout(LIMIT)
            .expect("harness must connect")
            .unwrap();
        stream.set_read_timeout(Some(LIMIT)).unwrap();
        stream.set_write_timeout(Some(LIMIT)).unwrap();
        let mut bridge = Self {
            stream: BufReader::new(stream),
            agent: started["agent"]["agent_id"].as_str().unwrap().into(),
            seq: 0,
            inputs: structured.then(|| path.with_extension("inputs")),
        };
        bridge.request("initialize", json!({}));
        bridge
    }
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.seq += 1;
        writeln!(
            self.stream.get_mut(),
            "{}",
            json!({"jsonrpc":"2.0", "id":self.seq, "method":method, "params":params})
        )
        .unwrap();
        let mut line = String::new();
        assert!(
            self.stream
                .read_line(&mut line)
                .expect("bounded MCP response")
                > 0,
            "harness disconnected"
        );
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["id"], self.seq);
        assert!(value.get("error").is_none(), "MCP refusal: {value}");
        value
    }
    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        let response = self.request("tools/call", json!({"name":name, "arguments":arguments}));
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    }
    fn input_once(&self) {
        if let Some(path) = &self.inputs {
            let inputs = fs::read_to_string(path).expect("harness input precedes started");
            assert_eq!(
                inputs.lines().count(),
                1,
                "one actual harness input: {inputs}"
            );
        }
    }
    fn inbox(&mut self, node: &Node, queued: &Value) -> Value {
        if let Some(path) = &self.inputs {
            let item = queued["item_id"]
                .as_str()
                .expect("native admission has an item id");
            let deadline = Instant::now() + LIMIT;
            let mut after = 0;
            // Consume committed transition pages. An empty stream waits on the
            // server's EventHub condvar; this never polls inbox snapshots.
            let delivered = loop {
                let remaining = deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis();
                assert!(remaining > 0, "native delivery deadline");
                let page = api(
                    node,
                    "events.read",
                    json!({"after":after,
                    "kinds":["queue_item_state"],"subjects":[item],
                    "wait_ms":remaining.min(5000)}),
                );
                assert!(page.get("gap").is_none(), "delivery cursor gap: {page}");
                let events = page["events"].as_array().unwrap();
                assert!(
                    !events.is_empty(),
                    "native delivery facts did not arrive: {page}; input={:?}; owner={}",
                    fs::read_to_string(path),
                    api(node, "agent.get", json!({"target":self.agent}))
                );
                if let Some(event) = events.iter().find(|e| e["data"]["to"] == "delivered") {
                    break event["data"].clone();
                }
                assert!(
                    events.iter().all(|e| e["data"]["to"] == "queued"),
                    "harness did not start the admitted item: {page}"
                );
                let next = page["next_after"].as_u64().unwrap();
                assert!(next > after, "delivery cursor must advance: {page}");
                after = next;
            };
            assert_eq!(delivered["agent"], self.agent);
            assert_eq!(delivered["item_id"], item);
            self.input_once();
            let input: Value =
                serde_json::from_str(fs::read_to_string(path).unwrap().lines().next().unwrap())
                    .unwrap();
            assert_eq!(input["uuid"], delivered["batch_id"]);
        }
        let read = self.tool("flock_msg_read", json!({}));
        let messages = read["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1, "one delivered message: {read}");
        let message = messages[0].clone();
        assert_eq!(
            self.tool("flock_msg_read", json!({}))["messages"],
            json!([])
        );
        message
    }
}
fn setup(specs: &[NodeSpec]) -> fleet::Fleet {
    fleet::spawn_with_startup_probe("mcp-native", specs, |fleet, name| {
        let node = fleet.node(name);
        let fake = fleet.base.join("bin/claude");
        fs::write(
            &fake,
            format!(
                "#!{}\n{}",
                python().display(),
                include_str!("fixtures/mcp-messaging/claude.py")
            ),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
        let token = node.home.join("token");
        fs::write(&token, "fixture-token-not-a-credential").unwrap();
        fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
        let config = node
            .config_home
            .join(support::paths::app_dir_name())
            .join("config.toml");
        let mut text = fs::read_to_string(&config).unwrap();
        text.push_str(&format!("\n[structured]\nrun_as='operator'\ntoken_file='{}'\n[security]\nagent_account='none'\n", token.display()));
        fs::write(config, text).unwrap();
    })
}
fn exchange(
    sender_node: &Node,
    sender: &mut Bridge,
    recipient_node: &Node,
    recipient: &mut Bridge,
    correlation: &str,
) {
    let sent = sender.tool("flock_msg_send", json!({"to":{"type":"agent", "agent":recipient.agent}, "body":"fixture question", "intent":"needs_reply", "correlation_id":correlation}));
    assert_eq!(sent["correlation_id"], correlation, "{sent}");
    let received = recipient.inbox(recipient_node, &sent);
    assert_eq!(received["body"], "fixture question");
    assert_eq!(received["correlation_id"], correlation);
    assert_eq!(received["from_agent"], sender.agent);
    assert_eq!(received["intent"], "needs_reply");
    let reply_id = format!("{correlation}-answer");
    let reply = recipient.tool(
        "flock_msg_reply",
        json!({"correlation_id":correlation,
        "reply_correlation_id":reply_id,"body":"fixture answer"}),
    );
    assert_eq!(reply["correlation_id"], reply_id, "{reply}");
    let waited = sender.tool(
        "flock_msg_wait_reply",
        json!({"correlation_id":correlation,"timeout_ms":5000}),
    );
    assert_eq!(waited["outcome"], "replied", "{waited}");
    assert_eq!(waited["correlation_id"], correlation);
    assert_eq!(waited["reply"]["body"], "fixture answer");
    assert_eq!(waited["reply"]["correlation_id"], reply_id);
    assert_eq!(waited["reply"]["from_agent"], recipient.agent);
    let answer = sender.inbox(sender_node, &reply);
    assert_eq!(answer["correlation_id"], reply_id);
    assert_eq!(answer["in_reply_to"], correlation);
    assert_eq!(answer["body"], "fixture answer");
    assert_eq!(answer["intent"], "fyi");
    assert_eq!(answer["from_agent"], recipient.agent);
    sender.input_once();
    recipient.input_once();
}
#[test]
fn pty_to_structured_to_reply() {
    let fleet = setup(LOCAL);
    let node = fleet.node("nodea");
    let mut sender = Bridge::start(node, "terminal", false);
    let mut recipient = Bridge::start(node, "native", true);
    exchange(node, &mut sender, node, &mut recipient, "pty-native");
}
#[test]
fn structured_to_pty_to_reply() {
    let fleet = setup(LOCAL);
    let node = fleet.node("nodea");
    let mut sender = Bridge::start(node, "native", true);
    let mut recipient = Bridge::start(node, "terminal", false);
    exchange(node, &mut sender, node, &mut recipient, "native-pty");
}
#[test]
fn structured_to_structured_to_reply() {
    let fleet = setup(LOCAL);
    let node = fleet.node("nodea");
    let mut sender = Bridge::start(node, "first", true);
    let mut recipient = Bridge::start(node, "second", true);
    exchange(node, &mut sender, node, &mut recipient, "native-native");
}
#[test]
fn remote_structured_recipient_replies_to_the_other_node() {
    let fleet = setup(NODES);
    let a = fleet.node("nodea");
    let b = fleet.node("nodeb");
    let mut sender = Bridge::start(b, "terminal", false);
    let mut recipient = Bridge::start(a, "native", true);
    let deadline = Instant::now() + LIMIT;
    // Discovery is asynchronous. Observe the actual MCP directory before
    // sending once, rather than retrying a message whose effect is unknown.
    loop {
        let listing = sender.tool("flock_agent_list", json!({}));
        if let Some(row) = listing["fleet"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["agent_id"] == recipient.agent)
        {
            assert_eq!(row["runtime"], "structured");
            assert_eq!(row["local"], false);
            assert!(row.get("pane_id").is_none());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "remote native discovery absent: {listing}"
        );
    }
    exchange(b, &mut sender, a, &mut recipient, "remote-native");
}
