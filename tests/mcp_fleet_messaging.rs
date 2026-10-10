//! E2E: an agent on one host discovers and messages an agent on ANOTHER host,
//! using only MCP tools (#320).
//!
//! Two isolated `flk` servers, mutually peered over the fake-`ssh` shim
//! (`support::fleet`), plus a real `flk mcp serve` speaking JSON-RPC over
//! stdio (`tests/mcp_serve.rs`'s shape). Both halves already existed; the gap
//! this covers is what happens when they meet — the MCP surface could not
//! express a fleet-global target and had no tool that would name one, so
//! cross-host messaging was reachable from the CLI and from nowhere else.
//!
//! The MCP server runs INSIDE a pane on node A rather than as a child of the
//! test. That is not incidental: a cross-host send has to carry a sender the
//! receiving host can name, and the only sender flock will attest is one it
//! can find in the caller's process ancestry. A client parented to the test
//! harness is correctly refused, so a harness that spawned it that way would
//! be testing the refusal, not the feature.

// Integration tests exec real git/ssh to build their fake fleet, and this one
// drives a subprocess over pipes — the TracedCommand funnel polices flock's
// own subprocesses, not the harness's.
#![allow(clippy::disallowed_methods)]
#![allow(clippy::print_stderr)]

mod support;

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use support::fleet::{self, NodeSpec, HUB_SPOKES};

/// Reciprocal peers exercise discovery racing with down-gossip. Replies use
/// the persisted binding and collection, independent of the return directory.
const PAIR_AB: &[NodeSpec] = &[
    NodeSpec::new("nodea", "alpha", &["nodeb"]),
    NodeSpec::new("nodeb", "beta", &["nodea"]),
];

const GOSSIP_TIMEOUT: Duration = Duration::from_secs(30);
const RPC_TIMEOUT: Duration = Duration::from_secs(15);

// Advance only the sandbox's durable collection deadline after a held answer.
// An earlier empty poll correctly backs off for 60s in production.
fn collect_now(node: &fleet::Node) {
    rusqlite::Connection::open(node.home.join("state/flock-dev/mesh-mail.sqlite"))
        .unwrap()
        .execute("UPDATE envelopes SET collect_at=0", [])
        .unwrap();
}

// ---- MCP client that lives inside a pane ---------------------------------

/// `flk mcp serve` running as a pane's process on a fleet node, wired to a
/// pair of FIFOs so the test can speak exact bytes to it.
///
/// A PTY would have worked too, but only by making every assertion fight echo,
/// line wrapping and ANSI. FIFOs keep the transport boring so the test is
/// about addressing.
struct PanedMcp {
    stdin: File,
    stdout: BufReader<File>,
    stderr_path: PathBuf,
    pane_id: String,
    agent_id: String,
    next_id: u64,
}

fn mkfifo(path: &Path) {
    let raw = CString::new(path.as_os_str().as_encoded_bytes()).expect("fifo path has no NUL");
    let rc = unsafe { libc::mkfifo(raw.as_ptr(), 0o600) };
    assert_eq!(rc, 0, "mkfifo {} failed", path.display());
}

/// Open a FIFO with a deadline. A FIFO open blocks until the other end shows
/// up, which is exactly the handshake we want — and exactly what wedges the
/// suite if the pane never started. The blocked `open` cannot be cancelled, so
/// the timeout is enforced by the caller and the doomed thread is left to die
/// with the process.
fn open_fifo_or_timeout(path: &Path, write: bool, timeout: Duration, what: &str) -> File {
    let (tx, rx) = mpsc::channel();
    let owned = path.to_path_buf();
    thread::spawn(move || {
        let opened = if write {
            OpenOptions::new().write(true).open(&owned)
        } else {
            OpenOptions::new().read(true).open(&owned)
        };
        let _ = tx.send(opened);
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(file)) => file,
        Ok(Err(err)) => panic!("opening {what} at {} failed: {err}", path.display()),
        Err(_) => panic!(
            "{what} at {} never opened — the MCP pane did not start",
            path.display()
        ),
    }
}

impl PanedMcp {
    /// Start `flk mcp serve` as an agent pane on `node` and connect to it.
    fn start(node: &fleet::Node, base: &Path) -> Self {
        Self::start_named(node, base, "mcpbridge")
    }

    fn start_named(node: &fleet::Node, base: &Path, name: &str) -> Self {
        let dir = base.join(format!("mcp-bridge-{}", node.name));
        std::fs::create_dir_all(&dir).unwrap();
        let to_mcp = dir.join("in");
        let from_mcp = dir.join("out");
        let stderr_path = dir.join("err");
        mkfifo(&to_mcp);
        mkfifo(&from_mcp);

        // NOT `exec`, deliberately. `exec` would make the pane's own child pid
        // the MCP server, which reads as tidier — and kills it on Linux. It
        // replaces every fd the shell holds on the PTY slave with these FIFOs,
        // so nothing on the child side keeps the slave open, a read of the
        // master returns EIO, and flock correctly concludes the pane's process
        // is gone and reaps it. macOS blocks on that read instead of erroring,
        // which is why the exec'd version passed there and nowhere else.
        // Keeping the shell means the pane keeps its terminal, and ancestry
        // still attests the sender: the walk climbs 16 levels, and one shell is
        // one of them.
        let command = format!(
            "{} mcp serve <{} >{} 2>{}",
            env!("CARGO_BIN_EXE_flk"),
            to_mcp.display(),
            from_mcp.display(),
            stderr_path.display(),
        );
        let response = node.api(&format!(
            r#"{{"id":"t:start","method":"agent.start","params":{{"name":{},"argv":["/bin/sh","-c",{}],"cwd":"{}"}}}}"#,
            serde_json::to_string(name).unwrap(),
            serde_json::to_string(&command).unwrap(),
            node.repo.display(),
        ));
        let started: Value = serde_json::from_str(&response)
            .unwrap_or_else(|e| panic!("agent.start on {}: {response} ({e})", node.name));
        let agent = &started["result"]["agent"];
        let pane_id = agent["pane_id"]
            .as_str()
            .unwrap_or_else(|| panic!("agent.start returned no pane: {response}"))
            .to_string();
        let agent_id = agent["agent_id"].as_str().expect("agent id").to_string();

        // Order matters and is forced by FIFO semantics: our write end unblocks
        // the pane's `<in`, which lets it reach `>out`, which our read end then
        // unblocks. Reversing these two lines deadlocks.
        let stdin = open_fifo_or_timeout(&to_mcp, true, GOSSIP_TIMEOUT, "MCP stdin");
        let stdout = open_fifo_or_timeout(&from_mcp, false, GOSSIP_TIMEOUT, "MCP stdout");

        let mut mcp = Self {
            stdin,
            stdout: BufReader::new(stdout),
            stderr_path,
            pane_id,
            agent_id,
            next_id: 0,
        };
        mcp.handshake();
        mcp
    }

