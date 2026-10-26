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
    sync::mpsc,
    time::{Duration, Instant},
};
use support::fleet::{self, Node, NodeSpec};
const LIMIT: Duration = Duration::from_secs(15);
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
        };
        bridge.request("initialize", json!({}));
        bridge
    }
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.begin(method, params);
        self.finish()
    }
    fn begin(&mut self, method: &str, params: Value) {
        self.seq += 1;
        writeln!(
            self.stream.get_mut(),
            "{}",
            json!({"jsonrpc":"2.0", "id":self.seq, "method":method, "params":params})
        )
        .unwrap();
    }
    fn finish(&mut self) -> Value {
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
}
fn setup() -> fleet::Fleet {
    fleet::spawn_with_startup_probe("mcp-native", NODES, |fleet, name| {
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
    node: &Node,
    sender: &mut Bridge,
    recipient: &mut Bridge,
    correlation: &str,
    remote: bool,
) {
    let sent = sender.tool("flock_msg_send", json!({"to":{"type":"agent", "agent":recipient.agent}, "body":"fixture question", "intent":"needs_reply", "correlation_id":correlation}));
    assert_eq!(sent["correlation_id"], correlation, "{sent}");
    let deadline = Instant::now() + LIMIT;
    let received = loop {
        let read = recipient.tool("flock_msg_read", json!({}));
        if let Some(message) = read["messages"].as_array().unwrap().first() {
            break message.clone();
        }
        assert!(
            Instant::now() < deadline,
            "delivery never reached inbox: {read}"
        );
    };
    assert_eq!(received["body"], "fixture question");
    assert_eq!(received["correlation_id"], correlation);
    assert_eq!(received["from_agent"], sender.agent);
    assert_eq!(received["intent"], "needs_reply");
    assert_eq!(
        recipient.tool("flock_msg_read", json!({}))["messages"],
        json!([])
    );
    // Put the wait in flight before the recipient replies. Its response is
    // read only after the independent recipient bridge accepts the answer.
    sender.begin("tools/call", json!({"name":"flock_msg_wait_reply", "arguments":{"correlation_id":correlation,"timeout_ms":5000}}));
    let reply = recipient.tool(
        "flock_msg_reply",
        json!({"correlation_id":correlation, "body":"fixture answer"}),
    );
    assert!(reply.get("error").is_none(), "{reply}");
    let response = sender.finish();
    let waited: Value =
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    if waited["outcome"] != "replied" {
        let status = sender.tool("flock_msg_status", json!({"correlation_id":correlation}));
        let inbox = sender.tool("flock_msg_read", json!({}));
        panic!("accepted reply: {reply}; wait: {waited}; sender status: {status}; sender inbox: {inbox}");
    }
    assert_eq!(waited["reply"]["body"], "fixture answer", "{waited}");
    let events = api(
        node,
        "events.read",
        json!({"after":0,"kinds":["message_replied"],"wait_ms":1000}),
    );
    let observations: Vec<_> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["data"]["correlation_id"] == correlation)
        .collect();
    assert_eq!(observations.len(), 1, "one native reply event: {events}");
    assert_eq!(
        observations[0]["data"]["reply_correlation_id"],
        reply["correlation_id"]
    );
    assert_eq!(observations[0]["data"]["round_trips"], 1);
    // Remote acceptance may hold custody before relay collection. The event
    // records that same accepted state, omitting held only when false.
    if remote {
        assert_eq!(reply["state"], "held");
        assert_eq!(observations[0]["data"]["held"], true);
    } else {
        assert!(observations[0]["data"].get("held").is_none());
    }
    let status = sender.tool("flock_msg_status", json!({"correlation_id":correlation}));
    assert_eq!(status["reply"]["correlation_id"], reply["correlation_id"]);
    assert_eq!(waited["reply"]["correlation_id"], reply["correlation_id"]);

    assert_eq!(
        status["reply"]["body"], "fixture answer",
        "durable reply absent: {status}"
    );
    let deadline = Instant::now() + LIMIT;
    loop {
        let inbox = sender.tool("flock_msg_read", json!({}));
        if let Some(answer) = inbox["messages"].as_array().unwrap().first() {
            assert_eq!(answer["body"], "fixture answer");
            assert_eq!(answer["in_reply_to"], correlation);
            assert_eq!(answer["from_agent"], recipient.agent);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "native answer delivery absent: {inbox}"
        );
    }
    assert_eq!(
        sender.tool("flock_msg_read", json!({}))["messages"],
        json!([])
    );
    assert_eq!(
        sender.tool("flock_msg_status", json!({"correlation_id":correlation}))["reply"]["body"],
        "fixture answer"
    );
}
#[test]
fn pty_to_structured_reply_is_observable() {
    let fleet = setup();
    let node = fleet.node("nodea");
    let mut sender = Bridge::start(node, "native", true);
    let mut recipient = Bridge::start(node, "terminal", false);
    exchange(node, &mut sender, &mut recipient, "pty-native", false);
    let inputs = fs::read_to_string(node.home.join("native.inputs")).unwrap();
    assert_eq!(
        inputs.lines().count(),
        1,
        "exactly one actual harness input"
    );
}

#[test]
fn structured_to_structured_reply_is_observable() {
    let fleet = setup();
    let node = fleet.node("nodea");
    let mut sender = Bridge::start(node, "first", true);
    let mut recipient = Bridge::start(node, "second", true);
    exchange(node, &mut sender, &mut recipient, "native-native", false);
}

#[test]
fn remote_discovery_triage() {
    let fleet = setup();
    let node = fleet.node("nodea");
    let control = Bridge::start(node, "terminal", false);
    let mut native = Bridge::start(node, "native", true);
    let mut observer = Bridge::start(fleet.node("nodeb"), "observer", false);
    let deadline = Instant::now() + LIMIT;
    let mut listing = loop {
        let listing = observer.tool("flock_agent_list", json!({}));
        if listing["fleet"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["agent_id"] == control.agent)
        {
            break listing;
        }
        assert!(Instant::now() < deadline, "PTY control absent: {listing}");
    };
    let local = api(node, "agent.list", json!({}));
    assert!(local["structured_agents"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["agent_id"] == native.agent));
    let summary = api(node, "peers.summary", json!({}));
    assert!(
        summary.to_string().contains(&control.agent),
        "PTY control absent from summary"
    );
    // A summary captured between the two starts may contain only the PTY.
    // Wait on the native row within the same discovery bound, not on timing.
    while !listing["fleet"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["agent_id"] == native.agent)
    {
        assert!(Instant::now() < deadline,
            "native omitted while PTY discovered; local={local}; summary={summary}; remote={listing}");
        listing = observer.tool("flock_agent_list", json!({}));
    }
    assert!(
        listing.to_string().contains(&native.agent),
        "native omitted while PTY discovered; local={local}; summary={summary}; remote={listing}"
    );
    let remote = listing["fleet"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["agent_id"] == native.agent)
        .unwrap();
    assert_eq!(remote["runtime"], "structured");
    assert!(remote.get("pane_id").is_none());
    assert!(summary["structured_agents"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["agent_id"] == native.agent && row["runtime"] == "structured"));
    // Native reply acceptance is recorded on the recipient's node; the
    // pending MCP wait and durable reply remain on the other node.
    exchange(node, &mut observer, &mut native, "remote-native", true);
    let inputs = fs::read_to_string(node.home.join("native.inputs")).unwrap();
    assert_eq!(
        inputs.lines().count(),
        1,
        "exactly one native harness input"
    );
}
