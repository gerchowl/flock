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
    terminal.set_agent_name("fixture-agent".into()).unwrap();
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

#[tokio::test]
async fn missing_structured_metadata_is_a_typed_internal_error_not_a_panic() {
    assert_eq!(
        app().structured_info("missing").unwrap_err(),
        ErrorCode::InternalError
    );
}

#[test]
fn stop_authority_excludes_pty_agents_and_lineage_parents() {
    let pane = crate::workspace::Workspace::test_new("fixture").tabs[0]
        .layout
        .pane_ids()[0];
    let target = "structured-fixture";
    assert!(!may_stop(
        SpawnCaller::Agent {
            ws_idx: 0,
            pane_id: pane,
            agent_id: target.into()
        },
        target
    ));
    assert!(!may_stop(
        SpawnCaller::Structured {
            agent_id: Some("parent-agent".into()),
            depth: 0
        },
        target
    ));
    assert!(!may_stop(
        SpawnCaller::Structured {
            agent_id: None,
            depth: 0
        },
        target
    ));
    assert!(may_stop(
        SpawnCaller::Structured {
            agent_id: Some(target.into()),
            depth: 0
        },
        target
    ));
    assert!(may_stop(SpawnCaller::Operator { pane: None }, target));
}

/// The narrowed scoped wire is fresh-only; Unknown metadata remains fenced.
#[tokio::test]
async fn scoped_structured_spawn_never_resumes_an_existing_identity() {
    for unknown in [false, true] {
        scoped_spawn_refusal(
            unknown,
            "fixture-agent",
            false,
            if unknown {
                "structured_recovery_required"
            } else {
                "agent_name_taken"
            },
        )
        .await;
    }
}

#[tokio::test]
async fn scoped_structured_spawn_refuses_unattended() {
    scoped_spawn_refusal(false, "fresh-agent", true, "runtime_unsupported").await;
}

#[tokio::test]
async fn scoped_structured_spawn_reserves_self() {
    scoped_spawn_refusal(false, "self", false, "invalid_agent_name").await;
}

async fn scoped_spawn_refusal(unknown: bool, name: &str, unattended: bool, expected: &str) {
    let (leases, _lease, scope, peer) = crate::api::scoped::tests::live_scope();
    let mut app = app();
    app.session_leases = leases;
    app.runtime_config.uid_policy = crate::structured::identity::UidPolicy {
        operator: peer.uid.unwrap(),
        dedicated: Ok(None).into(),
    };
    app.state.config.fleet.agent_spawn_enabled = true;
    app.state.workspaces = vec![crate::workspace::Workspace::test_new("scoped-fresh")];
    app.state.ensure_test_terminals();
    register(&mut app);
    if unknown {
        app.state
            .structured_agents
            .restore_handoff_unknown(&["structured-fixture".into()]);
    }
    let generation = app
        .state
        .structured_agents
        .get("structured-fixture")
        .unwrap()
        .2;
    let workspace = app.public_workspace_id(0);
    let request: Request =
            serde_json::from_value(json!({"id":"scoped-fresh", "method":"agent.spawn",
            "params":{"runtime":"structured", "agent":"claude", "name":name, "unattended":unattended,
            "prompt":"fresh task", "location":{"kind":"workspace_id","workspace_id":workspace}}}))
            .unwrap();
    let (respond_to, response) = std::sync::mpsc::channel();
    app.handle_api_request_message(crate::api::ApiRequestMessage {
        request,
        respond_to,
        peer_pid: peer.pid,
        peer_birth: peer.birth,
        peer_uid: peer.uid,
        scope: Some(scope),
    });
    let result: Value = serde_json::from_str(
        &response
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["error"]["code"], expected);
    assert_eq!(
        app.state
            .structured_agents
            .get("structured-fixture")
            .unwrap()
            .2,
        generation
    );
    assert_eq!(app.state.structured_agents.records().len(), 1);
}

/// Existing history is operator-only, including self and unresolved agent callers.
#[tokio::test]
async fn structured_resume_refuses_every_agent_before_admission() {
    for identity in [Some("other-agent"), Some("structured-fixture"), None] {
        let mut app = app();
        register(&mut app);
        let params = serde_json::from_value(
            json!({"name":"fixture-agent", "runtime":"structured", "argv":["claude"]}),
        )
        .unwrap();
        let caller = SpawnCaller::Structured {
            agent_id: identity.map(str::to_owned),
            depth: 0,
        };
        let error = app.start_structured(params, &caller).unwrap_err();
        assert_eq!(error, ErrorCode::OperatorOnly);
        assert_eq!(
            app.state
                .structured_agents
                .get("structured-fixture")
                .unwrap()
                .2,
            1
        );
    }
    let mut app = app();
    register(&mut app);
    let params = serde_json::from_value(
        json!({"name":"fixture-agent", "runtime":"structured", "argv":["claude"]}),
    )
    .unwrap();
    let caller = SpawnCaller::Agent {
        ws_idx: 0,
        pane_id: crate::workspace::Workspace::test_new("resume").tabs[0]
            .layout
            .pane_ids()[0],
        agent_id: "structured-fixture".into(),
    };
    assert_eq!(
        app.start_structured(params, &caller).unwrap_err(),
        ErrorCode::OperatorOnly
    );
}

