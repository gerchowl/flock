//! Delegate submission output identifies persisted start metadata honestly.
mod support;

use std::fs;
use support::environment::Command;

fn peer(request: &serde_json::Value) -> serde_json::Value {
    match request["method"].as_str().unwrap() {
        "ping" => {
            serde_json::json!({"result":{"type":"pong", "version":"fixture", "protocol":support::PROTOCOL_VERSION}})
        }
        "agent.get" => serde_json::json!({"result":{"agent":{
            "terminal_id":"term_fixture", "pane_id":"w1:p1", "agent_status":"idle",
            "turn_cursor":"term_fixture:0:0:0:i"
        }}}),
        "pane.submit" => serde_json::json!({"result":{"outcome":"accepted"}}),
        other => panic!("unexpected delegate request: {other}"),
    }
}

fn send(round: u64, sha: Option<&str>, json: bool) -> std::process::Output {
    let (socket, _peer) = support::scripted::scripted_server("delegate-start-base", peer);
    let root = socket.parent().unwrap();
    let brief = root.join("brief.md");
    fs::write(&brief, "fixture task").unwrap();
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in socket.to_string_lossy().as_bytes() {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    let registry = root
        .join("state")
        .join(support::paths::app_dir_name())
        .join("delegates")
        .join(format!("{hash:016x}"));
    fs::create_dir_all(&registry).unwrap();
    fs::write(
        registry.join("fixture.json"),
        serde_json::json!({
            "name":"fixture", "terminal_id":"term_fixture", "pane_id":"w1:p1",
            "root_pane":"w1:p0", "workspace_id":"w1", "mode":"worktree", "worktree":root,
            "branch":"fixture", "harness":"opencode", "model":null, "round":round,
            "brief":brief, "submitted_at_ms":0, "cursor":"term_fixture:0:0:0:i", "created_at_ms":0,
            "base":"origin/dev", "base_sha":sha
        })
        .to_string(),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_flk"));
    command
        .args(["delegate", "send", "fixture", "--brief"])
        .arg(&brief)
        .env("FLOCK_SOCKET_PATH", &socket)
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"));
    if json {
        command.arg("--json");
    }
    let output = command.output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let updated: serde_json::Value =
        serde_json::from_slice(&fs::read(registry.join("fixture.json")).unwrap()).unwrap();
    assert_eq!(updated["round"], round + 1);
    assert_eq!(updated["base"], "origin/dev");
    assert_eq!(updated["base_sha"], serde_json::json!(sha));
    output
}

#[test]
fn later_send_labels_historical_start_base() {
    let sha = "1111111111111111111111111111111111111111";
    let output = send(4, Some(sha), false);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("submitted round 5"), "{text}");
    assert!(
        text.contains("  start base origin/dev (111111111111)"),
        "{text}"
    );
    assert!(!text.contains("\n  base "), "{text}");
}

#[test]
fn legacy_start_base_without_sha_is_still_labelled() {
    let output = send(1, None, false);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("  start base origin/dev\n"), "{text}");
}

#[test]
fn send_preserves_start_base_json_fields() {
    let sha = "1111111111111111111111111111111111111111";
    let output = send(4, Some(sha), true);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["base"], "origin/dev");
    assert_eq!(value["base_sha"], sha);
    assert_eq!(value["round"], 5);
    assert!(value.get("start_base").is_none());
}