    fn handshake(&mut self) {
        let init = self.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "fleet-e2e", "version": "0"},
            }),
        );
        assert_eq!(
            init["result"]["serverInfo"]["name"], "flock",
            "handshake: {init}"
        );
        self.notify("notifications/initialized");
    }

    fn notify(&mut self, method: &str) {
        self.write_line(&json!({"jsonrpc": "2.0", "method": method}));
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.write_line(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        let response = self.read_line();
        assert_eq!(response["id"], id, "response is for the request we sent");
        response
    }

    /// Call a tool and return the decoded flock payload it wrapped in text
    /// content. Panics if the tool refused — use [`call_tool_error`] for that.
    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        let response = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        assert!(
            response.get("error").is_none(),
            "{name} refused: {response}{}",
            self.stderr_tail()
        );
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{name} returned no text content: {response}"));
        serde_json::from_str(text)
            .unwrap_or_else(|e| panic!("{name} content is not JSON: {text} ({e})"))
    }

    /// Call a tool expecting a refusal, and return the error object.
    fn call_tool_error(&mut self, name: &str, arguments: Value) -> Value {
        let response = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        assert!(
            response.get("error").is_some(),
            "{name} was expected to refuse but answered: {response}"
        );
        response["error"].clone()
    }

    fn write_line(&mut self, message: &Value) {
        let mut buf = serde_json::to_vec(message).unwrap();
        buf.push(b'\n');
        self.stdin.write_all(&buf).unwrap_or_else(|e| {
            panic!("writing to the MCP pane failed: {e}{}", self.stderr_tail())
        });
        self.stdin.flush().unwrap();
    }

    fn read_line(&mut self) -> Value {
        // A FIFO read blocks forever, and a hung MCP server would surface as a
        // suite-wide timeout with nothing attached. Wait on the fd first so the
        // failure is this test's, and carries the server's own stderr.
        self.await_readable();
        let mut line = String::new();
        let n = self
            .stdout
            .read_line(&mut line)
            .unwrap_or_else(|e| panic!("reading the MCP pane failed: {e}"));
        assert!(
            n > 0,
            "the MCP pane closed its output{}",
            self.stderr_tail()
        );
        serde_json::from_str(line.trim())
            .unwrap_or_else(|e| panic!("non-JSON line from the MCP pane: {line:?} ({e})"))
    }

    /// Block until a whole line is available, or fail the test.
    fn await_readable(&mut self) {
        let deadline = Instant::now() + RPC_TIMEOUT;
        loop {
            // Already buffered from a previous read: poll would say "nothing
            // to read" while a complete response sits in the BufReader.
            if self.stdout.buffer().contains(&b'\n') {
                return;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "the MCP pane answered nothing within {RPC_TIMEOUT:?}{}",
                self.stderr_tail()
            );
            let mut fd = libc::pollfd {
                fd: self.stdout.get_ref().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe {
                libc::poll(
                    &mut fd,
                    1,
                    remaining.as_millis().min(i32::MAX as u128) as libc::c_int,
                )
            };
            if ready > 0 {
                return;
            }
            assert!(ready == 0, "poll on the MCP pane failed");
        }
    }

    fn stderr_tail(&self) -> String {
        match std::fs::read_to_string(&self.stderr_path) {
            Ok(text) if !text.trim().is_empty() => format!("\n--- mcp stderr ---\n{text}"),
            _ => String::new(),
        }
    }
}

// ---- fleet helpers -------------------------------------------------------

/// Poll `probe` until it returns a value, or fail with the last thing it saw.
fn wait_for<T>(what: &str, timeout: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(found) = probe() {
            return found;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(200));
    }
}

fn fleet_row<'a>(listing: &'a Value, agent_id: &str) -> Option<&'a Value> {
    listing["fleet"]
        .as_array()?
        .iter()
        .find(|row| row["agent_id"] == agent_id)
}

// ---- the case ------------------------------------------------------------

