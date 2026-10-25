//! Real CLI requests use generation-fenced grant handles over an isolated socket.
mod support;
use serde_json::{json, Value};
use support::scripted::{flk, scripted_server};

fn answer(request: &Value) -> Value {
    match request["method"].as_str() {
        Some("structured.grants.list") => {
            assert_eq!(request["params"], json!({"agent_id":"fixture-agent"}));
            json!({"result":{"type":"structured_grants","agent_id":"fixture-agent","generation":7,"grants":[]}})
        }
        Some("structured.grants.revoke") => {
            assert_eq!(
                request["params"],
                json!({"agent_id":"fixture-agent","generation":7,"request_id":"request-one","request_seq":9})
            );
            json!({"error":{"code":"operator_only","message":"operator required"}})
        }
        _ => json!({}),
    }
}
#[test]
fn grant_cli_lists_and_preserves_the_typed_policy_exit() {
    let (socket, _server) = scripted_server("grants", answer);
    let output = flk(&socket, &["structured", "grants", "list", "fixture-agent"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["result"]["generation"],
        7
    );
    let output = flk(
        &socket,
        &[
            "structured",
            "grants",
            "revoke",
            "fixture-agent",
            "7",
            "request-one",
            "9",
            "--json",
        ],
    );
    assert_eq!(output.status.code(), Some(13));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["error"]["code"],
        "operator_only"
    );
}
