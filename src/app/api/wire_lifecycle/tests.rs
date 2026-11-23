use super::*;
use crate::{
    structured::{
        view::Target,
        workers::tests::global::{max_policy, AppQueue},
    },
    web::wire::claim_lifecycle_fixture as issue,
    workspace::Workspace,
};
use serde_json::json;

fn run(future: impl std::future::Future<Output = ()>) {
    let (_guard, _scrub) = crate::config::test_config_env_guard();
    crate::web::wire::tests::runtime(future);
}
async fn fixture(n: u32) -> AppQueue {
    let mut case = AppQueue::new(n, |_| max_policy()).await;
    case.app.node_id = Some("b".repeat(64));
    let mut destination = Workspace::test_new("bound-destination");
    destination.id = "w1".into();
    let mut focused = Workspace::test_new("focused-other");
    focused.id = "w2".into();
    case.app.state.workspaces = vec![destination, focused];
    case.app
        .place_structured_view(
            Target {
                agent_id: case.case.id.clone(),
                generation: case.case.generation,
            },
            Some((0, 0)),
        )
        .unwrap();
    case.app.state.active = Some(1);
    case
}
fn stop(target: &str) -> AgentLifecycleCommand {
    serde_json::from_value(json!({"verb":"agent.stop", "args":{"target":target}})).unwrap()
}
#[test]
fn bound_grant_stops_exact_target_without_operator_identity() {
    run(async {
        let mut case = fixture(32801).await;
        let id = case.case.id.clone();
        let grant = issue("agent.stop", json!({"target":id}), &id, "w1").await;
        let authority = Authority::Grant(Box::new(grant));
        assert!(!authority.app_operator());
        assert_eq!(
            authority.remote_subject().unwrap().parent_key(),
            json!(["mesh-grant-v1", {"home":"a".repeat(64),"principal":"client-1"}]).to_string()
        );
        assert!(case.app.wire_agent_lifecycle(stop(&id), authority).is_ok());
        assert_eq!(case.app.state.active, Some(1));
        case.stop().await;
    });
}
#[test]
fn grant_rejects_changed_target_verb_payload_and_destination_before_handler() {
    run(async {
        let mut case = fixture(32802).await;
        let id = case.case.id.clone();
        let changes = [stop("agent_atlas_other"),
            serde_json::from_value(json!({"verb":"agent.restart", "args":{"target":id, "reason":"test", "when":"after_turn"}})).unwrap(),
            serde_json::from_value(json!({"verb":"agent.start", "args":{"name":"other", "argv":["claude"], "runtime":"structured", "workspace_id":"w1"}})).unwrap()];
        for changed in changes {
            let grant = issue("agent.stop", json!({"target":id}), &id, "w1").await;
            assert!(matches!(
                case.app
                    .wire_agent_lifecycle(changed, Authority::Grant(Box::new(grant))),
                Err(ErrorCode::NotAuthorized)
            ));
        }
        let args = json!({"target":id, "reason":"first", "when":"after_turn"});
        let grant = issue("agent.restart", args.clone(), &id, "w1").await;
        let mut changed = args;
        changed["reason"] = json!("changed");
        let changed =
            serde_json::from_value(json!({"verb":"agent.restart", "args":changed})).unwrap();
        assert!(matches!(
            case.app
                .wire_agent_lifecycle(changed, Authority::Grant(Box::new(grant))),
            Err(ErrorCode::NotAuthorized)
        ));
        let grant = issue("agent.stop", json!({"target":id}), &id, "w1").await;
        case.app.node_id = Some("c".repeat(64));
        assert!(matches!(
            case.app
                .wire_agent_lifecycle(stop(&id), Authority::Grant(Box::new(grant))),
            Err(ErrorCode::NotAuthorized)
        ));
        case.stop().await;
    });
}
#[test]
fn grant_refuses_wrong_workspace_even_for_matching_agent() {
    run(async {
        let mut case = fixture(32803).await;
        let id = case.case.id.clone();
        let grant = issue("agent.stop", json!({"target":id}), &id, "w2").await;
        assert!(matches!(
            case.app
                .wire_agent_lifecycle(stop(&id), Authority::Grant(Box::new(grant))),
            Err(ErrorCode::NotAuthorized)
        ));
        assert_eq!(case.app.state.active, Some(1));
        case.stop().await;
    });
}
#[test]
fn granted_start_uses_exact_workspace_and_keeps_argv_checks() {
    run(async {
        let mut case = fixture(32804).await;
        let args = json!({"runtime":"structured", "name":"fresh", "argv":["fixture-refused"], "workspace_id":"w1"});
        let grant = issue("agent.start", args.clone(), "w1", "w1").await;
        let command: AgentLifecycleCommand =
            serde_json::from_value(json!({"verb":"agent.start", "args":args})).unwrap();
        let AgentLifecycleCommand::Start(params) = &command else {
            panic!()
        };
        let params = params.clone();
        assert_eq!(
            grant.start(case.app.node_id.as_deref(), &params).unwrap(),
            "w1"
        );
        assert_eq!(case.app.granted_destination("w1", None).unwrap(), (0, 0));
        assert_eq!(case.app.state.active, Some(1));
        assert!(case.app.granted_destination("w3", None).is_err());
        assert!(matches!(
            case.app
                .wire_agent_lifecycle(command, Authority::Grant(Box::new(grant))),
            Err(ErrorCode::InvalidAgentArgv)
        ));
        let grant = issue(
            "agent.stop",
            json!({"target":case.case.id}),
            &case.case.id,
            "w1",
        )
        .await;
        let authority = Authority::Grant(Box::new(grant));
        assert!(case
            .app
            .start_structured_request_authenticated(
                params.clone(),
                &super::super::spawn::SpawnCaller::Operator { pane: None },
                crate::spawn::SpawnVerb::Fork,
                None,
                Some(&authority)
            )
            .is_err());
        let args = json!({"runtime":"pty", "name":"wrong-handler", "argv":["claude"], "workspace_id":"w1", "dry_run":true});
        let grant = issue("agent.start", args.clone(), "w1", "w1").await;
        let authority = Authority::Grant(Box::new(grant));
        let params = serde_json::from_value(args).unwrap();
        let refusal = case
            .app
            .start_structured_request_authenticated(
                params,
                &super::super::spawn::SpawnCaller::Operator { pane: None },
                crate::spawn::SpawnVerb::Fork,
                None,
                Some(&authority),
            )
            .expect_err("a PTY grant cannot enter the structured handler");
        assert_eq!(refusal, ErrorCode::NotAuthorized);
        case.stop().await;
    });
}

