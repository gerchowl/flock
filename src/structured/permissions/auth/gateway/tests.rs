use super::*;
use crate::{
    app::App,
    structured::{identity::UidPolicy, launch::SessionConfigDir},
};
use std::{cell::Cell, os::unix::net::UnixStream};

pub(super) fn operator() -> App {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &crate::config::Config::default(),
        true,
        None,
        rx,
        crate::api::EventHub::default(),
    );
    let (left, _right) = UnixStream::pair().unwrap();
    let peer = crate::platform::peer::credentials(&left);
    app.runtime_config.uid_policy = UidPolicy {
        operator: crate::platform::peer::effective_uid(),
        dedicated: Ok(None).into(),
    };
    app.current_api_peer_pid = peer.pid;
    app.current_api_peer_birth = Some(peer.birth);
    app.current_api_peer_uid = Some(peer.uid);
    app
}
pub(super) fn grant(targets: &[&str]) -> Grant {
    Grant {
        lifecycle: None,
        name: "tablet".into(),
        home_node_id: "a".repeat(64),
        principals: ["client-1".into()].into(),
        verbs: [
            "agent.send".into(),
            "agent.decide".into(),
            "agent.start".into(),
            "agent.stop".into(),
            "agent.restart".into(),
            "agent.resume".into(),
            "agent.read".into(),
            "question.answer".into(),
        ]
        .into(),
        scopes: [Scope::Read, Scope::Send, Scope::Answer, Scope::Operate].into(),
        targets: targets.iter().map(|s| (*s).into()).collect(),
    }
}
pub(super) fn request<'a>(g: &'a Grant, scopes: &'a BTreeSet<Scope>) -> Request<'a> {
    Request {
        home: &g.home_node_id,
        principal: "client-1",
        scopes,
        verb: "agent.send",
        target: Target::Agent("agent_atlas_1"),
    }
}
pub(super) fn fixture() -> (SessionConfigDir, Store, App) {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let store = Store::open(root.path()).unwrap();
    (root, store, operator())
}
pub(super) fn add(store: &Store, app: &mut App, g: Grant) -> u64 {
    store
        .manage_issued(
            &app.client_token_authority().unwrap(),
            Some(Change::Add(g)),
            || {
                Ok((
                    "b".repeat(64),
                    Some(ClockPair {
                        domain: "fixture-domain".into(),
                        wall_ms: 10000,
                        elapsed_ms: 20000,
                    }),
                ))
            },
        )
        .unwrap()
        .generation
}

#[test]
fn current_home_token_needs_exact_destination_authority_before_claim() {
    let (_root, store, mut app) = fixture();
    let g = grant(&["agent:agent_atlas_1"]);
    let mut r = request(&g, &g.scopes);
    let claimed = Cell::new(false);
    let admit = |r: &Request<'_>, generation| {
        store.admit(
            r,
            generation,
            |_| Some("w1".into()),
            || {
                claimed.set(true);
                Ok(())
            },
        )
    };
    assert_eq!(admit(&r, 0), Err(E::NotAuthorized));
    assert!(!claimed.get());
    let generation = add(&store, &mut app, g.clone());
    let wrong_home = "b".repeat(64);
    r.home = &wrong_home;
    assert_eq!(admit(&r, generation), Err(E::NotAuthorized));
    r.home = &g.home_node_id;
    r.principal = "other";
    assert_eq!(admit(&r, generation), Err(E::NotAuthorized));
    r.principal = "client-1";
    r.target = Target::Agent("agent_atlas_2");
    assert_eq!(admit(&r, generation), Err(E::NotAuthorized));
    r.target = Target::Agent("agent_atlas_1");
    let read = [Scope::Read].into();
    r.scopes = &read;
    assert_eq!(admit(&r, generation), Err(E::Scope));
    let send = [Scope::Send].into();
    r.scopes = &send;
    r.verb = "agent.decide";
    assert_eq!(admit(&r, generation), Err(E::Scope));
    let answer = [Scope::Answer].into();
    r.scopes = &answer;
    assert_eq!(admit(&r, generation), Err(E::Scope));
    assert!(!claimed.get());
    r.verb = "question.answer";
    assert_eq!(admit(&r, generation), Ok(()));
    claimed.set(false);
    r.verb = "agent.send";
    r.scopes = &send;
    assert_eq!(admit(&r, generation), Ok(()));
    assert!(claimed.get());
}

