use super::*;
use crate::api::schema::{Method, Request};
use crate::structured::registry::{Record, StructuredRuntime};
use serde_json::{json, Value};

fn app() -> App {
    let (_, rx) = tokio::sync::mpsc::unbounded_channel();
    App::new(
        &crate::config::Config::default(),
        true,
        None,
        rx,
        crate::api::EventHub::default(),
    )
}
fn register(app: &mut App) {
    app.state
        .structured_agents
        .register(Record {
            runtime: StructuredRuntime::Structured,
            agent_id: "structured-fixture".into(),
            name: Some("fixture-agent".into()),
            harness: "claude".into(),
            session_id: "00000000-0000-4000-8000-000000000901".into(),
            cwd: std::env::temp_dir().join("structured-project"),
            model: Some("fixture-model".into()),
        })
        .unwrap();
}
fn request(app: &mut App, method: &str, mut params: Value) -> Value {
    if method == "agent.read" {
        params["source"] = json!("visible");
    }
    let request: Request =
        serde_json::from_value(json!({"id":"test", "method":method, "params":params})).unwrap();
    serde_json::from_str(&app.handle_api_request(request)).unwrap()
}

#[tokio::test]
async fn structured_read_contract_is_additive_and_pane_free() {
    let mut app = app();
    app.state.workspaces = vec![crate::workspace::Workspace::test_new("fixture")];
    app.state.ensure_test_terminals();
    register(&mut app);
    let listed = request(&mut app, "agent.list", json!({}));
    let info = &listed["result"]["structured_agents"][0];
    assert_eq!(info["runtime"], "structured");
    assert_eq!(info["lifecycle"], "offline");
    assert_eq!(info["lease_state"], "absent");
    for field in ["pane_id", "terminal_id", "workspace_id", "tab_id", "view"] {
        assert!(info.get(field).is_none(), "no fabricated {field}");
    }
    assert!(
        listed["result"]["agents"].is_array(),
        "PTY list stays present"
    );
    for target in ["structured-fixture", "fixture-agent"] {
        for method in ["agent.get", "agent.status"] {
            let result = request(&mut app, method, json!({"target":target}));
            assert_eq!(&result["result"]["agent"], info);
        }
    }
    let old: ResponseResult =
        serde_json::from_value(json!({"type":"agent_list", "agents":[], "fleet":[]})).unwrap();
    assert!(
        matches!(old, ResponseResult::AgentList {structured_agents, ..} if structured_agents.is_empty())
    );
    let stop: Method =
        serde_json::from_value(json!({"method":"agent.stop","params":{"target":"fixture-agent"}}))
            .unwrap();
    assert!(stop.needs_current_protocol());
}

#[tokio::test]
async fn structured_stop_is_idempotent_offline_and_unknown_requires_recovery() {
    let mut app = app();
    register(&mut app);
    for _ in 0..2 {
        assert_eq!(
            request(&mut app, "agent.stop", json!({"target":"fixture-agent"}))["result"]["agent"]
                ["lifecycle"],
            "offline"
        );
    }
    app.state
        .structured_agents
        .restore_handoff_unknown(&["structured-fixture".into()]);
    let before = app
        .state
        .structured_agents
        .get("structured-fixture")
        .unwrap()
        .2;
    assert_eq!(
        request(&mut app, "agent.stop", json!({"target":"fixture-agent"}))["error"]["code"],
        "structured_recovery_required"
    );
    assert_eq!(
        app.state
            .structured_agents
            .get("structured-fixture")
            .unwrap()
            .2,
        before
    );
    assert_eq!(
        request(&mut app, "agent.get", json!({"target":"fixture-agent"}))["result"]["agent"]
            ["lifecycle"],
        "unknown"
    );
}