#[test]
fn granted_resume_preserves_runtime_policy_and_cannot_enter_stop_handler() {
    run(async {
        let mut case = fixture(32805).await;
        let id = case.case.id.clone();
        let args = json!({"target":id, "reason":"resume", "when":"after_turn"});
        let grant = issue("agent.restart", args.clone(), &id, "w1").await;
        let command = serde_json::from_value(json!({"verb":"agent.restart", "args":args})).unwrap();
        assert!(matches!(
            case.app
                .wire_agent_lifecycle(command, Authority::Grant(Box::new(grant))),
            Err(ErrorCode::StructuredNeedsAgentUser)
        ));
        let grant = issue(
            "agent.restart",
            json!({"target":id, "reason":"resume"}),
            &id,
            "w1",
        )
        .await;
        let authority = Authority::Grant(Box::new(grant));
        let refused = case.app.handle_structured_stop_authorized(
            String::new(),
            crate::api::schema::AgentTarget { target: id.clone() },
            super::super::spawn::SpawnCaller::Operator { pane: None },
            Some(&authority),
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&refused).unwrap()["error"]["code"],
            "not_authorized"
        );
        let args =
            json!({"runtime":"structured", "name":id, "argv":["claude"], "workspace_id":"w1"});
        let grant = issue("agent.start", args.clone(), "w1", "w1").await;
        let authority = Authority::Grant(Box::new(grant));
        let params = serde_json::from_value(args).unwrap();
        assert!(matches!(
            case.app.start_structured_request_authenticated(
                params,
                &super::super::spawn::SpawnCaller::Operator { pane: None },
                crate::spawn::SpawnVerb::Fork,
                None,
                Some(&authority)
            ),
            Err(ErrorCode::NotAuthorized)
        ));
        let missing = "agent_atlas_missing";
        let grant = issue(
            "agent.restart",
            json!({"target":missing, "reason":"resume"}),
            missing,
            "w1",
        )
        .await;
        let authority = Authority::Grant(Box::new(grant));
        let refusal = case
            .app
            .start_structured_request_authenticated(
                restart_start(missing).unwrap(),
                &super::super::spawn::SpawnCaller::Operator { pane: None },
                crate::spawn::SpawnVerb::Fork,
                None,
                Some(&authority),
            )
            .expect_err("resume cannot become a fresh launch");
        assert_eq!(refusal, ErrorCode::NotAuthorized);
        case.stop().await;
    });
}