#[test]
fn grant_cannot_promote_scope_and_all_restrictions_apply_to_one_record() {
    let (_root, store, mut app) = fixture();
    let mut g = grant(&["*"]);
    g.scopes = [Scope::Send].into();
    let generation = add(&store, &mut app, g.clone());
    let scopes = [Scope::Operate].into();
    let mut r = request(&g, &scopes);
    r.verb = "agent.decide";
    assert_eq!(
        store.admit(&r, generation, |_| Some("w1".into()), || Ok(())),
        Err(E::NotAuthorized)
    );
    let mut second = g.clone();
    second.name = "second".into();
    second.scopes = [Scope::Operate].into();
    second.verbs = ["agent.read".into()].into();
    let generation = add(&store, &mut app, second);
    assert_eq!(
        store.admit(&r, generation, |_| Some("w1".into()), || Ok(())),
        Err(E::NotAuthorized)
    );
}

#[test]
fn selectors_are_a_union_empty_denies_and_start_needs_workspace_or_star() {
    for (targets, agent, workspace, expected) in [
        (vec![], "agent_atlas_1", "w1", false),
        (vec!["*"], "agent_atlas_1", "w1", true),
        (vec!["agent:agent_atlas_1"], "agent_atlas_1", "w1", true),
        (vec!["agent:agent_atlas_1"], "agent_atlas_2", "w1", false),
        (vec!["workspace:w1"], "agent_atlas_1", "w1", true),
        (vec!["workspace:w1"], "agent_atlas_1", "w2", false),
        (
            vec!["agent:agent_atlas_2", "workspace:w1"],
            "agent_atlas_1",
            "w1",
            true,
        ),
    ] {
        assert_eq!(
            matches_target(&grant(&targets), Target::Agent(agent), workspace),
            expected
        );
    }
    assert!(!matches_target(
        &grant(&["agent:agent_atlas_1"]),
        Target::Start("w1"),
        "w1"
    ));
    for targets in [vec!["workspace:w1"], vec!["*"]] {
        assert!(matches_target(&grant(&targets), Target::Start("w1"), "w1"));
    }
}

#[test]
fn workspace_membership_is_resolved_again_at_admission() {
    let (_root, store, mut app) = fixture();
    let g = grant(&["workspace:w1"]);
    let generation = add(&store, &mut app, g.clone());
    let r = request(&g, &g.scopes);
    let workspace = Cell::new("w1");
    assert_eq!(
        store.admit(&r, generation, |_| Some(workspace.get().into()), || Ok(())),
        Ok(())
    );
    workspace.set("w2");
    assert_eq!(
        store.admit(
            &r,
            generation,
            |_| Some(workspace.get().into()),
            || panic!("moved agent claimed")
        ),
        Err::<(), _>(E::NotAuthorized)
    );
}

#[test]
fn remote_config_is_unsupported_even_without_storage_and_lifecycle_stays_refused() {
    let (root, store, mut app) = fixture();
    let g = grant(&["*"]);
    let generation = add(&store, &mut app, g.clone());
    let mut r = request(&g, &g.scopes);
    for verb in ["agent.start", "agent.stop", "agent.restart", "agent.resume"] {
        assert_eq!(required_scope(verb), Some(Scope::Operate));
        r.verb = verb;
        r.target = if verb == "agent.start" {
            Target::Start("w1")
        } else {
            Target::Agent("agent_atlas_1")
        };
        assert_eq!(
            store.admit(
                &r,
                generation,
                |_| Some("w1".into()),
                || panic!("remote lifecycle claimed")
            ),
            Err::<(), _>(E::UnsupportedForAgent)
        );
    }
    std::fs::remove_file(root.path().join("gateway-grants/grants.json")).unwrap();
    std::fs::write(root.path().join("gateway-grants/grants.json"), "invalid").unwrap();
    for verb in ["config.get", "config.set", "config.future"] {
        r.verb = verb;
        assert_eq!(
            store.admit(
                &r,
                generation,
                |_| panic!("config resolved target"),
                || panic!("config claimed")
            ),
            Err::<(), _>(E::Unsupported)
        );
    }
}

#[test]
fn durable_revocation_generation_fences_the_racing_claim_and_survives_restart() {
    let (root, store, mut app) = fixture();
    let g = grant(&["*"]);
    let generation = add(&store, &mut app, g.clone());
    assert_eq!(store.generation().unwrap(), generation);
    // Admission captured a generation before a second store revoked the grant.
    let second = Store::open(root.path()).unwrap();
    let revoked = second
        .manage(
            &app.client_token_authority().unwrap(),
            Some(Change::Revoke(g.name.clone())),
        )
        .unwrap();
    assert!(revoked.generation > generation);
    drop(second);
    let restored = Store::open(root.path()).unwrap();
    assert_eq!(
        restored.admit(
            &request(&g, &g.scopes),
            generation,
            |_| panic!("stale admission resolved"),
            || panic!("stale admission claimed")
        ),
        Err::<(), _>(E::AuthRevoked)
    );
    let generation2 = add(&restored, &mut app, g.clone());
    assert!(generation2 > revoked.generation);
    assert_eq!(
        restored.admit(&request(&g, &g.scopes), generation, |_| None, || Ok(())),
        Err(E::AuthRevoked)
    );
    let listed = Store::open(root.path())
        .unwrap()
        .manage(&app.client_token_authority().unwrap(), None)
        .unwrap();
    assert_eq!(listed.entries["tablet"], Some(g));
}

