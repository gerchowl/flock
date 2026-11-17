use crate::{api::schema::Request, app::App, config::Config};
use serde_json::{json, Value};

fn app(config: &Config) -> App {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(config, true, None, rx, crate::api::EventHub::default());
    app.runtime_config.uid_policy = crate::structured::identity::UidPolicy {
        operator: 7,
        dedicated: Ok(Some(42)).into(),
    };
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
