use crate::{api::schema::Request, app::App, config::Config};
use serde_json::{json, Value};

fn app(config: &Config) -> App {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(config, true, None, rx, crate::api::EventHub::default());
    app.runtime_config.uid_policy = crate::structured::identity::UidPolicy {
        operator: 7,
        dedicated: Ok(Some(42)).into(),
    };
    app.structured_profiles = Default::default();
    app.current_api_peer_pid = Some(std::process::id());
    app.current_api_peer_birth = Some(crate::platform::process_start_time(std::process::id()));
    app.current_api_peer_uid = Some(Some(7));
    app
}

fn request() -> Request {
    serde_json::from_value(json!({
        "id": "profile-enable", "method": "profile.enable",
        "params": {"plan": "00000000-0000-4000-8000-000000000001", "expect": "review-digest"}
    }))
    .unwrap()
}

fn enable(app: &mut App) -> Value {
    let workspaces = app.state.workspaces.len();
    let mail = app.mailboxes.pending_messages().len();
    let leases = app.session_leases.is_empty();
    let response =
        serde_json::from_str(&app.handle_api_request_after_internal_events_drained(request()))
            .unwrap();
    assert_eq!(app.state.workspaces.len(), workspaces);
    assert_eq!(app.mailboxes.pending_messages().len(), mail);
    assert_eq!(app.session_leases.is_empty(), leases);
    assert_eq!(
        app.structured_profiles.admit(),
        Err(crate::structured::profiles::ProfileRefusal::Disabled)
    );
    response
}

#[tokio::test]
async fn operator_enable_refuses_incomplete_build() {
    let mut app = app(&Config::default());
    assert!(app.caller_is_local_operator());
    assert_eq!(
        enable(&mut app)["error"]["code"],
        "structured_profiles_not_ready"
    );
}

#[tokio::test]
async fn dedicated_agent_cannot_enable() {
    let mut app = app(&Config::default());
    app.current_api_peer_uid = Some(Some(42));
    assert_eq!(enable(&mut app)["error"]["code"], "operator_only");
}

#[tokio::test]
async fn attested_pty_agent_cannot_enable() {
    let mut app = app(&Config::default());
    app.state.workspaces = vec![crate::workspace::Workspace::test_new("profile-agent")];
    app.state.ensure_test_terminals();
    let pane = app.state.workspaces[0].focused_pane_id().unwrap();
    let terminal = app.state.workspaces[0]
        .pane_state(pane)
        .unwrap()
        .attached_terminal_id
        .clone();
    app.state.terminals.get_mut(&terminal).unwrap().launch_argv = Some(vec!["claude".into()]);
    crate::test_support::attest_pane_caller(&mut app, pane);
    app.current_api_peer_birth = Some(
        app.current_api_peer_pid
            .and_then(crate::platform::process_start_time),
    );
    assert_eq!(enable(&mut app)["error"]["code"], "operator_only");
}

#[tokio::test]
async fn leased_structured_agent_cannot_enable() {
    let mut app = app(&Config::default());
    let mut lease = app
        .session_leases
        .reserve("agent_atlas_profile", "profile-session", None)
        .unwrap();
    lease.bind_test_peer(std::process::id());
    assert!(matches!(
        app.lease_caller(),
        crate::structured::lease::Caller::Agent { .. }
    ));
    assert_eq!(enable(&mut app)["error"]["code"], "operator_only");
}

#[tokio::test]
async fn pending_and_enrolled_relays_cannot_enable_even_without_uid_evidence() {
    for enrolled in [false, true] {
        for uid in [None, Some(Some(7))] {
            let mut app = app(&Config::default());
            app.current_api_peer_uid = uid;
            app.test_attach_relay(enrolled);
            let refused: Value = serde_json::from_str(
                &app.peer_uid_refusal(&request())
                    .expect("relay refused before dispatch"),
            )
            .unwrap();
            assert_eq!(refused["error"]["code"], "operator_only");
            assert_eq!(enable(&mut app)["error"]["code"], "operator_only");
        }
    }
}

#[tokio::test]
async fn agent_editable_config_cannot_enable_profiles() {
    let config: Config = toml::from_str("[structured_profiles]\nenabled = true\n").unwrap();
    assert!(!toml::to_string(&config)
        .unwrap()
        .contains("structured_profiles"));
    let mut app = app(&config);
    assert_eq!(
        enable(&mut app)["error"]["code"],
        "structured_profiles_not_ready"
    );
}

fn seed_bindings(app: &mut App) {
    let root = crate::test_support::unique_temp_path("profile-status");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("profile-bindings.json");
    std::fs::write(&path, serde_json::to_vec(&json!({
        "schema":1,"policy_revision":7,"execution_switch":false,"unknown":[],
        "bindings":[{"binding_id":"binding-sibling","target":{"repo_id":"repo-fixture",
        "node_ids":["node-fixture"],"agent_ids":["agent-sibling"]},"account_alias":"sibling-account",
        "credential_ref":"sibling-token-ref","credential_generation":1,"bundle_digests":["fixture-digest"],
        "grants":[],"revision":7,"revoked":null}]
    })).unwrap()).unwrap();
    app.structured_profiles = crate::structured::profiles::ProfileAdmission::load(&path);
    std::fs::remove_dir_all(root).unwrap();
}