#[test]
fn claim_holds_the_same_cross_handle_lock_as_revoke() {
    let (root, store, mut app) = fixture();
    let g = grant(&["*"]);
    let generation = add(&store, &mut app, g.clone());
    let second = Store::open(root.path()).unwrap();
    assert_eq!(
        store.admit(
            &request(&g, &g.scopes),
            generation,
            |_| Some("w1".into()),
            || {
                assert_eq!(
                    second
                        .manage(
                            &app.client_token_authority().unwrap(),
                            Some(Change::Revoke(g.name.clone()))
                        )
                        .unwrap_err(),
                    E::SessionBusy
                );
                Ok(())
            }
        ),
        Ok(())
    );
    second
        .manage(
            &app.client_token_authority().unwrap(),
            Some(Change::Revoke(g.name)),
        )
        .unwrap();
}

#[test]
fn unknown_invalid_entries_are_skipped_logged_and_preserved_on_mutation() {
    let (root, store, mut app) = fixture();
    let g = grant(&["*"]);
    add(&store, &mut app, g.clone());
    let path = root.path().join("gateway-grants/grants.json");
    let mut raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    raw["entries"]["future"] = json!({"future":42});
    raw["future_root"] = json!([1, 2, 3]);
    std::fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
    let listed = store
        .manage(&app.client_token_authority().unwrap(), None)
        .unwrap();
    assert_eq!(listed.entries["future"], None);
    assert_eq!(listed.entries["tablet"], Some(g));
    store
        .manage(
            &app.client_token_authority().unwrap(),
            Some(Change::Revoke("tablet".into())),
        )
        .unwrap();
    let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["entries"]["future"], raw["entries"]["future"]);
    assert_eq!(saved["future_root"], raw["future_root"]);
}

#[test]
fn malformed_selector_and_unknown_scope_never_become_authority() {
    let (_root, store, mut app) = fixture();
    for targets in [
        vec!["workspace:pretty name"],
        vec!["agent:display-label"],
        vec![""],
        vec!["workspace:../../elsewhere"],
    ] {
        assert_eq!(
            store
                .manage(
                    &app.client_token_authority().unwrap(),
                    Some(Change::Add(grant(&targets)))
                )
                .unwrap_err(),
            E::InvalidParams
        );
    }
    assert_eq!(required_scope("future.command"), None);
    assert_eq!(required_scope("agent.decide"), Some(Scope::Operate));
    assert_eq!(required_scope("agent.read"), Some(Scope::Read));
}

#[tokio::test]
async fn every_management_verb_refuses_nonoperator_dispatch_context() {
    use crate::api::schema::{EmptyParams, Method, Request as ApiRequest};
    let mut app = operator();
    let uid = crate::platform::peer::effective_uid();
    for caller_uid in [None, Some(uid.wrapping_add(1))] {
        app.current_api_peer_uid = Some(caller_uid);
        for method in [
            Method::ClientGrantAdd(grant(&["*"])),
            Method::ClientGrantList(EmptyParams {}),
            Method::ClientGrantRevoke(crate::api::gateway_grants::Revoke {
                name: "tablet".into(),
            }),
        ] {
            let response: Value = serde_json::from_str(
                &app.handle_api_request_after_internal_events_drained(ApiRequest {
                    id: "grant-fixture".into(),
                    method,
                }),
            )
            .unwrap();
            assert_eq!(response["error"]["code"], "operator_only", "{response}");
        }
    }
}

#[test]
fn lifecycle_schema_and_nonconsuming_decisions() {
    use crate::api::gateway_grants::{Lifecycle, Subject};
    let (_root, store, mut app) = fixture();
    let mut g = grant(&["*"]);
    g.lifecycle = Some(
        Lifecycle {
            subject: Subject::User,
            destination_node_id: "b".repeat(64),
            max_uses: Some(1),
            lifetime_ms: None,
            issuance: None,
        }
        .into(),
    );
    let generation = add(&store, &mut app, g.clone());
    let mut r = request(&g, &g.scopes);
    r.verb = "agent.stop";
    let context = Evaluation {
        request: &r,
        source: PrincipalKind::User,
        destination: &"b".repeat(64),
        clock: None,
        consumed: &BTreeMap::new(),
    };
    for _ in 0..2 {
        assert!(matches!(
            store
                .evaluate(&context, generation, |_| Some("w1".into()))
                .unwrap(),
            Decision::Allowed {
                remaining: Some(1),
                ..
            }
        ));
    }
}
