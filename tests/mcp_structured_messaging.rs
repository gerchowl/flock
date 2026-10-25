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
fn exchange(sender: &mut Bridge, recipient: &mut Bridge, correlation: &str) {
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
    let reply = recipient.tool(
        "flock_msg_reply",
        json!({"correlation_id":correlation, "body":"fixture answer"}),
    );
    assert!(reply.get("error").is_none(), "{reply}");
    let waited = sender.tool(
        "flock_msg_wait_reply",
        json!({"correlation_id":correlation, "timeout_ms":5000}),
    );
    if waited["outcome"] != "replied" {
        let status = sender.tool("flock_msg_status", json!({"correlation_id":correlation}));
        let inbox = sender.tool("flock_msg_read", json!({}));
        panic!("accepted reply: {reply}; wait: {waited}; sender status: {status}; sender inbox: {inbox}");
    }
    assert_eq!(waited["reply"]["body"], "fixture answer", "{waited}");
}
#[test]
fn pty_to_structured_to_reply() {
    let fleet = setup();
    let node = fleet.node("nodea");
    let mut sender = Bridge::start(node, "terminal", false);
    let mut recipient = Bridge::start(node, "native", true);
    exchange(&mut sender, &mut recipient, "pty-native");
    let inputs = fs::read_to_string(node.home.join("native.inputs")).unwrap();
    assert_eq!(
        inputs.lines().count(),
        1,
        "exactly one actual harness input"
    );
}

#[test]
fn structured_to_pty_to_reply() {
    let fleet = setup();
    let node = fleet.node("nodea");
    let mut sender = Bridge::start(node, "native", true);
    let mut recipient = Bridge::start(node, "terminal", false);
    exchange(&mut sender, &mut recipient, "native-pty");
}
#[test]
fn structured_to_structured_to_reply() {
    let fleet = setup();
    let node = fleet.node("nodea");
    let mut sender = Bridge::start(node, "first", true);
    let mut recipient = Bridge::start(node, "second", true);
    exchange(&mut sender, &mut recipient, "native-native");
}
#[test]
fn remote_structured_recipient_replies_to_the_other_node() {
    let fleet = setup();
    let mut sender = Bridge::start(fleet.node("nodeb"), "terminal", false);
    let mut recipient = Bridge::start(fleet.node("nodea"), "native", true);
    let deadline = Instant::now() + LIMIT;
    loop {
        let listing = sender.tool("flock_agent_list", json!({}));
        if listing["fleet"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["agent_id"] == recipient.agent)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "recipient absent from remote directory: {listing}"
        );
    }
    exchange(&mut sender, &mut recipient, "remote-native");
}
