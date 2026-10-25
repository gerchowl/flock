//! Real CLI requests use generation-fenced grant handles over an isolated socket.
mod support;
use serde_json::{json, Value};
use support::scripted::{flk, scripted_server};

fn answer(request: &Value) -> Value {
    match request["method"].as_str() {
        Some("structured.grants.list") => {
            let show_input = request["params"]["show_input"].as_bool().unwrap();
            assert_eq!(
                request["params"],
                json!({"agent_id":"fixture-agent","show_input":show_input})
            );
            let key = json!(["Bash", {"command":"SECRET_2416_CLI_PROBE"}]).to_string();
            let mut grant = json!({"request_id":"request-one","request_seq":9,"tool":"Bash","canonical_input_key_hash":"0000000000000000","canonical_input_key_bytes":key.len(),"installed_ms":1234});
            if show_input {
                grant["canonical_input_key"] = json!(key);
            }
            json!({"result":{"type":"structured_grants","agent_id":"fixture-agent","generation":7,"grants":[grant]}})
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
    assert!(!String::from_utf8_lossy(&output.stdout).contains("SECRET_"));
    let shown = flk(
        &socket,
        &[
            "structured",
            "grants",
            "list",
            "fixture-agent",
            "--show-input",
        ],
    );
    assert_eq!(
        shown.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&shown.stderr)
    );
    assert!(String::from_utf8_lossy(&shown.stdout).contains("SECRET_2416_CLI_PROBE"));
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