fn status_request() -> Request {
    serde_json::from_value(json!({"id":"profile-status","method":"profile.status","params":{}}))
        .expect("read-only profile.status must be supported")
}

#[tokio::test]
async fn operator_status_reports_disabled_binding_state() {
    let mut app = app(&Config::default());
    let response: Value = serde_json::from_str(
        &app.handle_api_request_after_internal_events_drained(status_request()),
    )
    .unwrap();
    assert_eq!(response["result"]["execution_switch"], false);
    assert_eq!(response["result"]["policy_revision"], 0);
    assert_eq!(response["result"]["bindings"], json!([]));
    seed_bindings(&mut app);
    let response: Value = serde_json::from_str(
        &app.handle_api_request_after_internal_events_drained(status_request()),
    )
    .unwrap();
    assert_eq!(response["result"]["policy_revision"], 7);
    assert_eq!(
        response["result"]["bindings"][0]["binding_id"],
        "binding-sibling"
    );
}

#[tokio::test]
async fn non_operator_status_never_discloses_authority_or_usage() {
    fn redacted(app: &mut App) {
        seed_bindings(app);
        let mut request = status_request();
        // Self-reported identity and profile selection never confer authority.
        request = serde_json::from_value(json!({"id":request.id,"method":"profile.status",
            "params":{"operator":true,"agent_id":"agent-sibling","binding_id":"binding-sibling"}}))
        .unwrap();
        let response: Value =
            serde_json::from_str(&app.handle_api_request_after_internal_events_drained(request))
                .unwrap();
        assert_eq!(response["result"], json!({"type":"profile_status"}));
    }
    for enrolled in [false, true] {
        for uid in [None, Some(Some(7))] {
            let mut app = app(&Config::default());
            app.test_attach_relay(enrolled);
            app.current_api_peer_uid = uid;
            redacted(&mut app);
        }
    }
    let mut dedicated = app(&Config::default());
    dedicated.current_api_peer_uid = Some(Some(42));
    redacted(&mut dedicated);
    let mut unauthenticated = app(&Config::default());
    unauthenticated.current_api_peer_pid = None;
    redacted(&mut unauthenticated);
    let mut leased = app(&Config::default());
    let mut lease = leased
        .session_leases
        .reserve("agent_atlas_profile", "profile-session", None)
        .unwrap();
    lease.bind_test_peer(std::process::id());
    redacted(&mut leased);
}

fn mutation(method: &str, params: Value) -> Request {
    serde_json::from_value(json!({"id":"profile-transaction","method":method,"params":params}))
        .expect("profile transaction API must be supported")
}

fn proposed(binding: &str) -> Value {
    json!({"action":"bind","binding_id":binding,
        "target":{"repo_id":"repo-fixture","node_ids":["node-fixture"],"agent_ids":["agent-fixture"]},
        "account_alias":"fixture-account","credential_ref":"fixture-token-reference","credential_generation":1,
        "bundle_digests":["fixture-digest"],"grants":["fixture-select"]})
}

fn transactional_app() -> (App, std::path::PathBuf) {
    let root = crate::test_support::unique_temp_path("profile-transactions");
    std::fs::create_dir_all(&root).unwrap();
    let mut app = app(&Config::default());
    app.structured_profiles =
        crate::structured::profiles::ProfileAdmission::load(&root.join("profile-bindings.json"));
    (app, root)
}

fn transact(app: &mut App, method: &str, params: Value) -> Value {
    serde_json::from_str(
        &app.handle_api_request_after_internal_events_drained(mutation(method, params)),
    )
    .unwrap()
}

fn commit_plan(app: &mut App, plan: &Value) -> Value {
    transact(
        app,
        "profile.bind",
        json!({"plan":plan["plan"],"expect":plan["expect"]}),
    )
}

