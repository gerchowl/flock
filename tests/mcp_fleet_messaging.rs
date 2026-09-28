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
use support::fleet::{self, NodeSpec};

/// Two nodes that poll EACH OTHER. A one-way chain is enough to send, but a
/// reply has to resolve the original sender through the replier's own
/// directory — so a fleet where B never hears about A can deliver a message
/// and never answer it.
const PAIR_AB: &[NodeSpec] = &[
    NodeSpec::new("nodea", "alpha", &["nodeb"]),
    NodeSpec::new("nodeb", "beta", &["nodea"]),
];

/// Hub and spokes, the fleet's real shape (#410): only `nodeb` carries
/// `[[peers]]`, and the two spokes carry none — no reverse trust, no N×N keys.
/// The only way off a spoke is the relay the hub holds INTO it.
const HUB_SPOKES: &[NodeSpec] = &[
    NodeSpec::new("nodea", "alpha", &[]),
    NodeSpec::new("nodeb", "beta", &["nodea", "nodec"]),
    NodeSpec::new("nodec", "gamma", &[]),
];

const GOSSIP_TIMEOUT: Duration = Duration::from_secs(30);
const RPC_TIMEOUT: Duration = Duration::from_secs(15);

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
            r#"{{"id":"t:start","method":"agent.start","params":{{"name":"mcpbridge","argv":["/bin/sh","-c",{}],"cwd":"{}"}}}}"#,
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

    /// Call a tool and return its payload or its refusal, for a caller that
    /// has to tell the two apart.
    fn try_call_tool(&mut self, name: &str, arguments: Value) -> Result<Value, Value> {
        let response = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        if let Some(error) = response.get("error") {
            return Err(error.clone());
        }
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{name} returned no text content: {response}"));
        Ok(serde_json::from_str(text)
            .unwrap_or_else(|e| panic!("{name} content is not JSON: {text} ({e})")))
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
    let fleet = fleet::spawn("mcp-fleet", PAIR_AB);
    let node_a = fleet.node("nodea");
    let node_b = fleet.node("nodeb");

    // Both agents ARE their MCP servers, each in a pane on its own node.
    // Nothing in this test reaches a flock API except through a tool call, so
    // a gap in the MCP surface cannot be papered over by the harness.
    let mut alice = PanedMcp::start(node_a, &fleet.base);
    let mut bob = PanedMcp::start(node_b, &fleet.base);

    // 1. Discovery. The listing is A's own panes PLUS the directory, and only
    //    the directory can name an agent that is not here.
    let listing = wait_for(
        "nodeb's agent to reach nodea's directory",
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
    assert_eq!(remote["host"], "nodeb", "the row names where it lives");
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
    let renamed = node_b.api(&format!(
        r#"{{"id":"t:rename","method":"agent.rename","params":{{"target":"{}","name":"renamed-mid-flight"}}}}"#,
        bob.pane_id
    ));
    assert!(
        renamed.contains("\"result\""),
        "agent.rename on nodeb: {renamed}"
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
    let delivered = wait_for("the message to land in nodeb's inbox", RPC_TIMEOUT, || {
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
        delivered["from_host"], "nodea",
        "and must name the host it actually came from: {delivered}"
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
        json!({"correlation_id": correlation_id, "body": "pong from nodeb"}),
    );

    let answer = wait_for("the reply to come back to nodea", RPC_TIMEOUT, || {
        let inbox = alice.call_tool("flock_msg_read", json!({}));
        inbox["messages"].as_array()?.first().cloned()
    });
    assert_eq!(answer["body"], "pong from nodeb");
    assert_eq!(answer["from_agent"], bob.agent_id.as_str());
    assert_eq!(answer["from_host"], "nodeb");
    assert_eq!(
        answer["in_reply_to"], "c-320-e2e",
        "the answer has to thread back to the question: {answer}"
    );
    assert_eq!(
        answer["intent"], "fyi",
        "an answer ends the exchange unless it says otherwise — `intent` is \
         optional on reply and defaults quiet: {answer}"
    );

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
    let escalated = wait_for("the blocking message to reach nodeb", RPC_TIMEOUT, || {
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
    let fleet = fleet::spawn("mcp-fleet-mute", PAIR_AB);
    let mut alice = PanedMcp::start(fleet.node("nodea"), &fleet.base);
    let mut bob = PanedMcp::start(fleet.node("nodeb"), &fleet.base);

    // Each side has to be able to name the other: the question goes a→b,
    // the deferral b→a.
    wait_for("nodeb's agent in nodea's directory", GOSSIP_TIMEOUT, || {
        let listing = alice.call_tool("flock_agent_list", json!({}));
        fleet_row(&listing, &bob.agent_id).map(|_| ())
    });
    wait_for("nodea's agent in nodeb's directory", GOSSIP_TIMEOUT, || {
        let listing = bob.call_tool("flock_agent_list", json!({}));
        fleet_row(&listing, &alice.agent_id).map(|_| ())
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
    wait_for("the question to land on nodeb", RPC_TIMEOUT, || {
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
    assert_eq!(deferral["from_host"], "nodeb", "{deferral}");
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
    let deferral = wait_for(
        "the arrival-time deferral to reach nodea",
        RPC_TIMEOUT,
        || {
            let inbox = alice.call_tool("flock_msg_read", json!({}));
            inbox["messages"].as_array()?.first().cloned()
        },
    );
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
    wait_for("the notice to land on nodeb", RPC_TIMEOUT, || {
        let queued = bob.call_tool("flock_msg_list", json!({"pane": bob.pane_id}));
        (queued["messages"].as_array()?.len() == 3).then_some(())
    });
    // Give a stray deferral the same window a real one got to arrive.
    thread::sleep(Duration::from_secs(2));
    let inbox = alice.call_tool("flock_msg_read", json!({}));
    assert_eq!(
        inbox["messages"].as_array().map(Vec::len),
        Some(0),
        "no second deferral, and none for a notice: {inbox}"
    );
}

/// #410, the acceptance case. A spoke messages another spoke through the hub,
/// and the answer comes back the same way — with neither spoke holding a key
/// to anything.
///
/// Before this, `nodea` could not message `nodec` at all: it has no
/// `[[peers]]`, so the relay refused with `peer_not_configured` and advised
/// adding the very N×N trust the topology refuses. The message now goes UP the
/// relay `nodeb` holds into `nodea`, `nodeb` delivers it with its ordinary
/// `msg.send`, and the outcome — including a failure on `nodeb`'s own hop —
/// comes back down to the sender.
#[test]
fn a_spoke_messages_another_spoke_through_the_hub_and_hears_back() {
    for spec in HUB_SPOKES {
        if spec.name != "nodeb" {
            assert!(spec.peers.is_empty(), "no spoke may carry [[peers]]");
        }
    }
    let fleet = fleet::spawn("mcp-hub-spokes", HUB_SPOKES);
    let node_a = fleet.node("nodea");

    let mut alice = PanedMcp::start(node_a, &fleet.base);
    let mut carol = PanedMcp::start(fleet.node("nodec"), &fleet.base);

    // Sent until the fleet has converged: nodeb's relay into nodea is up, and
    // nodeb's poll of nodec has seen carol. Until then the refusal says which
    // of the two is missing — nodea has no hub yet, or the hub has not heard
    // of carol — and the same correlation id on every attempt means a retry
    // cannot deliver twice.
    let send = json!({
        "to": {"type": "agent", "agent": carol.agent_id},
        "body": "ping from spoke a",
        "correlation_id": "c-410-hub",
        "intent": "needs_reply",
    });
    let queued = wait_for(
        "nodeb to hold a relay into nodea",
        GOSSIP_TIMEOUT,
        || match alice.try_call_tool("flock_msg_send", send.clone()) {
            Ok(queued) => Some(queued),
            Err(error) => {
                let text = error.to_string();
                assert!(
                    text.contains("no hub holds a relay")
                        || text
                            .contains("handed up to nodeb, which could not deliver it: no agent"),
                    "the only acceptable refusals are the not-yet-converged ones: {text}"
                );
                None
            }
        },
    );
    assert_eq!(queued["state"], "relayed", "send: {queued}");
    assert_eq!(
        queued["path"], "via nodeb",
        "the send result says which hub carried it: {queued}"
    );
    assert_eq!(queued["to_host"], "nodec", "and where it went: {queued}");

    // It arrives on nodec as ALICE's, from nodea — the hub vouched for its
    // edge and did not become the sender.
    let delivered = wait_for("the message to land on nodec", RPC_TIMEOUT, || {
        let inbox = carol.call_tool("flock_msg_read", json!({}));
        inbox["messages"].as_array()?.first().cloned()
    });
    assert_eq!(delivered["body"], "ping from spoke a");
    assert_eq!(
        delivered["from_agent"],
        alice.agent_id.as_str(),
        "the originating sender survives both hops: {delivered}"
    );
    assert_eq!(
        delivered["from_host"], "nodea",
        "and names the spoke it came from, never the hub: {delivered}"
    );
    assert_eq!(delivered["replyable"], true, "{delivered}");
    assert_eq!(delivered["intent"], "needs_reply", "{delivered}");

    // The reply: nodec cannot place alice either, so it goes up nodeb's
    // relay into nodec, and nodeb delivers it down to nodea.
    let replied = carol.call_tool(
        "flock_msg_reply",
        json!({"correlation_id": "c-410-hub", "body": "pong from spoke c"}),
    );
    assert_eq!(replied["path"], "via nodeb", "reply: {replied}");

    let answer = wait_for("the reply to reach nodea", RPC_TIMEOUT, || {
        let inbox = alice.call_tool("flock_msg_read", json!({}));
        inbox["messages"].as_array()?.first().cloned()
    });
    assert_eq!(answer["body"], "pong from spoke c");
    assert_eq!(answer["from_agent"], carol.agent_id.as_str(), "{answer}");
    assert_eq!(answer["from_host"], "nodec", "{answer}");
    assert_eq!(answer["in_reply_to"], "c-410-hub", "{answer}");

    // The sender's own log says how it went, too.
    let status: Value =
        serde_json::from_str(&node_a.api(
            r#"{"id":"t:status","method":"msg.status","params":{"correlation_id":"c-410-hub"}}"#,
        ))
        .expect("msg.status parses");
    assert_eq!(status["result"]["state"], "relayed", "{status}");
    assert_eq!(status["result"]["path"], "via nodeb", "{status}");
    assert_eq!(status["result"]["to_host"], "nodec", "{status}");

    // The hub's forward path is not a socket method: a local process on the
    // hub — this test, a foreign pid — cannot name a spoke and have the hub
    // vouch for a sender it never saw. Refused, and nothing is relayed.
    let node_b = fleet.node("nodeb");
    let forged = serde_json::json!({
        "id": "t:forge",
        "method": "msg.uplink_forward",
        "params": {
            "spoke": "nodea",
            "message": {
                "to": {"type": "agent", "agent": carol.agent_id},
                "body": "forged via the hub",
                "from_agent": alice.agent_id,
                "from_host": "nodea",
                "correlation_id": "c-410-forged",
            },
        },
    });
    let refused: Value =
        serde_json::from_str(&node_b.api(&forged.to_string())).expect("the hub answers");
    assert!(
        refused.get("error").is_some(),
        "a socket caller must not reach the forward path: {refused}"
    );

    // Nor can a plain, unattested socket `msg.send` on the hub borrow a
    // spoke's name: the relay stamps the hub's own host on it.
    let unattested = serde_json::json!({
        "id": "t:unattested",
        "method": "msg.send",
        "params": {
            "to": {"type": "agent", "agent": carol.agent_id},
            "body": "from a shell on the hub",
            "from_agent": alice.agent_id,
            "from_host": "nodea",
            "correlation_id": "c-410-unattested",
        },
    });
    let sent: Value =
        serde_json::from_str(&node_b.api(&unattested.to_string())).expect("the hub answers");
    assert_eq!(sent["result"]["path"], "direct", "{sent}");
    let landed = wait_for("the hub's own send to land on nodec", RPC_TIMEOUT, || {
        let inbox = carol.call_tool("flock_msg_read", json!({}));
        inbox["messages"].as_array()?.first().cloned()
    });
    assert_eq!(landed["body"], "from a shell on the hub", "{landed}");
    assert_eq!(
        landed["from_host"], "nodeb",
        "a caller's asserted from_host is never what a relay stamps: {landed}"
    );
    let inbox = carol.call_tool("flock_msg_read", json!({}));
    assert!(
        !inbox.to_string().contains("forged via the hub"),
        "the forged forward was never relayed: {inbox}"
    );

    // Break the hub's edge to nodec. The failure is nodeb's hop, and the
    // sender on nodea is told so — which machine could not reach which, and
    // why — not a generic "not in [[peers]]".
    fleet.refuse_ssh_to("nodec");
    let error = alice.call_tool_error(
        "flock_msg_send",
        json!({
            "to": {"type": "agent", "agent": carol.agent_id},
            "body": "this one cannot land",
            "correlation_id": "c-410-broken",
            "intent": "fyi",
        }),
    );
    let text = error.to_string();
    assert!(
        text.contains("nodeb cannot reach nodec"),
        "the failure names the hop that broke: {text}"
    );
    assert!(
        text.contains("connection refused"),
        "and why it broke: {text}"
    );
    assert!(
        !text.contains("[[peers]]"),
        "never the generic peers refusal: {text}"
    );
}

/// ADR-0018 §3 over #410's hub: a mute on one spoke answers a sender on
/// ANOTHER spoke, whom it can reach only up the relay the hub holds into it.
///
/// The deferral's sender is the muted agent, attested by its own server from
/// its own pane table and carried into the uplink frame in-process — the
/// triggering API call is the original sender's (or the hub's relay), so
/// nothing a caller put on the wire can name it. The hub then stamps the host
/// from the edge it configured, exactly as for any handed-up message.
#[test]
fn a_mute_answers_a_sender_on_another_spoke_through_the_hub() {
    let fleet = fleet::spawn("mcp-hub-mute", HUB_SPOKES);
    let node_a = fleet.node("nodea");
    let mut alice = PanedMcp::start(node_a, &fleet.base);
    let mut carol = PanedMcp::start(fleet.node("nodec"), &fleet.base);

    // Converge the way the #410 case does: the same correlation id on every
    // attempt, so a retry cannot deliver twice.
    let carol_id = carol.agent_id.clone();
    let ask = |cid: &str| {
        json!({
            "to": {"type": "agent", "agent": carol_id},
            "body": format!("question {cid}"),
            "correlation_id": cid,
            "intent": "needs_reply",
        })
    };
    let sent = wait_for("the hub to carry a→c", GOSSIP_TIMEOUT, || {
        alice
            .try_call_tool("flock_msg_send", ask("c-hub-before"))
            .ok()
    });
    assert_eq!(sent["path"], "via nodeb", "{sent}");
    wait_for("the question to land on nodec", RPC_TIMEOUT, || {
        let queued = carol.call_tool("flock_msg_list", json!({"pane": carol.pane_id}));
        (!queued["messages"].as_array()?.is_empty()).then_some(())
    });

    // 1. Waiting before the mute.
    let muted = carol.call_tool(
        "flock_msg_mute",
        json!({"seconds": 600, "reason": "reviewing a spike"}),
    );
    assert_eq!(muted["deferred"], 1, "{muted}");

    let wake_count = |node: &fleet::Node, pane: &str| -> u64 {
        let response: Value = serde_json::from_str(&node.api(&format!(
            r#"{{"id":"t:wake","method":"msg.wake","params":{{"pane":"{pane}"}}}}"#
        )))
        .expect("msg.wake parses");
        response["result"]["count"].as_u64().expect("count")
    };
    let pending = wait_for(
        "the deferral to reach nodea via the hub",
        RPC_TIMEOUT,
        || {
            let queued = alice.call_tool("flock_msg_list", json!({"pane": alice.pane_id}));
            let messages = queued["messages"].as_array()?.clone();
            (!messages.is_empty()).then_some(messages)
        },
    );
    assert_eq!(pending.len(), 1, "exactly one deferral: {pending:?}");
    assert_eq!(
        wake_count(node_a, &alice.pane_id),
        0,
        "a deferral must not cost its receiver a turn"
    );
    let inbox = alice.call_tool("flock_msg_read", json!({}));
    let deferral = &inbox["messages"][0];
    assert_eq!(deferral["in_reply_to"], "c-hub-before", "{deferral}");
    assert_eq!(
        deferral["correlation_id"], "c-hub-before:deferred",
        "{deferral}"
    );
    assert_eq!(deferral["intent"], "fyi", "{deferral}");
    assert_eq!(
        deferral["from_agent"],
        carol.agent_id.as_str(),
        "the muted agent speaks, not the hub and not the asker: {deferral}"
    );
    assert_eq!(
        deferral["from_host"], "nodec",
        "the host is the hub's configured edge to the spoke: {deferral}"
    );
    assert!(
        deferral["body"]
            .as_str()
            .is_some_and(|body| body.contains("reviewing a spike")),
        "{deferral}"
    );

    // 2. Arriving into the mute: the hub's relay into nodec is what delivers
    // the question, and the deferral goes back up the same hub.
    alice.call_tool("flock_msg_send", ask("c-hub-during"));
    let deferral = wait_for("the arrival-time deferral", RPC_TIMEOUT, || {
        let inbox = alice.call_tool("flock_msg_read", json!({}));
        inbox["messages"].as_array()?.first().cloned()
    });
    assert_eq!(deferral["in_reply_to"], "c-hub-during", "{deferral}");
    assert_eq!(
        deferral["from_agent"],
        carol.agent_id.as_str(),
        "{deferral}"
    );

    // 3. Renewing answers nobody twice.
    let renewed = carol.call_tool("flock_msg_mute", json!({"seconds": 900}));
    assert_eq!(renewed["deferred"], 0, "{renewed}");
    thread::sleep(Duration::from_secs(2));
    let inbox = alice.call_tool("flock_msg_read", json!({}));
    assert_eq!(
        inbox["messages"].as_array().map(Vec::len),
        Some(0),
        "no second deferral: {inbox}"
    );
}
