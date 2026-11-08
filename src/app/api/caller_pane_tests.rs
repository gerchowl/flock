use crate::api::schema::Request;
use crate::app::delegation::handler_tests::{app_on, caller_is_an_agent, shutdown};
use crate::app::App;
use serde_json::{json, Value};

fn call(app: &mut App, method: &str, params: Value) -> Value {
    let request: Request = serde_json::from_value(json!({
        "id": "caller-pane-test", "method": method, "params": params,
    }))
    .unwrap();
    serde_json::from_str(&app.handle_api_request(request)).unwrap()
}

fn assert_self_compact_refusal(reply: &Value, reason: &str) {
    assert_eq!(reply["error"]["code"], "caller_pane_unresolved", "{reply}");
    let message = reply["error"]["message"].as_str().expect("error message");
    for expected in [
        "calling pane",
        "attested identity",
        reason,
        "pane: \"<id>\"",
        "--pane <id>",
    ] {
        assert!(
            message.contains(expected),
            "missing {expected:?}: {message}"
        );
    }
    assert!(!message.contains("pane  not found"), "{message}");
}

#[tokio::test]
async fn omitted_self_compact_pane_explains_missing_peer_identity() {
    let mut app = app_on(crate::api::EventHub::default());
    for params in [json!({"abort": true}), json!({"pane": "", "abort": true})] {
        let reply = call(&mut app, "pane.arm_self_compact", params);
        assert_self_compact_refusal(&reply, "socket peer process identity is unavailable");
    }
    shutdown(app);
}

#[tokio::test]
async fn legacy_self_compact_pane_explains_missing_birth_without_granting_identity() {
    let mut app = app_on(crate::api::EventHub::default());
    caller_is_an_agent(&mut app);
    let pane = app.state.workspaces[0].focused_pane_id().unwrap();
    crate::test_support::adopt_unproven_pane_caller(&mut app, pane, false);
    let reply = call(&mut app, "pane.arm_self_compact", json!({"abort": true}));
    let caller = app.caller_pane();
    let empty_claim = app.parse_pane_id_or_peer("", app.current_api_peer_pid);
    shutdown(app);
    assert_self_compact_refusal(&reply, "no process-birth evidence");
    assert!(reply["error"]["message"]
        .as_str()
        .unwrap()
        .contains("pre-3.3 handoff"));
    assert_eq!(caller, None);
    assert_eq!(empty_claim, None);
}

#[tokio::test]
async fn delegate_list_explains_legacy_identity_refusal_and_architect_override() {
    let mut app = app_on(crate::api::EventHub::default());
    caller_is_an_agent(&mut app);
    let pane = app.state.workspaces[0].focused_pane_id().unwrap();
    crate::test_support::adopt_unproven_pane_caller(&mut app, pane, false);
    let reply = call(&mut app, "delegate.list", json!({"architect": "self"}));
    shutdown(app);
    assert_eq!(reply["error"]["code"], "no_caller_pane", "{reply}");
    let message = reply["error"]["message"].as_str().expect("error message");
    for expected in [
        "attested identity",
        "no process-birth evidence",
        "pre-3.3 handoff",
        "--all",
        "--architect <id>",
        "architect: \"<id>\"",
    ] {
        assert!(
            message.contains(expected),
            "missing {expected:?}: {message}"
        );
    }
    assert!(!message.contains("runs in no agent pane"), "{message}");
}

#[tokio::test]
async fn explicit_missing_self_compact_pane_still_names_the_requested_target() {
    let mut app = app_on(crate::api::EventHub::default());
    let reply = call(
        &mut app,
        "pane.arm_self_compact",
        json!({"pane": "missing-pane", "abort": true}),
    );
    shutdown(app);
    assert_eq!(reply["error"]["code"], "pane_not_found", "{reply}");
    assert!(reply["error"]["message"]
        .as_str()
        .unwrap()
        .contains("missing-pane"));
}
