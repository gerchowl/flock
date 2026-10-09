mod support;

use serde_json::{json, Value};
use std::time::Duration;
use support::fleet::{self, MeshMode, Node, NodeSpec};

const PAIR: &[NodeSpec] = &[
    NodeSpec::new("dialer.test", "mesh-dialer", &["acceptor.test"]),
    NodeSpec::new("acceptor.test", "mesh-acceptor", &[]),
];

fn request(node: &Node, method: &str, params: Value) -> Value {
    serde_json::from_str(
        &node.api(&json!({"id":"probe", "method":method, "params":params}).to_string()),
    )
    .unwrap()
}

fn enrollment(node: &Node, peer: &str, state: &str) -> Value {
    fleet::wait_until("mesh enrollment", Duration::from_secs(90), || {
        let response = request(node, "peers.enrollment", json!({}));
        response["result"]["peers"]
            .as_array()?
            .iter()
            .find(|s| s["peer"] == peer && s["state"] == state)
            .cloned()
    })
}

// Integration scaffolding launches the public CLI outside the product logging funnel.
#[allow(clippy::disallowed_methods)]
fn cli(node: &Node, args: &[&str]) -> Value {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_flk"))
        .args(args)
        .env_clear()
        .env("HOME", &node.home)
        .env("XDG_CONFIG_HOME", &node.config_home)
        .env("XDG_RUNTIME_DIR", &node.runtime_dir)
        .env("XDG_STATE_HOME", node.home.join("state"))
        .env("FLOCK_SOCKET_PATH", &node.api_socket)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn peer_restart_reenrolls_promptly_after_missing_server_hello() {
    let mut fleet = fleet::spawn("mesh-retry", PAIR);
    let pinned = enrollment(fleet.node("dialer.test"), "acceptor.test", "pinned");
    fleet.node_mut("acceptor.test").stop();
    // Wait for a fresh hello to reach the stopped server, not just for the
    // previously enrolled stream to notice that its server disappeared.
    fleet::wait_until("hello to stopped peer", Duration::from_secs(10), || {
        let status = request(fleet.node("dialer.test"), "peers.enrollment", json!({}));
        status["result"]["peers"]
            .as_array()?
            .iter()
            .find(|p| {
                p["peer"] == "acceptor.test"
                    && p["reason"].as_str().is_some_and(|reason| {
                        reason.contains("mesh handshake refused")
                            && reason.contains("no_local_server")
                    })
            })
            .cloned()
    });
    fleet.node_mut("acceptor.test").restart();
    fleet::wait_until("re-enrolled after restart", Duration::from_secs(6), || {
        let status = request(fleet.node("dialer.test"), "peers.enrollment", json!({}));
        status["result"]["peers"]
            .as_array()?
            .iter()
            .find(|p| {
                p["peer"] == "acceptor.test"
                    && p["state"] == "pinned"
                    && p["node_id"] == pinned["node_id"]
            })
            .cloned()
    });
    enrollment(fleet.node("acceptor.test"), "dialer.test", "pinned");
}

#[test]
fn mutual_enrollment_and_restart_keep_pins() {
    let mut fleet = fleet::spawn("mesh-enroll", PAIR);
    let remote = enrollment(fleet.node("dialer.test"), "acceptor.test", "pinned");
    let local = enrollment(fleet.node("acceptor.test"), "dialer.test", "pinned");
    assert_ne!(remote["node_id"], local["node_id"]);
    assert_eq!(remote["node_id"].as_str().unwrap().len(), 64);
    let peers = cli(fleet.node("dialer.test"), &["peers", "status", "--json"]);
    assert!(peers
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["node_id"] == remote["node_id"] && p["enrollment"] == "pinned"));
    let status = cli(fleet.node("dialer.test"), &["status", "--json"]);
    assert_eq!(status["peers"][0]["node_id"], remote["node_id"]);
    fleet.node_mut("acceptor.test").restart();
    fleet.node_mut("dialer.test").restart();
    assert_eq!(
        enrollment(fleet.node("dialer.test"), "acceptor.test", "pinned")["node_id"],
        remote["node_id"]
    );
    assert_eq!(
        enrollment(fleet.node("acceptor.test"), "dialer.test", "pinned")["node_id"],
        local["node_id"]
    );
}