async fn non_structured_target_is_fenced(verb: &str, n: u32) {
    let mut case = fixture(n).await;
    case.app.state.ensure_test_terminals();
    let pane = case.app.state.workspaces[1].focused_pane_id().unwrap();
    let terminal_id = case.app.state.workspaces[1]
        .terminal_id(pane)
        .unwrap()
        .clone();
    let terminal = case.app.state.terminals.get_mut(&terminal_id).unwrap();
    terminal.detected_agent = Some(crate::detect::Agent::Claude);
    let id = terminal.agent_id.to_string();
    assert!(case.app.resolve_terminal_target(&id).is_ok());
    assert!(case.app.structured_target(&id).unwrap().is_none());
    let args = if verb == "agent.stop" {
        json!({"target":id})
    } else {
        json!({"target":id,"reason":"","when":"after_turn"})
    };
    let grant = issue(verb, args.clone(), &id, "w2").await;
    let command = serde_json::from_value(json!({"verb":verb,"args":args})).unwrap();
    assert_eq!(
        case.app
            .wire_agent_lifecycle(command, Authority::Grant(Box::new(grant)))
            .unwrap_err(),
        ErrorCode::AgentNotFound
    );
    case.stop().await;
}
#[test]
fn grant_cannot_enter_legacy_terminal_stop_handler() {
    run(non_structured_target_is_fenced("agent.stop", 32811));
}
#[test]
fn grant_cannot_enter_legacy_terminal_restart_handler() {
    run(non_structured_target_is_fenced("agent.restart", 32812));
}
async fn unbound_launch_inputs_are_fenced(verb: crate::spawn::SpawnVerb, opening: bool, n: u32) {
    let mut case = fixture(n).await;
    let args =
        json!({"runtime":"structured","name":"grant-framed","argv":["claude"],"workspace_id":"w1"});
    let grant = issue("agent.start", args.clone(), "w1", "w1").await;
    let authority = Authority::Grant(Box::new(grant));
    let prompt = opening
        .then(|| crate::spawn::prompt::SpawnPrompt::compose("fixture opening turn").unwrap());
    let result = case.app.start_structured_request_authenticated(
        serde_json::from_value(args).unwrap(),
        &super::super::spawn::SpawnCaller::Operator { pane: None },
        verb,
        prompt,
        Some(&authority),
    );
    assert_eq!(result.unwrap_err(), ErrorCode::NotAuthorized);
    case.stop().await;
}
#[test]
fn grant_cannot_supply_an_unbound_opening_turn() {
    run(unbound_launch_inputs_are_fenced(
        crate::spawn::SpawnVerb::Fork,
        true,
        32813,
    ));
}
#[test]
fn grant_cannot_switch_the_spawn_verb() {
    run(unbound_launch_inputs_are_fenced(
        crate::spawn::SpawnVerb::Spawn,
        false,
        32814,
    ));
}
#[test]
fn bound_workspace_requires_valid_tab_membership() {
    run(async {
        let mut case = fixture(32815).await;
        let panes = std::mem::take(&mut case.app.state.workspaces[0].tabs[0].panes);
        assert!(!case.app.state.workspaces[0].tabs[0].membership_valid());
        let result = case.app.granted_destination("w1", None);
        case.app.state.workspaces[0].tabs[0].panes = panes;
        assert_eq!(result.unwrap_err(), ErrorCode::NotAuthorized);
        assert_eq!(case.app.state.active, Some(1));
        case.stop().await;
    });
}
#[test]
fn grants_for_stop_and_restart_reject_reusable_names() {
    run(async {
        let mut case = fixture(32816).await;
        let alias = case
            .app
            .state
            .structured_agents
            .get(&case.case.id)
            .unwrap()
            .0
            .name
            .clone()
            .unwrap();
        assert_ne!(alias, case.case.id);
        for verb in ["agent.stop", "agent.restart"] {
            let args = if verb == "agent.stop" {
                json!({"target":alias})
            } else {
                json!({"target":alias,"reason":"resume","when":"after_turn"})
            };
            let grant = issue(verb, args.clone(), &alias, "w1").await;
            let command = serde_json::from_value(json!({"verb":verb,"args":args})).unwrap();
            assert_eq!(
                case.app
                    .wire_agent_lifecycle(command, Authority::Grant(Box::new(grant)))
                    .unwrap_err(),
                ErrorCode::NotAuthorized,
                "{verb}"
            );
        }
        case.stop().await;
    });
}