/// The whole feature in one pass, because the halves are worthless apart: an
/// agent that can address another host but cannot learn its id has nothing to
/// put in the field, and an agent that can list the fleet but not target it
/// has nothing to do with the answer.
#[test]
fn an_agent_discovers_and_messages_another_host_through_mcp_alone() {
    let fleet = fleet::spawn("mcp-fleet", fleet::CHAIN_ABC);
    let node_a = fleet.node("nodea");
    let node_c = fleet.node("nodec");

    // Both agents ARE their MCP servers, each in a pane on its own node.
    // Nothing in this test reaches a flock API except through a tool call, so
    // a gap in the MCP surface cannot be papered over by the harness.
    let mut alice = PanedMcp::start(node_a, &fleet.base);
    let mut bob = PanedMcp::start(node_c, &fleet.base);

    // 1. Discovery. The listing is A's own panes PLUS the directory, and only
    //    the directory can name an agent that is not here.
    let listing = wait_for(
        "nodec's agent to reach nodea's directory",
        GOSSIP_TIMEOUT,
        || {
            let listing = alice.call_tool("flock_agent_list", json!({}));
            fleet_row(&listing, &bob.agent_id).map(|_| listing.clone())
        },
    );

    let remote = fleet_row(&listing, &bob.agent_id).expect("just found it");
    assert_eq!(
        remote["local"], false,
        "an agent on another machine must not look addressable by pane id: {remote}"
    );
    assert_eq!(remote["host"], "nodec", "the row names where it lives");
    assert_eq!(
        remote["route"], "nodeb",
        "and how this server reaches it: {remote}"
    );

    let local = fleet_row(&listing, &alice.agent_id)
        .unwrap_or_else(|| panic!("the caller's own agent is missing from the fleet: {listing}"));
    assert_eq!(local["local"], true);
    assert_eq!(local["pane_id"], alice.pane_id.as_str());

    // The local half of the tool is unchanged: `agents` is still this
    // server's panes, and B's agent is NOT one of them.
    let local_ids: Vec<&str> = listing["agents"]
        .as_array()
        .expect("agents array")
        .iter()
        .filter_map(|agent| agent["agent_id"].as_str())
        .collect();
    assert!(
        !local_ids.contains(&bob.agent_id.as_str()),
        "a remote agent must not be reported as a local pane: {listing}"
    );

    // A name is a label; the id is the address. Renaming B's agent between
    // discovery and delivery must change nothing — if the route were carrying
    // the name, this is where it would break.
    let renamed = node_c.api(&format!(
        r#"{{"id":"t:rename","method":"agent.rename","params":{{"target":"{}","name":"renamed-mid-flight"}}}}"#,
        bob.pane_id
    ));
    assert!(
        renamed.contains("\"result\""),
        "agent.rename on nodec: {renamed}"
    );

    // 2. Addressing. The id from the listing goes straight into the target.
    let queued = alice.call_tool(
        "flock_msg_send",
        json!({
            "to": {"type": "agent", "agent": bob.agent_id},
            "body": "ping from nodea over mcp",
            "correlation_id": "c-320-e2e",
            // #280. Required on this tool, and it has a hop to survive: the
            // relay rebuilds the send as a `flk msg send` on the owning
            // server, so a stamp dropped here is a cross-host question
            // arriving as a notice.
            "intent": "needs_reply",
        }),
    );
    assert_eq!(queued["correlation_id"], "c-320-e2e", "send: {queued}");

    // 3. It arrives, and B reads it as its OWN inbox — no addressing, the
    //    same call a real agent makes when its stop hook wakes it.
    let delivered = wait_for("the message to land in nodec's inbox", RPC_TIMEOUT, || {
        let inbox = bob.call_tool("flock_msg_read", json!({}));
        inbox["messages"].as_array()?.first().cloned()
    });
    assert_eq!(delivered["body"], "ping from nodea over mcp");
    assert_eq!(
        delivered["from_agent"],
        alice.agent_id.as_str(),
        "the sender must survive the hop as an identity, not a pane: {delivered}"
    );
    assert_eq!(
        delivered["from_host"],
        Value::Null,
        "a gossip-only origin has no locally pinned host label: {delivered}"
    );
    assert_eq!(
        delivered["replyable"], true,
        "a message that cannot be answered is a dead end: {delivered}"
    );
    assert_eq!(
        delivered["intent"], "needs_reply",
        "the sender's stamp has to survive the hop, or a cross-host question \
         arrives as a notice: {delivered}"
    );

    // 4. The reply routes home, with B addressing nothing at all.
    let correlation_id = delivered["correlation_id"]
        .as_str()
        .expect("correlation id");
    bob.call_tool(
        "flock_msg_reply",
        json!({"correlation_id": correlation_id, "body": "pong from nodec"}),
    );

    collect_now(fleet.node("nodea"));
    let answer = wait_for("the reply to come back to nodea", RPC_TIMEOUT, || {
        let inbox = alice.call_tool("flock_msg_read", json!({}));
        inbox["messages"].as_array()?.first().cloned()
    });
    assert_eq!(answer["body"], "pong from nodec");
    assert_eq!(answer["from_agent"], bob.agent_id.as_str());
    assert_eq!(answer["from_host"], Value::Null);
    assert_eq!(
        answer["in_reply_to"], "c-320-e2e",
        "the answer has to thread back to the question: {answer}"
    );
    assert_eq!(
        answer["intent"], "fyi",
        "an answer ends the exchange unless it says otherwise — `intent` is \
         optional on reply and defaults quiet: {answer}"
    );

    let status = alice.call_tool("flock_msg_status", json!({"correlation_id":correlation_id}));
    assert!(status["reference"].is_object(), "{status}");
    assert_eq!(status["reply"]["body"], "pong from nodec", "{status}");
    let waited = alice.call_tool(
        "flock_msg_wait_reply",
        json!({
            "correlation_id":correlation_id, "timeout_ms":0, "reference":status["reference"]
        }),
    );
    assert_eq!(waited["outcome"], "replied", "{waited}");

    // 4b. ADR-0018 §1: the top tier crosses the hop too, so the recipient's
    //     own server — the one that knows whether it is muted — can escalate.
    alice.call_tool(
        "flock_msg_send",
        json!({
            "to": {"type": "agent", "agent": bob.agent_id},
            "body": "cannot merge until you rebase",
            "correlation_id": "c-408-blocking",
            "intent": "blocking",
        }),
    );
    let escalated = wait_for("the blocking message to reach nodec", RPC_TIMEOUT, || {
        let inbox = bob.call_tool("flock_msg_read", json!({}));
        inbox["messages"].as_array()?.first().cloned()
    });
    assert_eq!(
        escalated["intent"], "blocking",
        "a relayed blocking message must not arrive demoted: {escalated}"
    );

    // 5. A target that does not exist is refused by name, never dropped.
    let error = alice.call_tool_error(
        "flock_msg_send",
        json!({
            "to": {"type": "agent", "agent": "agent_nowhere_00000000"},
            "body": "into the void",
            "intent": "fyi",
        }),
    );
    let message = serde_json::to_string(&error).unwrap();
    assert!(
        message.contains("agent_nowhere_00000000"),
        "the refusal has to name what it could not find: {message}"
    );

    // 6. A pane id names a placement on ONE server. Put one in the agent
    //    field and it addresses nobody — guessing would be worse than the
    //    refusal, because the guess lands on some other agent.
    let error = alice.call_tool_error(
        "flock_msg_send",
        json!({
            "to": {"type": "agent", "agent": bob.pane_id},
            "body": "wrong field",
            "intent": "fyi",
        }),
    );
    let message = serde_json::to_string(&error).unwrap();
    assert!(
        message.contains(&bob.pane_id),
        "a pane id used as an agent id must be refused, not resolved: {message}"
    );
}

