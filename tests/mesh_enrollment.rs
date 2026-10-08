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
        "upgrade flk on acceptor.test",
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