#[tokio::test]
async fn profile_mutations_refuse_relays_and_agents_before_any_effect() {
    let mut widened = proposed("binding-a");
    widened["mesh_scopes"] = json!(["read", "operate"]);
    for params in [
        ("profile.plan", widened),
        (
            "profile.bind",
            json!({"plan":"fixture-plan","expect":"fixture-digest"}),
        ),
        (
            "profile.revoke",
            json!({"binding":"binding-a","reason":"fixture revoke"}),
        ),
    ] {
        for enrolled in [false, true] {
            for uid in [None, Some(Some(7))] {
                let (mut app, root) = transactional_app();
                let before = std::fs::read_dir(&root).unwrap().count();
                app.test_attach_relay(enrolled);
                app.current_api_peer_uid = uid;
                assert_eq!(
                    transact(&mut app, params.0, params.1.clone())["error"]["code"],
                    "operator_only"
                );
                assert_eq!(std::fs::read_dir(&root).unwrap().count(), before);
                assert_eq!(app.structured_profiles.status(true)["policy_revision"], 0);
                std::fs::remove_dir_all(root).unwrap();
            }
        }
        let (mut app, root) = transactional_app();
        app.current_api_peer_uid = None;
        assert_eq!(
            transact(&mut app, params.0, params.1.clone())["error"]["code"],
            "operator_only"
        );
        assert!(!root.join("profile-bindings.json").exists());
        std::fs::remove_dir_all(root).unwrap();
        let (mut app, root) = transactional_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("profile-agent")];
        app.state.ensure_test_terminals();
        let pane = app.state.workspaces[0].focused_pane_id().unwrap();
        let terminal = app.state.workspaces[0]
            .pane_state(pane)
            .unwrap()
            .attached_terminal_id
            .clone();
        app.state.terminals.get_mut(&terminal).unwrap().launch_argv = Some(vec!["claude".into()]);
        crate::test_support::attest_pane_caller(&mut app, pane);
        app.current_api_peer_birth = Some(
            app.current_api_peer_pid
                .and_then(crate::platform::process_start_time),
        );
        assert_eq!(
            transact(&mut app, params.0, params.1.clone())["error"]["code"],
            "operator_only"
        );
        assert!(!root.join("profile-bindings.json").exists());
        std::fs::remove_dir_all(root).unwrap();
        let (mut app, root) = transactional_app();
        app.current_api_peer_uid = Some(Some(42));
        assert_eq!(
            transact(&mut app, params.0, params.1.clone())["error"]["code"],
            "operator_only"
        );
        assert!(!root.join("profile-bindings.json").exists());
        std::fs::remove_dir_all(root).unwrap();
        let (mut app, root) = transactional_app();
        let mut lease = app
            .session_leases
            .reserve("agent_atlas_profile", "profile-session", None)
            .unwrap();
        lease.bind_test_peer(std::process::id());
        assert_eq!(
            transact(&mut app, params.0, params.1)["error"]["code"],
            "operator_only"
        );
        assert!(!root.join("profile-bindings.json").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn bind_checks_digest_cas_and_durable_idempotence() {
    let (mut app, root) = transactional_app();
    let first = transact(&mut app, "profile.plan", proposed("binding-a"))["result"].clone();
    let stale = transact(&mut app, "profile.plan", proposed("binding-b"))["result"].clone();
    assert_eq!(
        transact(
            &mut app,
            "profile.bind",
            json!({"plan":first["plan"],"expect":"altered"})
        )["error"]["code"],
        "profile_plan_invalid"
    );
    assert!(!root.join("profile-bindings.json").exists());
    let applied = commit_plan(&mut app, &first);
    assert_eq!(applied["result"]["binding"]["revision"], 1);
    assert_eq!(commit_plan(&mut app, &first), applied);
    assert_eq!(
        commit_plan(&mut app, &stale)["error"]["code"],
        "profile_revision_conflict"
    );
    let saved = std::fs::read(root.join("profile-bindings.json")).unwrap();
    app.structured_profiles =
        crate::structured::profiles::ProfileAdmission::load(&root.join("profile-bindings.json"));
    assert_eq!(commit_plan(&mut app, &first), applied);
    assert_eq!(
        std::fs::read(root.join("profile-bindings.json")).unwrap(),
        saved
    );
    assert_eq!(app.structured_profiles.status(true)["policy_revision"], 1);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn revoke_is_terminal_durable_and_idempotent() {
    let (mut app, root) = transactional_app();
    let plan = transact(&mut app, "profile.plan", proposed("binding-a"))["result"].clone();
    let bound = commit_plan(&mut app, &plan);
    let revoked = transact(
        &mut app,
        "profile.revoke",
        json!({"binding":"binding-a","reason":"fixture revoke"}),
    );
    assert_eq!(revoked["result"]["binding"]["revision"], 2);
    assert_eq!(
        transact(
            &mut app,
            "profile.bind",
            json!({"plan":revoked["result"]["operation_id"],"expect":revoked["result"]["expect"]})
        )["error"]["code"],
        "profile_plan_invalid"
    );
    assert_eq!(
        revoked["result"]["binding"]["revoked"]["operation_id"],
        revoked["result"]["operation_id"]
    );
    app.structured_profiles =
        crate::structured::profiles::ProfileAdmission::load(&root.join("profile-bindings.json"));
    assert_eq!(
        transact(
            &mut app,
            "profile.revoke",
            json!({"binding":"binding-a","reason":"fixture revoke"})
        ),
        revoked
    );
    assert_eq!(commit_plan(&mut app, &plan), bound);
    assert_eq!(app.structured_profiles.status(true)["policy_revision"], 2);
    assert_eq!(
        transact(&mut app, "profile.plan", proposed("binding-a"))["error"]["code"],
        "profile_binding_revoked"
    );
    assert_eq!(
        app.structured_profiles.admit().unwrap_err().code().as_str(),
        "structured_profiles_disabled"
    );
    std::fs::remove_dir_all(root).unwrap();
}