#[test]
fn changed_key_is_refused_until_operator_reset() {
    let mut fleet = fleet::spawn("mesh-rekey", PAIR);
    let old = enrollment(fleet.node("dialer.test"), "acceptor.test", "pinned");
    for app in ["flock", "flock-dev"] {
        let key = fleet
            .node("acceptor.test")
            .home
            .join("state")
            .join(app)
            .join("mesh/identity.json");
        if key.exists() {
            std::fs::remove_file(key).unwrap();
        }
    }
    fleet.node_mut("acceptor.test").restart();
    fleet.node_mut("dialer.test").restart();
    let refused = enrollment(fleet.node("dialer.test"), "acceptor.test", "refused");
    assert!(
        refused["reason"]
            .as_str()
            .unwrap()
            .contains("identity changed"),
        "{refused}"
    );
    let result = cli(
        fleet.node("dialer.test"),
        &["peers", "enroll", "--reset", "acceptor.test"],
    );
    assert!(result.get("error").is_none(), "{result}");
    let new = enrollment(fleet.node("dialer.test"), "acceptor.test", "pinned");
    assert_ne!(new["node_id"], old["node_id"]);
}

fn refused_mode(tag: &str, mode: MeshMode, expected: &str) {
    let mut specs = PAIR.to_vec();
    specs[1].mesh = mode;
    let fleet = fleet::spawn(tag, &specs);
    let status = enrollment(fleet.node("dialer.test"), "acceptor.test", "refused");
    assert!(
        status["reason"].as_str().unwrap().contains(expected),
        "{status}"
    );
    if let MeshMode::VersionMismatch(version) = mode {
        assert!(
            status["reason"]
                .as_str()
                .unwrap()
                .contains(&format!("local 1, remote {version}")),
            "{status}"
        );
    }
    assert!(
        status["node_id"].is_null(),
        "an unauthenticated key must not be published: {status}"
    );
    let peers = cli(fleet.node("dialer.test"), &["peers", "status", "--json"]);
    assert!(peers
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["enrollment"] == "refused"));
}

#[test]
fn version_mismatch_is_refused_with_upgrade_instruction() {
    refused_mode(
        "mesh-version",
        MeshMode::VersionMismatch(99),
        "upgrade flk on this node",
    );
}

#[test]
fn mesh_disabled_peer_is_refused_in_status() {
    refused_mode("mesh-disabled", MeshMode::Disabled, "unknown variant");
}

#[test]
fn forged_signature_is_refused() {
    refused_mode(
        "mesh-forged",
        MeshMode::ForgedSignature,
        "invalid mesh signature",
    );
}

#[test]
fn custom_summary_transport_is_refused() {
    let mut specs = PAIR.to_vec();
    specs[0].extra_config = "summary_command = 'false'\n";
    let fleet = fleet::spawn("mesh-custom", &specs);
    let status = enrollment(fleet.node("dialer.test"), "acceptor.test", "refused");
    assert!(
        status["reason"]
            .as_str()
            .unwrap()
            .contains("custom summary transport unsupported"),
        "{status}"
    );
}

#[test]
fn forged_acceptor_challenge_is_refused() {
    refused_mode(
        "mesh-challenge",
        MeshMode::ForgedChallenge,
        "invalid mesh signature",
    );
}

fn remove_identity(node: &Node) {
    for app in ["flock", "flock-dev"] {
        let key = node.home.join("state").join(app).join("mesh/identity.json");
        if key.exists() {
            std::fs::remove_file(key).unwrap();
        }
    }
}

#[test]
fn acceptor_refuses_changed_dialer_key_until_inbound_reset() {
    let mut fleet = fleet::spawn("mesh-dialer-key", PAIR);
    let old = enrollment(fleet.node("acceptor.test"), "dialer.test", "pinned");
    enrollment(fleet.node("dialer.test"), "acceptor.test", "pinned");
    remove_identity(fleet.node("dialer.test"));
    fleet.node_mut("dialer.test").restart();
    let refused = enrollment(fleet.node("acceptor.test"), "dialer.test", "refused");
    assert!(
        refused["reason"]
            .as_str()
            .unwrap()
            .contains("--direction inbound"),
        "{refused}"
    );
    let reset = cli(
        fleet.node("acceptor.test"),
        &["peers", "enroll", "--reset", "dialer.test"],
    );
    assert!(reset["result"]["node_id"].is_null(), "{reset}");
    assert_eq!(
        enrollment(fleet.node("acceptor.test"), "dialer.test", "refused")["state"],
        "refused"
    );
    let reset = cli(
        fleet.node("acceptor.test"),
        &[
            "peers",
            "enroll",
            "--reset",
            "dialer.test",
            "--direction",
            "inbound",
        ],
    );
    assert_eq!(reset["result"]["node_id"], old["node_id"]);
    // A fresh dial avoids waiting for the deliberately backed-off refused edge.
    fleet.node_mut("dialer.test").restart();
    assert_ne!(
        enrollment(fleet.node("acceptor.test"), "dialer.test", "pinned")["node_id"],
        old["node_id"]
    );
}