/// ADR-0018 §3 across a machine boundary: a mute must answer a sender on
/// ANOTHER host the same way it answers a local one, through the ordinary
/// reply path. Both triggers, because they take different roads home:
///
/// - a question already waiting when the mute is set is answered from the
///   mute call itself;
/// - a question arriving INTO the mute is answered while the relayed send
///   that carried it is still being handled — the sender's server is at that
///   moment inside its own ssh hop to us. A deferral sent back synchronously
///   would wait on a server that is waiting on it.
#[test]
fn a_mute_answers_a_sender_on_another_host() {
    let fleet = fleet::spawn("mcp-fleet-mute", fleet::CHAIN_ABC);
    let mut alice = PanedMcp::start(fleet.node("nodea"), &fleet.base);
    let mut bob = PanedMcp::start(fleet.node("nodec"), &fleet.base);

    // Discovery addresses A → B → C. The durable binding routes the deferral home.
    wait_for("nodec's agent in nodea's directory", GOSSIP_TIMEOUT, || {
        let listing = alice.call_tool("flock_agent_list", json!({}));
        fleet_row(&listing, &bob.agent_id).map(|_| ())
    });

    // 1. Waiting before the mute.
    alice.call_tool(
        "flock_msg_send",
        json!({
            "to": {"type": "agent", "agent": bob.agent_id},
            "body": "are you free to review?",
            "correlation_id": "c-408-before",
            "intent": "needs_reply",
        }),
    );
    wait_for("the question to land on nodec", RPC_TIMEOUT, || {
        let queued = bob.call_tool("flock_msg_list", json!({"pane": bob.pane_id}));
        (!queued["messages"].as_array()?.is_empty()).then_some(())
    });

    let muted = bob.call_tool(
        "flock_msg_mute",
        json!({"seconds": 600, "reason": "mid-rebase"}),
    );
    let until = muted["muted_until_ms"].as_u64().expect("lift time");
    assert!(until > 0, "{muted}");
    assert_eq!(
        muted["deferred"], 1,
        "the waiting question is answered: {muted}"
    );
    collect_now(fleet.node("nodea"));

    let deferral = wait_for("the deferral to reach nodea", RPC_TIMEOUT, || {
        let inbox = alice.call_tool("flock_msg_read", json!({}));
        inbox["messages"].as_array()?.first().cloned()
    });
    assert_eq!(deferral["in_reply_to"], "c-408-before", "{deferral}");
    assert_eq!(
        deferral["intent"], "fyi",
        "never a question back: {deferral}"
    );
    assert_eq!(deferral["from_agent"], bob.agent_id.as_str(), "{deferral}");
    assert_eq!(deferral["from_host"], Value::Null, "{deferral}");
    let body = deferral["body"].as_str().expect("body");
    assert!(body.contains("mid-rebase"), "the reason travels: {body}");
    assert!(
        body.contains(&format!("muted_until_ms={until}")),
        "the deadline travels: {body}"
    );

    // 2. Arriving into the mute.
    alice.call_tool(
        "flock_msg_send",
        json!({
            "to": {"type": "agent", "agent": bob.agent_id},
            "body": "ping again",
            "correlation_id": "c-408-during",
            "intent": "needs_reply",
        }),
    );
    collect_now(fleet.node("nodea"));
    let arrived = wait_for(
        "the arrival-time deferral to reach nodea",
        RPC_TIMEOUT,
        || {
            let inbox = alice.call_tool("flock_msg_read", json!({}));
            let messages = inbox["messages"].as_array()?.clone();
            (!messages.is_empty()).then_some(messages)
        },
    );
    assert_eq!(arrived.len(), 1, "exactly one deferral: {arrived:?}");
    let deferral = &arrived[0];
    assert_eq!(deferral["in_reply_to"], "c-408-during", "{deferral}");
    assert_eq!(deferral["intent"], "fyi", "{deferral}");

    // 3. Renewing answers nobody twice, and a notice is owed nothing.
    let renewed = bob.call_tool("flock_msg_mute", json!({"seconds": 900}));
    assert_eq!(renewed["deferred"], 0, "{renewed}");
    alice.call_tool(
        "flock_msg_send",
        json!({
            "to": {"type": "agent", "agent": bob.agent_id},
            "body": "just so you know",
            "intent": "fyi",
        }),
    );
    wait_for("the notice to land on nodec", RPC_TIMEOUT, || {
        let queued = bob.call_tool("flock_msg_list", json!({"pane": bob.pane_id}));
        (queued["messages"].as_array()?.len() == 3).then_some(())
    });
    // Give a stray deferral the same window a real one got to arrive.
    let observed = Instant::now();
    wait_for("no duplicate deferral", RPC_TIMEOUT, || {
        assert_eq!(
            alice.call_tool("flock_msg_read", json!({}))["messages"],
            json!([])
        );
        (observed.elapsed() >= Duration::from_secs(2)).then_some(())
    });
    let inbox = alice.call_tool("flock_msg_read", json!({}));
    assert_eq!(
        inbox["messages"].as_array().map(Vec::len),
        Some(0),
        "no second deferral, and none for a notice: {inbox}"
    );
}

fn spoke_api(node: &fleet::Node, method: &str, params: Value) -> Value {
    serde_json::from_str(
        &node.api(&json!({"id":"spoke-test","method":method,"params":params}).to_string()),
    )
    .unwrap()
}

fn spoke_db(node: &fleet::Node) -> rusqlite::Connection {
    let db =
        rusqlite::Connection::open(node.home.join("state/flock-dev/mesh-mail.sqlite")).unwrap();
    db.busy_timeout(Duration::from_secs(5)).unwrap();
    db
}