/// The server boundary also refuses a relay bypassing the CLI transport filter.
#[tokio::test]
async fn structured_resume_refuses_a_recorded_mesh_edge() {
    let mut app = app();
    register(&mut app);
    let pid = std::process::id();
    let birth = crate::platform::process_start_time(pid).unwrap();
    app.inbound
        .attach(pid, birth, crate::platform::process_start_time)
        .unwrap();
    app.current_api_peer_pid = Some(pid);
    let params = serde_json::from_value(
        json!({"name":"fixture-agent", "runtime":"structured", "argv":["claude"]}),
    )
    .unwrap();
    assert_eq!(
        app.start_structured(params, &SpawnCaller::Operator { pane: None })
            .unwrap_err(),
        ErrorCode::OperatorOnly
    );
}

/// Missing ancestry cannot promote a dedicated, unknown or unreadable UID.
#[tokio::test]
async fn structured_resume_refuses_nonoperator_uid_without_ancestry() {
    for uid in [None, Some(42), Some(43)] {
        let mut app = app();
        register(&mut app);
        app.current_api_peer_uid = Some(uid);
        app.runtime_config.uid_policy = crate::structured::identity::UidPolicy {
            operator: 41,
            dedicated: Ok(Some(42)).into(),
        };
        let params = serde_json::from_value(
            json!({"name":"fixture-agent", "runtime":"structured", "argv":["claude"]}),
        )
        .unwrap();
        assert_eq!(
            app.start_structured(params, &SpawnCaller::Operator { pane: None })
                .unwrap_err(),
            ErrorCode::OperatorOnly
        );
    }
}

#[tokio::test]
async fn grant_socket_methods_fence_generation_authority_and_emit_revocation_audit() {
    use crate::structured::{
        permissions::{self, AgentDecide, Decision, Principal},
        workers::tests::{wait, Case},
    };
    let mut app = app();
    app.current_api_peer_pid = Some(std::process::id());
    let notify = app.render_notify.clone();
    let event = json!({"type":"control_request","request_id":"grant-origin","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"fixture-tool","input":{"command":"true"}}});
    let case = Case::start(
        &mut app.state.structured_agents,
        &mut app.structured_workers,
        2362,
        "state-reducer",
        vec![event],
    );
    case.human(
        &mut app.state.structured_agents,
        &mut app.structured_workers,
        &notify,
        std::time::Instant::now(),
    )
    .await;
    let seq = app
        .state
        .structured_agents
        .get(&case.id)
        .unwrap()
        .1
        .update
        .permission_requests[0]
        .1;
    app.structured_workers
        .runtime(&case.id, case.generation)
        .unwrap()
        .agent_decide(
            &Principal::Token(permissions::auth::fixture("fixture-operator", true)),
            &AgentDecide {
                agent: case.id.clone(),
                request_id: "grant-origin".into(),
                request_seq: seq,
                decision: Decision::AllowAlways,
                message: None,
            },
        )
        .unwrap();
    wait(&notify, || {
        app.structured_workers.poll(
            &mut app.state.structured_agents,
            std::time::Instant::now(),
            1000,
        );
        app.state
            .structured_agents
            .get(&case.id)
            .unwrap()
            .1
            .update
            .needs_human
            .is_empty()
    })
    .await;
    let listed = request(
        &mut app,
        "structured.grants.list",
        json!({"agent_id":case.id}),
    );
    assert_eq!(listed["result"]["generation"], case.generation);
    assert_eq!(listed["result"]["grants"][0]["tool"], "Bash");
    let _: crate::api::schema::SuccessResponse = serde_json::from_value(listed.clone()).unwrap();
    let mut params = json!({"agent_id":case.id, "generation":case.generation+1, "request_id":"grant-origin","request_seq":seq});
    assert_eq!(
        request(&mut app, "structured.grants.revoke", params.clone())["error"]["code"],
        "permission_grant_stale_generation"
    );
    params["generation"] = json!(case.generation);
    app.current_api_peer_pid = None;
    for (method, p) in [
        ("structured.grants.list", json!({"agent_id":case.id})),
        ("structured.grants.revoke", params.clone()),
    ] {
        assert_eq!(
            request(&mut app, method, p)["error"]["code"],
            "operator_only"
        );
    }
    app.current_api_peer_pid = Some(std::process::id());
    let revoked = request(&mut app, "structured.grants.revoke", params);
    assert_eq!(revoked["result"]["revoked"]["request_seq"], seq);
    assert_eq!(revoked["result"]["grants"], json!([]));
    let events = app.event_hub.ring_snapshot_after(0);
    assert!(events.iter().any(|(_, e)| matches!(&e.data, crate::api::schema::EventData::StructuredGrantRevoked { request_seq, principal, .. } if *request_seq==seq && principal.starts_with("operator:pid:"))));
    case.finish(
        &mut app.state.structured_agents,
        &mut app.structured_workers,
        &notify,
        std::time::Instant::now(),
        None,
    )
    .await;
}