#[test]
fn enrolled_dialer_cannot_reconnect_under_an_unused_name() {
    let mut fleet = fleet::spawn("mesh-rename", PAIR);
    let pinned = enrollment(fleet.node("acceptor.test"), "dialer.test", "pinned");
    enrollment(fleet.node("dialer.test"), "acceptor.test", "pinned");
    rename_node(fleet.node("dialer.test"), "impostor.test");
    fleet.node_mut("dialer.test").restart();
    let refused = enrollment(fleet.node("acceptor.test"), "impostor.test", "refused");
    assert_eq!(
        refused["reason"],
        format!(
            "node {} is enrolled as dialer.test",
            pinned["node_id"].as_str().unwrap()
        )
    );
    assert!(refused["node_id"].is_null());
    let preview = request(
        fleet.node("acceptor.test"),
        "peers.enroll_reset",
        json!({"peer":"impostor.test", "source":"inbound", "preview":true}),
    );
    assert!(preview["result"]["node_id"].is_null(), "{preview}");
    rename_node(fleet.node("dialer.test"), "dialer.test");
    fleet.node_mut("dialer.test").restart();
    assert_eq!(
        enrollment(fleet.node("acceptor.test"), "dialer.test", "pinned")["node_id"],
        pinned["node_id"]
    );
}

#[test]
fn authenticated_relay_cannot_reset_enrollment() {
    let mut specs = PAIR.to_vec();
    specs[1].mesh = MeshMode::RelayReset;
    let fleet = fleet::spawn("mesh-reset-relay", &specs);
    let pinned = enrollment(fleet.node("acceptor.test"), "dialer.test", "pinned");
    let refused = fleet::wait_until("relay reset refusal", Duration::from_secs(30), || {
        std::fs::read_to_string(fleet.base.join("reset-refused-acceptor.test")).ok()
    });
    let refused: Value = serde_json::from_str(&refused).unwrap();
    assert_eq!(refused["error"]["code"], "operator_only");
    let preview = request(
        fleet.node("acceptor.test"),
        "peers.enroll_reset",
        json!({"peer":"dialer.test", "source":"inbound", "preview":true}),
    );
    assert_eq!(preview["result"]["node_id"], pinned["node_id"]);
}

// This subprocess exercises the public MCP stdio boundary.
#[allow(clippy::disallowed_methods)]
#[test]
fn mcp_cannot_reset_enrollment() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let fleet = fleet::spawn("mesh-reset-mcp", PAIR);
    let node = fleet.node("dialer.test");
    let pinned = enrollment(node, "acceptor.test", "pinned");
    let mut child = Command::new(env!("CARGO_BIN_EXE_flk"))
        .args(["mcp", "serve"])
        .env_clear()
        .env("HOME", &node.home)
        .env("XDG_CONFIG_HOME", &node.config_home)
        .env("XDG_RUNTIME_DIR", &node.runtime_dir)
        .env("XDG_STATE_HOME", node.home.join("state"))
        .env("FLOCK_SOCKET_PATH", &node.api_socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for message in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"mesh-test","version":"1"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"flock_peers_enroll_reset","arguments":{"peer":"acceptor.test"}}}),
    ] {
        writeln!(stdin, "{message}").unwrap();
    }
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    let responses: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let tools = responses.iter().find(|v| v["id"] == 2).unwrap();
    assert!(!tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"].as_str().unwrap().contains("enroll_reset")));
    let reset = responses.iter().find(|v| v["id"] == 3).unwrap();
    assert!(
        reset.get("error").is_some() || reset["result"]["isError"] == true,
        "{reset}"
    );
    let preview = request(
        node,
        "peers.enroll_reset",
        json!({"peer":"acceptor.test","preview":true}),
    );
    assert_eq!(preview["result"]["node_id"], pinned["node_id"]);
}