fn spoke_pair(label: &str, specs: &[NodeSpec]) -> (fleet::Fleet, PanedMcp, PanedMcp) {
    let fleet = fleet::spawn(label, specs);
    let mut alice = PanedMcp::start(fleet.node("nodea"), &fleet.base);
    let bob = PanedMcp::start(fleet.node("nodeb"), &fleet.base);
    wait_for(
        "hub discovery on the edge-less spoke",
        GOSSIP_TIMEOUT,
        || {
            let listing = alice.call_tool("flock_agent_list", json!({}));
            fleet_row(&listing, &bob.agent_id).map(|_| ())
        },
    );
    (fleet, alice, bob)
}

const SPOKE_PAIR: &[NodeSpec] = &[
    NodeSpec::new("nodeb", "mesh-hub", &["nodea"]),
    NodeSpec::new("nodea", "mesh-spoke", &[]),
];

fn spoke_send(alice: &mut PanedMcp, target: &str) -> Value {
    let sent = alice.call_tool(
        "flock_msg_send",
        json!({
            "to":{"type":"agent","agent":target}, "body":"durable spoke question",
            "correlation_id":"spoke-question", "intent":"needs_reply"
        }),
    );
    assert_eq!(sent["state"], "queued", "{sent}");
    assert!(sent["message_key"].is_object(), "{sent}");
    sent
}

fn spoke_wait_mail(node: &fleet::Node, pane: &str) -> Value {
    wait_for("durable spoke mail", GOSSIP_TIMEOUT, || {
        let result = spoke_api(node, "msg.read", json!({"pane":pane}));
        let messages = &result["result"]["messages"];
        (!messages.as_array()?.is_empty()).then(|| messages.clone())
    })
}

#[test]
fn spoke_custody_round_trip_uses_authenticated_origin_and_push_down() {
    let (fleet, mut alice, mut bob) = spoke_pair("h2rt", SPOKE_PAIR);
    let sent = spoke_send(&mut alice, &bob.agent_id);
    let messages = spoke_wait_mail(fleet.node("nodeb"), &bob.pane_id);
    assert_eq!(messages.as_array().unwrap().len(), 1);
    assert_eq!(messages[0]["from_host"], "nodea");
    assert_eq!(messages[0]["from_agent"], alice.agent_id);
    let imported: (String, String) = spoke_db(fleet.node("nodeb"))
        .query_row(
            "SELECT origin,id FROM envelopes WHERE correlation='spoke-question'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        json!({"origin_node": imported.0, "message_id": imported.1}),
        sent["message_key"]
    );
    bob.call_tool(
        "flock_msg_reply",
        json!({"correlation_id":"spoke-question","body":"push-down answer"}),
    );
    let answer = spoke_wait_mail(fleet.node("nodea"), &alice.pane_id);
    assert_eq!(answer.as_array().unwrap().len(), 1);
    assert_eq!(answer[0]["body"], "push-down answer");
    assert_eq!(answer[0]["from_host"], "nodeb");
    let forged_sender = spoke_api(
        fleet.node("nodea"),
        "msg.send",
        json!({
            "to":{"type":"agent","agent":bob.agent_id},
            "body":"unattested spoke sender", "intent":"fyi"
        }),
    );
    assert_eq!(forged_sender["error"]["code"], "sender_unresolved");
    for method in ["msg.uplink_take", "msg.uplink_result", "msg.uplink_forward"] {
        assert!(spoke_api(fleet.node("nodea"), method, json!({}))["error"].is_object());
    }
    let forged = spoke_api(
        fleet.node("nodea"),
        "mesh.collect",
        json!({"outbound":{"ack":[]}}),
    );
    assert_eq!(forged["error"]["code"], "mesh_collection_refused");
}