#[tokio::test]
async fn structured_pane_operations_refuse_and_cross_runtime_names_are_ambiguous() {
    let mut app = app();
    app.state.workspaces = vec![crate::workspace::Workspace::test_new("fixture")];
    app.state.ensure_test_terminals();
    register(&mut app);
    assert_eq!(
        request(&mut app, "agent.stop", json!({"target":"missing-agent"}))["error"]["code"],
        "agent_not_found"
    );
    let pane = app.state.terminals.values().next().unwrap().id.to_string();
    assert_eq!(
        request(&mut app, "agent.stop", json!({"target":pane}))["error"]["code"],
        "runtime_unsupported"
    );
    for method in [
        "agent.read",
        "agent.focus",
        "agent.send",
        "agent.hibernate",
        "agent.resume",
        "agent.fork",
        "agent.history",
        "agent.result",
        "agent.rename",
        "agent.restart",
        "agent.lineage",
    ] {
        let result = request(
            &mut app,
            method,
            if method == "agent.restart" {
                json!({"target":"fixture-agent", "reason":"fixture restart"})
            } else {
                json!({"target":"fixture-agent", "text":"fixture message", "name":"renamed-fixture"})
            },
        );
        assert_eq!(
            result["error"]["code"], "runtime_unsupported",
            "{method}: {result}"
        );
    }
    let terminal = app.state.terminals.values_mut().next().unwrap();
    terminal.agent_name = Some("fixture-agent".into());
    for method in ["agent.get", "agent.status", "agent.stop", "agent.read"] {
        let result = request(&mut app, method, json!({"target":"fixture-agent"}));
        assert_eq!(
            result["error"]["code"], "agent_target_ambiguous",
            "{method}: {result}"
        );
    }
    assert!(request(
        &mut app,
        "agent.get",
        json!({"target":"structured-fixture"})
    )["result"]["agent"]
        .is_object());
}

#[test]
fn structured_refusals_have_registered_retry_and_exit_semantics() {
    use crate::cli::exit::{self, Retry};
    assert_eq!(ErrorCode::RuntimeUnsupported.exit(), exit::USAGE);
    assert_eq!(ErrorCode::RuntimeUnsupported.retry(), Retry::Never);
    assert_eq!(ErrorCode::StructuredRecoveryRequired.exit(), exit::FAILURE);
    assert_eq!(
        ErrorCode::StructuredRecoveryRequired.retry(),
        Retry::AfterHuman
    );
}

#[tokio::test]
async fn structured_stop_reaches_the_owned_worker_without_inventing_offline() {
    use crate::structured::workers::tests::{wait, Case};
    let mut app = app();
    let notify = app.render_notify.clone();
    let case = Case::start(
        &mut app.state.structured_agents,
        &mut app.structured_workers,
        902,
        "state-reducer",
        vec![],
    );
    case.ready(
        &mut app.state.structured_agents,
        &mut app.structured_workers,
        &notify,
        std::time::Instant::now(),
    )
    .await;
    let value = request(&mut app, "agent.stop", json!({"target":case.id}));
    assert_eq!(value["result"]["agent"]["lifecycle"], "live");
    wait(&notify, || {
        app.structured_workers.poll(
            &mut app.state.structured_agents,
            std::time::Instant::now(),
            1000,
        );
        app.structured_workers
            .report(&case.id, case.generation)
            .is_some()
    })
    .await;
    assert_eq!(
        request(&mut app, "agent.status", json!({"target":case.id}))["result"]["agent"]
            ["lifecycle"],
        "offline"
    );
    app.structured_workers
        .shutdown(&mut app.state.structured_agents)
        .unwrap();
}

#[tokio::test]
async fn structured_stop_requires_operator_or_owning_lease_identity() {
    let mut app = app();
    register(&mut app);
    let mut lease = app
        .session_leases
        .reserve("other-agent", "other-session", None)
        .unwrap();
    lease.bind_test_peer(std::process::id());
    app.current_api_peer_pid = Some(std::process::id());
    assert_eq!(
        request(
            &mut app,
            "agent.stop",
            json!({"target":"structured-fixture"})
        )["error"]["code"],
        "not_authorized"
    );
    drop(lease);
    let mut own = app
        .session_leases
        .reserve("structured-fixture", "owned-session", None)
        .unwrap();
    own.bind_test_peer(std::process::id());
    assert!(
        request(&mut app, "agent.stop", json!({"target":"self"}))["result"]["agent"].is_object()
    );
    drop(own);
    app.current_api_peer_uid = Some(None);
    app.current_api_peer_pid = None;
    assert_eq!(
        request(
            &mut app,
            "agent.stop",
            json!({"target":"structured-fixture"})
        )["error"]["code"],
        "operator_only"
    );
}