// The old-server fixture implements discovery but deliberately rejects enrollment.
#[allow(clippy::disallowed_methods)]
#[test]
fn old_server_keeps_status_output_when_enrollment_method_is_missing() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    let dir = std::env::temp_dir().join(format!("ml-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.as_path().join("api.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_server = stop.clone();
    let worker = std::thread::spawn(move || {
        while !stop_server.load(Ordering::Relaxed) {
            let (mut stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("{error}"),
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let mut response = match request["method"].as_str().unwrap() {
                "ping" => json!({"result":{"type":"pong","version":"0.10.0","protocol":26}}),
                "peers.summary" => {
                    json!({"result":{"relayed_fleet":[{"name":"legacy.test","ssh_target":"legacy.test","origin":"local.test","latency_ms":7,"error":null}]}})
                }
                "peers.enrollment" => {
                    json!({"error":{"code":"method_not_found","message":"unknown method"}})
                }
                method => panic!("unexpected method {method}"),
            };
            response["id"] = request["id"].clone();
            writeln!(stream, "{response}").unwrap();
        }
    });
    let mut outputs = Vec::new();
    for args in [
        vec!["status"],
        vec!["status", "--json"],
        vec!["peers", "status"],
        vec!["peers", "status", "--json"],
    ] {
        outputs.push(
            std::process::Command::new(env!("CARGO_BIN_EXE_flk"))
                .args(args)
                .env_clear()
                .env("HOME", dir.as_path())
                .env("XDG_CONFIG_HOME", dir.as_path().join("config"))
                .env("XDG_STATE_HOME", dir.as_path().join("state"))
                .env("XDG_RUNTIME_DIR", dir.as_path().join("runtime"))
                .env("FLOCK_SOCKET_PATH", &socket)
                .output()
                .unwrap(),
        );
    }
    stop.store(true, Ordering::Relaxed);
    worker.join().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    for output in &outputs {
        assert!(output.status.success(), "{output:?}");
    }
    let warning = "enrollment: unknown (server predates mesh; restart needed)";
    let full = String::from_utf8_lossy(&outputs[0].stdout);
    assert!(full.starts_with("client:"), "{full}");
    assert!(
        full.contains("peers:\n") && full.contains(warning) && full.contains("server:\n"),
        "{full}"
    );
    let full: Value = serde_json::from_slice(&outputs[1].stdout).unwrap();
    assert_eq!(full["enrollment_warning"], warning);
    assert_eq!(full["server"]["version"], "0.10.0");
    let peers = String::from_utf8_lossy(&outputs[2].stdout);
    assert!(
        peers.contains("legacy.test") && peers.contains("7ms") && peers.contains(warning),
        "{peers}"
    );
    let peers: Value = serde_json::from_slice(&outputs[3].stdout).unwrap();
    assert_eq!(peers[0]["name"], "legacy.test");
    assert_eq!(peers[0]["enrollment"], "unknown");
}

fn rename_node(node: &Node, name: &str) {
    for app in ["flock", "flock-dev"] {
        let path = node.config_home.join(app).join("config.toml");
        let config = std::fs::read_to_string(&path).unwrap();
        let original = config
            .lines()
            .find(|line| line.starts_with("name = "))
            .unwrap();
        let config = config.replacen(original, &format!("name = {name:?}"), 1);
        std::fs::write(path, config).unwrap();
    }
}

#[test]
fn old_dialer_is_visible_as_refused_on_acceptor() {
    let mut specs = PAIR.to_vec();
    specs[1].mesh = MeshMode::LegacyDialer;
    let fleet = fleet::spawn("mesh-old-dialer", &specs);
    let refused = enrollment(
        fleet.node("acceptor.test"),
        "unidentified SSH peer",
        "refused",
    );
    assert!(
        refused["reason"]
            .as_str()
            .unwrap()
            .contains("upgrade flk on the dialer"),
        "{refused}"
    );
    assert!(refused["node_id"].is_null());
}

const ALIAS_A_TO_B: &str = "[[peers]]\nname = \"b-ts.test\"\nssh = \"b.test\"\n";
const ALIAS_B_TO_A: &str = "[[peers]]\nname = \"a-ts.test\"\nssh = \"a.test\"\n";

fn add_peer(node: &Node, config: &str) {
    use std::io::Write;
    for app in ["flock", "flock-dev"] {
        let path = node.config_home.join(app).join("config.toml");
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "\n{config}").unwrap();
    }
}

fn bidirectional_aliases(a_dials_first: bool) {
    let specs = [
        NodeSpec::new("a.test", "alias-a", &[]).with_config(if a_dials_first {
            ALIAS_A_TO_B
        } else {
            ""
        }),
        NodeSpec::new("b.test", "alias-b", &[]).with_config(if a_dials_first {
            ""
        } else {
            ALIAS_B_TO_A
        }),
    ];
    let mut fleet = fleet::spawn("mesh-alias", &specs);
    if a_dials_first {
        enrollment(fleet.node("a.test"), "b-ts.test", "pinned");
        enrollment(fleet.node("b.test"), "a.test", "pinned");
        add_peer(fleet.node("b.test"), ALIAS_B_TO_A);
        let reloaded = request(fleet.node("b.test"), "server.reload_config", json!({}));
        assert!(reloaded.get("error").is_none(), "{reloaded}");
    } else {
        enrollment(fleet.node("b.test"), "a-ts.test", "pinned");
        enrollment(fleet.node("a.test"), "b.test", "pinned");
        add_peer(fleet.node("a.test"), ALIAS_A_TO_B);
        let reloaded = request(fleet.node("a.test"), "server.reload_config", json!({}));
        assert!(reloaded.get("error").is_none(), "{reloaded}");
    }
    for (name, alias) in [("a.test", "b-ts.test"), ("b.test", "a-ts.test")] {
        let node = fleet.node(name);
        let peers = fleet::wait_until(
            "both alias directions enrolled",
            Duration::from_secs(90),
            || {
                let response = request(node, "peers.enrollment", json!({}));
                let peers = response["result"]["peers"].as_array()?;
                (peers.len() == 2
                    && peers
                        .iter()
                        .all(|p| p["peer"] == alias && p["state"] == "pinned"))
                .then(|| peers.clone())
            },
        );
        assert_eq!(peers[0]["node_id"], peers[1]["node_id"]);
        assert!(peers.iter().any(|p| p["source"] == "configured"));
        assert!(peers.iter().any(|p| p["source"] == "inbound"));
        let status = cli(node, &["status", "--json"]);
        assert!(status["peers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["peer"] == alias));
        let status = cli(node, &["peers", "status", "--json"]);
        assert!(status
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["name"] == alias));
        let preview = request(
            node,
            "peers.enroll_reset",
            json!({"peer":alias,"source":"inbound","preview":true}),
        );
        assert_eq!(
            preview["result"]["node_id"], peers[0]["node_id"],
            "{preview}"
        );
    }
    // An existing configured alias must not authorize a new inbound name.
    rename_node(fleet.node("b.test"), "impostor.test");
    fleet.node_mut("b.test").restart();
    let refused = enrollment(fleet.node("a.test"), "b-ts.test", "refused");
    assert!(
        refused["reason"]
            .as_str()
            .unwrap()
            .contains("is enrolled as b.test"),
        "{refused}"
    );
}

#[test]
fn bidirectional_alias_enrollment_outbound_first() {
    bidirectional_aliases(true);
}

#[test]
fn bidirectional_alias_enrollment_inbound_first() {
    bidirectional_aliases(false);
}

#[test]
fn configured_name_first_contact_is_tofu_but_later_impersonation_is_refused() {
    for pinned_first in [false, true] {
        let specs = [
            NodeSpec::new("a.test", "reserved-a", &[]),
            NodeSpec::new("b.test", "reserved-b", &[]),
            NodeSpec::new("e.test", "reserved-e", &[]),
        ];
        let mut fleet = fleet::spawn("mesh-reserved", &specs);
        if !pinned_first {
            fleet.refuse_edge("a.test", "b.test");
        }
        add_peer(fleet.node("a.test"), ALIAS_A_TO_B);
        let response = request(fleet.node("a.test"), "server.reload_config", json!({}));
        assert!(response.get("error").is_none(), "{response}");
        let legitimate =
            pinned_first.then(|| enrollment(fleet.node("a.test"), "b-ts.test", "pinned"));
        rename_node(fleet.node("e.test"), "b-ts.test");
        add_peer(
            fleet.node("e.test"),
            "[[peers]]\nname = \"a.test\"\nssh = \"a.test\"\n",
        );
        fleet.node_mut("e.test").restart();
        let attacker_id = identity_id(fleet.node("e.test"));
        if let Some(pinned) = legitimate {
            let refused = enrollment(fleet.node("e.test"), "a.test", "refused");
            let reason = refused["reason"].as_str().unwrap();
            assert!(
                reason.contains("impersonation of configured peer b-ts.test"),
                "{refused}"
            );
            assert!(
                reason.contains(pinned["node_id"].as_str().unwrap()),
                "{refused}"
            );
            assert!(reason.contains(&attacker_id), "{refused}");
            assert_single_configured_row(fleet.node("a.test"));
            let inbound = enrollment(fleet.node("a.test"), "unidentified SSH peer", "refused");
            assert!(inbound["node_id"].is_null());
        } else {
            // A caller with SSH access can win the first-contact trust decision.
            enrollment(fleet.node("e.test"), "a.test", "pinned");
            let inbound = enrollment(fleet.node("a.test"), "b-ts.test", "pinned");
            assert_eq!(inbound["node_id"], attacker_id);
            for source in ["configured", "inbound"] {
                let preview = request(
                    fleet.node("a.test"),
                    "peers.enroll_reset",
                    json!({"peer":"b-ts.test","source":source,"preview":true}),
                );
                assert_eq!(preview["result"]["node_id"], attacker_id, "{preview}");
            }
            fleet.allow_edge("a.test", "b.test");
            fleet.node_mut("a.test").restart();
            let refused = enrollment(fleet.node("a.test"), "b-ts.test", "refused");
            assert!(
                refused["reason"]
                    .as_str()
                    .unwrap()
                    .contains("identity changed"),
                "{refused}"
            );
        }
    }
}

fn identity_id(node: &Node) -> String {
    // Derive the identity independently of the node's claimed name.
    let e_id = ["flock", "flock-dev"]
        .into_iter()
        .find_map(|app| {
            let path = node.home.join("state").join(app).join("mesh/identity.json");
            std::fs::read_to_string(path).ok()
        })
        .unwrap();
    let e_id: Value = serde_json::from_str(&e_id).unwrap();
    use sha2::{Digest, Sha256};
    let public_key: Vec<u8> = serde_json::from_value(e_id["public_key"].clone()).unwrap();
    Sha256::digest(public_key)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn fresh_exact_name_peers_enroll_simultaneously() {
    let specs = [
        NodeSpec::new("a.test", "simultaneous-a", &["b.test"]),
        NodeSpec::new("b.test", "simultaneous-b", &["a.test"]),
    ];
    let fleet = fleet::spawn("mesh-simultaneous", &specs);
    for (local, remote) in [("a.test", "b.test"), ("b.test", "a.test")] {
        let expected = identity_id(fleet.node(remote));
        fleet::wait_until(
            "both exact-name directions enrolled",
            Duration::from_secs(90),
            || {
                let response = request(fleet.node(local), "peers.enrollment", json!({}));
                let peers = response["result"]["peers"].as_array()?;
                (peers.len() == 2
                    && peers.iter().all(|p| {
                        p["peer"] == remote && p["state"] == "pinned" && p["node_id"] == expected
                    }))
                .then_some(())
            },
        );
        for source in ["configured", "inbound"] {
            let preview = request(
                fleet.node(local),
                "peers.enroll_reset",
                json!({"peer":remote,"source":source,"preview":true}),
            );
            assert_eq!(preview["result"]["node_id"], expected, "{preview}");
        }
    }
}

fn assert_single_configured_row(node: &Node) {
    let preview = request(
        node,
        "peers.enroll_reset",
        json!({"peer":"b-ts.test","source":"inbound","preview":true}),
    );
    assert!(preview["result"]["node_id"].is_null(), "{preview}");
    let status = cli(node, &["status", "--json"]);
    let peers = status["peers"].as_array().unwrap();
    let named: Vec<_> = peers.iter().filter(|p| p["peer"] == "b-ts.test").collect();
    assert_eq!(named.len(), 1, "{status}");
    assert_eq!(named[0]["source"], "configured", "{status}");
    let peer_status = cli(node, &["peers", "status", "--json"]);
    assert_eq!(
        peer_status
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["name"] == "b-ts.test")
            .count(),
        1,
        "{peer_status}"
    );
    assert!(
        !peers
            .iter()
            .any(|p| p["source"] == "inbound" && p["state"] == "pinned"),
        "{status}"
    );
}