fn spoke_lost_ack_restart(restart: &str) {
    let (mut fleet, mut alice, bob) = spoke_pair("h2ack", SPOKE_PAIR);
    let gate = fleet.base.join("gate-outbound-ack-nodeb-nodea");
    std::fs::create_dir(&gate).unwrap();
    spoke_send(&mut alice, &bob.agent_id);
    wait_for("spoke ack after hub import", GOSSIP_TIMEOUT, || {
        gate.join("entered").exists().then_some(())
    });
    let messages = spoke_wait_mail(fleet.node("nodeb"), &bob.pane_id);
    assert_eq!(messages.as_array().unwrap().len(), 1);
    fleet.refuse_edge("nodeb", "nodea");
    fleet.kill_edge("nodeb", "nodea", Duration::from_secs(10));
    let remaining: i64 = spoke_db(fleet.node("nodea"))
        .query_row(
            "SELECT count(*) FROM envelopes WHERE state='held'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 1);
    std::fs::remove_dir_all(gate).unwrap();
    fleet.node_mut(restart).restart();
    fleet.allow_edge("nodeb", "nodea");
    wait_for("durable spoke ack replay", GOSSIP_TIMEOUT, || {
        let state: String = spoke_db(fleet.node("nodea"))
            .query_row(
                "SELECT state FROM envelopes WHERE correlation='spoke-question'",
                [],
                |r| r.get(0),
            )
            .ok()?;
        (state == "delivered").then_some(())
    });
    let imports: i64 = spoke_db(fleet.node("nodeb"))
        .query_row("SELECT count(*) FROM inbox_imports", [], |r| r.get(0))
        .unwrap();
    assert_eq!(imports, 1);
    assert_eq!(
        spoke_api(fleet.node("nodeb"), "msg.read", json!({"pane":bob.pane_id}))["result"]
            ["messages"],
        json!([])
    );
}

#[test]
fn spoke_custody_lost_ack_survives_hub_restart() {
    spoke_lost_ack_restart("nodeb");
}

#[test]
fn spoke_custody_lost_ack_survives_spoke_restart() {
    spoke_lost_ack_restart("nodea");
}

#[test]
fn spoke_custody_offline_push_down_is_held_until_reenrollment() {
    let (fleet, mut alice, mut bob) = spoke_pair("h2off", SPOKE_PAIR);
    spoke_send(&mut alice, &bob.agent_id);
    spoke_wait_mail(fleet.node("nodeb"), &bob.pane_id);
    fleet.refuse_edge("nodeb", "nodea");
    fleet.kill_edge("nodeb", "nodea", Duration::from_secs(10));
    let result = bob.call_tool(
        "flock_msg_reply",
        json!({"correlation_id":"spoke-question","body":"held for spoke"}),
    );
    assert_eq!(result["state"], "held", "{result}");
    wait_for("answer held durably", GOSSIP_TIMEOUT, || {
        let count: i64 = spoke_db(fleet.node("nodeb"))
            .query_row(
                "SELECT count(*) FROM envelopes WHERE state='held' AND request_origin IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .ok()?;
        (count == 1).then_some(())
    });
    fleet.allow_edge("nodeb", "nodea");
    let answer = spoke_wait_mail(fleet.node("nodea"), &alice.pane_id);
    assert_eq!(answer.as_array().unwrap().len(), 1);
    assert_eq!(answer[0]["body"], "held for spoke");
}

#[test]
fn spoke_custody_allow_from_refuses_authenticated_spoke() {
    const DENIED: &[NodeSpec] = &[
        NodeSpec::new("nodeb", "denied-hub", &["nodea"])
            .with_config("[msg]\nallow_from = [\"other.example\"]\n"),
        NodeSpec::new("nodea", "denied-spoke", &[]),
    ];
    let (fleet, mut alice, bob) = spoke_pair("h2deny", DENIED);
    spoke_send(&mut alice, &bob.agent_id);
    wait_for(
        "origin policy refusal returned durably",
        GOSSIP_TIMEOUT,
        || {
            let status = spoke_api(
                fleet.node("nodea"),
                "msg.status",
                json!({"correlation_id":"spoke-question"}),
            );
            (status["result"]["detail"] == "msg_not_allowed").then_some(())
        },
    );
    assert_eq!(
        spoke_api(fleet.node("nodeb"), "msg.read", json!({"pane":bob.pane_id}))["result"]
            ["messages"],
        json!([])
    );
}

#[test]
fn spoke_custody_remote_target_beyond_hub_is_delivered_once() {
    let (fleet, mut alice, _) = spoke_pair("h2limit", HUB_SPOKES);
    let carol = PanedMcp::start(fleet.node("nodec"), &fleet.base);
    wait_for("hub knows remote recipient", GOSSIP_TIMEOUT, || {
        let listing = spoke_api(fleet.node("nodeb"), "agent.list", json!({}));
        fleet_row(&listing["result"], &carol.agent_id).map(|_| ())
    });
    wait_for("spoke knows remote owner", GOSSIP_TIMEOUT, || {
        let listing = spoke_api(fleet.node("nodea"), "agent.list", json!({}));
        fleet_row(&listing["result"], &carol.agent_id).map(|_| ())
    });
    fleet.wait_route("nodea", "nodec", true);
    spoke_send(&mut alice, &carol.agent_id);
    let mail = spoke_wait_mail(fleet.node("nodec"), &carol.pane_id);
    assert_eq!(mail.as_array().unwrap().len(), 1);
    assert_eq!(mail[0]["correlation_id"], "spoke-question");
    assert_eq!(
        spoke_api(
            fleet.node("nodec"),
            "msg.read",
            json!({"pane":carol.pane_id})
        )["result"]["messages"],
        json!([])
    );
}

#[test]
fn spoke_custody_hub_live_handoff_mid_conversation() {
    let (fleet, mut alice, bob) = spoke_pair("h2ho", SPOKE_PAIR);
    spoke_send(&mut alice, &bob.agent_id);
    spoke_wait_mail(fleet.node("nodeb"), &bob.pane_id);
    let hub = fleet.node("nodeb");
    let old_pid = hub.process_id();
    let result = spoke_api(hub, "server.live_handoff", json!({}));
    assert!(result.get("error").is_none(), "{result}");
    let replacement = wait_for("hub handoff replacement", Duration::from_secs(15), || {
        support::flock_server_pids_for_runtime_dir(&hub.runtime_dir)
            .ok()?
            .into_iter()
            .find(|pid| *pid != old_pid)
    });
    support::register_spawned_flock_pid(Some(replacement));
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
    wait_for("handoff reply acceptance", GOSSIP_TIMEOUT, || {
        let response = spoke_api(
            hub,
            "msg.reply",
            json!({"correlation_id":"spoke-question","body":"answer after handoff"}),
        );
        response.get("result").cloned()
    });
    let answer = spoke_wait_mail(fleet.node("nodea"), &alice.pane_id);
    assert_eq!(answer.as_array().unwrap().len(), 1);
    assert_eq!(answer[0]["body"], "answer after handoff");
}

/// Hold the actual legacy SSH command until another app-loop API responds.
/// Ping is served by the socket thread, so workspace.list is the probe.
fn slow_message_hop_keeps_api_responsive(specs: &[NodeSpec], recipient: &str, relay: &str) {
    let fleet = fleet::spawn_with_startup_probe("slow-message-hop", specs, |fleet, name| {
        if relay == "nodeb" && name == "nodea" {
            // Force the hub's first dial before this spoke exists. The shim
            // must wait for readiness, not refuse hello and back off for 60s.
            fleet::wait_until("hub dialing the unstarted spoke", GOSSIP_TIMEOUT, || {
                fleet
                    .base
                    .join("startup-wait-nodeb-nodea")
                    .exists()
                    .then_some(())
            });
        }
    });
    let source = fleet.node("nodea");
    let destination = fleet.node(recipient);
    let mut alice = PanedMcp::start(source, &fleet.base);
    let mut bob = PanedMcp::start(destination, &fleet.base);
    wait_for("remote agent discovery", GOSSIP_TIMEOUT, || {
        let listing = alice.call_tool("flock_agent_list", json!({}));
        fleet_row(&listing, &bob.agent_id).map(|_| ())
    });

    let gate = fleet.gate_message_edge(relay, recipient);
    let target = bob.agent_id.clone();
    let send = thread::spawn(move || {
        alice.call_tool(
            "flock_msg_send",
            json!({
                "to": {"type": "agent", "agent": target},
                "body": "slow peer question", "intent": "needs_reply",
                "correlation_id": "slow-hop-question"
            }),
        )
    });
    gate.wait_entered(GOSSIP_TIMEOUT);

    let mut socket =
        std::os::unix::net::UnixStream::connect(&fleet.node(relay).api_socket).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    writeln!(
        socket,
        "{}",
        json!({"id":"responsive", "method":"workspace.list", "params": {}})
    )
    .unwrap();
    let mut answer = String::new();
    let result = BufReader::new(socket).read_line(&mut answer);
    // Release even on failure so the test never leaves a held SSH process.
    gate.release();
    result.expect("workspace.list must complete while the message SSH is held");
    assert_eq!(
        serde_json::from_str::<Value>(&answer).unwrap()["id"],
        "responsive"
    );
    let sent = send.join().unwrap();
    assert_eq!(sent["state"], "delivered", "{sent}");
    assert_eq!(sent["correlation_id"], "slow-hop-question");
    let read = bob.call_tool("flock_msg_read", json!({}));
    assert!(read["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| message["correlation_id"] == "slow-hop-question"));
}

#[test]
fn a_slow_direct_message_peer_does_not_stall_other_api_requests() {
    slow_message_hop_keeps_api_responsive(PAIR_AB, "nodeb", "nodea");
}

#[test]
fn direct_message_delivers_before_topology_adverts_arrive() {
    let fleet = fleet::spawn_with_startup_probe("direct-before-routes", PAIR_AB, |fleet, _| {
        std::fs::write(fleet.base.join("withhold-route-adverts"), "").unwrap();
    });
    let mut alice = PanedMcp::start(fleet.node("nodea"), &fleet.base);
    let mut bob = PanedMcp::start(fleet.node("nodeb"), &fleet.base);
    wait_for(
        "direct recipient discovery before topology exchange",
        GOSSIP_TIMEOUT,
        || {
            let listing = alice.call_tool("flock_agent_list", json!({}));
            fleet_row(&listing, &bob.agent_id).map(|_| ())
        },
    );
    let enrollment = spoke_api(fleet.node("nodea"), "peers.enrollment", json!({}));
    assert_eq!(enrollment["result"]["routes"], json!([]));
    assert!(
        enrollment["result"]["peers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|peer| peer["source"] == "configured" && peer["state"] == "pinned"),
        "{enrollment}"
    );
    let sent = alice.call_tool(
        "flock_msg_send",
        json!({
            "to": {"type":"agent", "agent":bob.agent_id},
            "body":"direct before adverts", "intent":"needs_reply",
            "correlation_id":"direct-before-routes"
        }),
    );
    assert_eq!(sent["state"], "delivered", "{sent}");
    assert_eq!(sent["path"], "direct", "{sent}");
    let read = bob.call_tool("flock_msg_read", json!({}));
    assert_eq!(
        read["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["correlation_id"] == "direct-before-routes")
            .count(),
        1,
        "{read}"
    );
}

#[test]
fn spoke_custody_twenty_unresolvable_targets_do_not_block_valid_mail() {
    let (fleet, mut alice, bob) = spoke_pair("h2bad", SPOKE_PAIR);
    let hold = fleet.base.join("hold-outbound-nodeb-nodea");
    std::fs::create_dir(&hold).unwrap();
    for i in 0..20 {
        let sent = alice.call_tool_error(
            "flock_msg_send",
            json!({
                "to":{"type":"agent","agent":"agent_spoke2_123456789abcdef0"},
                "body":"no such recipient", "correlation_id":format!("bad-{i}"), "intent":"fyi"
            }),
        );
        assert_eq!(sent["data"]["refusal"], "msg_target_not_found", "{sent}");
    }
    spoke_send(&mut alice, &bob.agent_id);
    std::fs::remove_dir_all(hold).unwrap();
    let mail = spoke_wait_mail(fleet.node("nodeb"), &bob.pane_id);
    assert_eq!(mail.as_array().unwrap().len(), 1);
    let count: i64 = spoke_db(fleet.node("nodea"))
        .query_row(
            "SELECT count(*) FROM envelopes WHERE correlation LIKE 'bad-%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "unknown owners were never admitted into custody");
}

#[test]
fn spoke_custody_one_undecodable_record_does_not_discard_its_batch() {
    let (fleet, mut alice, bob) = spoke_pair("h2wire", SPOKE_PAIR);
    let hold = fleet.base.join("hold-outbound-nodeb-nodea");
    std::fs::create_dir(&hold).unwrap();
    std::fs::write(fleet.base.join("corrupt-outbound-nodeb-nodea"), "").unwrap();
    for i in 0..2 {
        let sent = alice.call_tool(
            "flock_msg_send",
            json!({
                "to":{"type":"agent","agent":bob.agent_id}, "body":"wire record",
                "correlation_id":format!("wire-{i}"), "intent":"fyi"
            }),
        );
        assert_eq!(sent["state"], "queued", "{sent}");
    }
    std::fs::remove_dir_all(hold).unwrap();
    assert_eq!(
        spoke_wait_mail(fleet.node("nodeb"), &bob.pane_id)
            .as_array()
            .unwrap()
            .len(),
        1
    );
    wait_for("isolated malformed record refusal", GOSSIP_TIMEOUT, || {
        let count: i64 = spoke_db(fleet.node("nodea")).query_row(
            "SELECT count(*) FROM envelopes WHERE state='refused' AND collect_error='invalid_envelope'", [], |r| r.get(0)).ok()?;
        (count == 1).then_some(())
    });
}

#[test]
fn spoke_custody_offline_send_survives_missing_runtime_enrollment() {
    let (mut fleet, mut alice, bob) = spoke_pair("h2down", SPOKE_PAIR);
    fleet.refuse_edge("nodeb", "nodea");
    fleet.kill_edge("nodeb", "nodea", Duration::from_secs(10));
    drop(alice);
    fleet.node_mut("nodea").restart();
    alice = PanedMcp::start_named(
        fleet.node("nodea"),
        &fleet.base.join("after-restart"),
        "mcpbridge2",
    );
    spoke_send(&mut alice, &bob.agent_id);
    fleet.allow_edge("nodeb", "nodea");
    assert_eq!(
        spoke_wait_mail(fleet.node("nodeb"), &bob.pane_id)
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn spoke_custody_lost_ack_while_muted_is_read_only_and_eventually_stops_offers() {
    let (fleet, mut alice, mut bob) = spoke_pair("h2mute", SPOKE_PAIR);
    let muted = bob.call_tool("flock_msg_mute", json!({"seconds":900}));
    assert!(muted["muted_until_ms"].as_u64().is_some(), "{muted}");
    let lost = fleet.base.join("lose-outbound-ack-nodeb-nodea");
    std::fs::write(&lost, "").unwrap();
    let sent = alice.call_tool(
        "flock_msg_send",
        json!({
            "to":{"type":"agent","agent":bob.agent_id}, "body":"unread while muted",
            "correlation_id":"muted-ack", "intent":"fyi"
        }),
    );
    assert_eq!(sent["state"], "queued", "{sent}");
    wait_for("first ack lost", GOSSIP_TIMEOUT, || {
        fleet
            .base
            .join("lost-outbound-ack-nodeb-nodea")
            .exists()
            .then_some(())
    });
    let db = spoke_db(fleet.node("nodeb"));
    let version = || {
        db.query_row("PRAGMA data_version", [], |r| r.get::<_, i64>(0))
            .unwrap()
    };
    // A lost ack now leaves receipt debt pending until the origin explicitly
    // acknowledges it. Permit that one bookkeeping update, but audit every
    // other write so duplicate import must still be read-only.
    db.execute_batch("CREATE TABLE replay_writes (table_name TEXT)")
        .unwrap();
    let tables: Vec<String> = db.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name!='replay_writes'"
    ).unwrap().query_map([], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
    for table in tables {
        let quoted = format!("\"{}\"", table.replace('"', "\"\""));
        for operation in ["INSERT", "UPDATE", "DELETE"] {
            let condition = if table == "envelopes" && operation == "UPDATE" {
                let columns: Vec<String> = db.prepare(
                    "SELECT name FROM pragma_table_info('envelopes') WHERE name!='receipt_sent'"
                ).unwrap().query_map([], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
                format!("WHEN NOT (OLD.receipt_sent IS NULL AND NEW.receipt_sent IS 'delivered' AND {})",
                    columns.iter().map(|column| format!("OLD.\"{column}\" IS NEW.\"{column}\"")).collect::<Vec<_>>().join(" AND "))
            } else {
                String::new()
            };
            db.execute_batch(&format!(
                "CREATE TRIGGER replay_{table}_{operation} AFTER {operation} ON {quoted} {condition}
                 BEGIN INSERT INTO replay_writes VALUES ('{table}'); END"
            ))
            .unwrap();
        }
    }
    let replay_writes = || {
        db.query_row("SELECT COUNT(*) FROM replay_writes", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap()
    };
    let offers = fleet.base.join("outbound-offers-nodeb-nodea");
    wait_for(
        "duplicate offered to muted recipient",
        GOSSIP_TIMEOUT,
        || (std::fs::read_to_string(&offers).ok()?.lines().count() >= 2).then_some(()),
    );
    assert_eq!(replay_writes(), 0, "duplicate import must not write on hub");
    wait_for("next ack settles custody", GOSSIP_TIMEOUT, || {
        let state: String = spoke_db(fleet.node("nodea"))
            .query_row(
                "SELECT state FROM envelopes WHERE correlation='muted-ack'",
                [],
                |r| r.get(0),
            )
            .ok()?;
        (state == "delivered").then_some(())
    });
    assert_eq!(replay_writes(), 0);
    let before = version();
    let count = std::fs::read_to_string(&offers).unwrap().lines().count();
    thread::sleep(Duration::from_secs(6));
    assert_eq!(version(), before);
    assert_eq!(
        std::fs::read_to_string(&offers).unwrap().lines().count(),
        count
    );
    assert_eq!(
        spoke_wait_mail(fleet.node("nodeb"), &bob.pane_id)
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn spoke_custody_idle_pinned_peer_does_not_poll_every_second() {
    let (fleet, _alice, _bob) = spoke_pair("h2idle", SPOKE_PAIR);
    let polls = fleet.base.join("outbound-polls-nodeb-nodea");
    thread::sleep(Duration::from_secs(8));
    let times: Vec<f64> = std::fs::read_to_string(polls)
        .unwrap()
        .lines()
        .map(|s| s.parse().unwrap())
        .collect();
    assert!(times.len() <= 2, "idle polls: {times:?}");
    for pair in times.windows(2) {
        assert!(pair[1] - pair[0] >= 5.0, "{times:?}");
    }
}

#[test]
fn spoke_custody_hub_busy_import_retries_without_refusal() {
    let (fleet, mut alice, bob) = spoke_pair("h2busy", SPOKE_PAIR);
    let hub_db = spoke_db(fleet.node("nodeb"));
    hub_db.execute_batch("BEGIN IMMEDIATE").unwrap();
    spoke_send(&mut alice, &bob.agent_id);
    let source_db = spoke_db(fleet.node("nodea"));
    wait_for("second offer after hub busy import", GOSSIP_TIMEOUT, || {
        let (state, attempts): (String, i64) = source_db
            .query_row(
                "SELECT state,collect_attempts FROM envelopes WHERE correlation='spoke-question'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "held", "a busy hub must not refuse custody");
        (attempts >= 2).then_some(())
    });
    hub_db.execute_batch("ROLLBACK").unwrap();
    // Bring the retry forward so this probe need not wait for the next backoff.
    source_db
        .execute(
            "UPDATE envelopes SET retry_at=0 WHERE correlation='spoke-question'",
            [],
        )
        .unwrap();
    let messages = spoke_wait_mail(fleet.node("nodeb"), &bob.pane_id);
    assert_eq!(messages.as_array().unwrap().len(), 1);
    wait_for(
        "successful ack after hub storage recovery",
        GOSSIP_TIMEOUT,
        || {
            let state: String = source_db
                .query_row(
                    "SELECT state FROM envelopes WHERE correlation='spoke-question'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            (state == "delivered" || state == "read").then_some(())
        },
    );
}
