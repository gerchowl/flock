use super::{tests::*, *};
use crate::api::gateway_grants::{Lifecycle, Role, Subject, SubjectKind};
fn pair(wall_ms: u64, elapsed_ms: u64) -> ClockPair {
    ClockPair {
        domain: "fixture-domain".into(),
        wall_ms,
        elapsed_ms,
    }
}
fn policy() -> Lifecycle {
    Lifecycle {
        subject: Subject::User,
        destination_node_id: "b".repeat(64),
        max_uses: Some(1),
        lifetime_ms: None,
        issuance: None,
    }
}
fn evaluate_case(mutator: impl FnOnce(&mut Grant, &mut Evaluation<'_>), expected: Decision) {
    let (_root, store, mut app) = fixture();
    let mut g = grant(&["agent:agent_atlas_1"]);
    g.lifecycle = Some(policy().into());
    let base = g.clone();
    let mut r = request(&base, &base.scopes);
    r.verb = "agent.stop";
    let destination = "b".repeat(64);
    let consumed = BTreeMap::new();
    let mut context = Evaluation {
        request: &r,
        source: PrincipalKind::User,
        destination: &destination,
        clock: Some(pair(10000, 20000)),
        consumed: &consumed,
    };
    mutator(&mut g, &mut context);
    let generation = store
        .manage_issued(
            &app.client_token_authority().unwrap(),
            Some(Change::Add(g)),
            || Ok((destination.clone(), Some(pair(10000, 20000)))),
        )
        .unwrap()
        .generation;
    let actual = store
        .evaluate(&context, generation, |_| Some("w1".into()))
        .unwrap();
    match (actual, expected) {
        (
            Decision::Allowed {
                remaining,
                issuance_id,
            },
            Decision::Allowed {
                remaining: wanted, ..
            },
        ) => {
            assert!(!issuance_id.is_empty());
            assert_eq!(remaining, wanted);
        }
        (actual, expected) => assert_eq!(actual, expected),
    }
}
fn allowed(n: u64) -> Decision {
    Decision::Allowed {
        issuance_id: String::new(),
        remaining: Some(n),
    }
}
#[test]
fn user_once_allows() {
    evaluate_case(|_, _| {}, allowed(1));
}
#[test]
fn agent_only_matches_its_authenticated_subject() {
    evaluate_case(
        |g, c| {
            g.lifecycle.as_mut().unwrap().subject = Subject::Agent;
            c.source = PrincipalKind::Agent;
        },
        allowed(1),
    );
    evaluate_case(
        |g, _| {
            g.lifecycle.as_mut().unwrap().subject = Subject::Agent;
        },
        Decision::Refused(Refusal::Principal),
    );
    evaluate_case(
        |g, c| {
            g.principals = ["agent_atlas_2".into()].into();
            g.lifecycle.as_mut().unwrap().subject = Subject::Agent;
            c.source = PrincipalKind::Agent;
        },
        Decision::Refused(Refusal::Principal),
    );
}
#[test]
fn file_principal_refused() {
    evaluate_case(
        |_, c| c.source = PrincipalKind::File,
        Decision::Refused(Refusal::Principal),
    );
}
#[test]
fn verb_outside_set_refused() {
    evaluate_case(
        |g, _| {
            g.verbs.remove("agent.stop");
        },
        Decision::Refused(Refusal::Verb),
    );
}
#[test]
fn target_mismatch_refused() {
    evaluate_case(
        |g, _| g.targets = ["agent:agent_atlas_2".into()].into(),
        Decision::Refused(Refusal::Target),
    );
}
#[test]
fn count_and_destination_scope() {
    evaluate_case(
        |g, _| g.lifecycle.as_mut().unwrap().max_uses = Some(7),
        allowed(7),
    );
    evaluate_case(
        |_, c| c.destination = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        Decision::Refused(Refusal::Destination),
    );
}
#[test]
fn destination_expiry_checks_both_clocks_and_missing_evidence() {
    evaluate_case(
        |g, _| g.lifecycle.as_mut().unwrap().lifetime_ms = Some(5000),
        allowed(1),
    );
    for clock in [
        None,
        Some(pair(14000, 24000)),
        Some(pair(15000, 25000)),
        Some(pair(9000, 21000)),
        Some(pair(10000, 25000)),
    ] {
        evaluate_case(
            |g, c| {
                g.lifecycle.as_mut().unwrap().lifetime_ms = Some(5000);
                c.clock = clock;
            },
            Decision::Refused(Refusal::Expired),
        );
    }
}
#[test]
fn roles_are_explicit_members_with_no_parentage_authority() {
    for role in [Role::Operator, Role::OwnerOfAgent, Role::Delegate] {
        evaluate_case(
            |g, _| {
                g.lifecycle.as_mut().unwrap().subject = Subject::Role {
                    role,
                    kind: SubjectKind::User,
                }
            },
            allowed(1),
        );
    }
    evaluate_case(
        |g, _| {
            g.principals.clear();
        },
        Decision::Refused(Refusal::Principal),
    );
}
#[test]
fn prechange_row_loads_unchanged() {
    let raw = json!({"generation":12,"entries":{"tablet":{"name":"tablet","home_node_id":"a".repeat(64),"principals":["client-1"],"verbs":["agent.send"],"scopes":["send"],"targets":["*"]}}});
    let (_root, store, mut app) = fixture();
    store
        .0
        .file("grants.json", true)
        .unwrap()
        .write_all(&serde_json::to_vec(&raw).unwrap())
        .unwrap();
    let listing = store
        .manage(&app.client_token_authority().unwrap(), None)
        .unwrap();
    let g = listing.entries["tablet"].as_ref().unwrap();
    assert!(g.lifecycle.is_none());
    assert_eq!(serde_json::to_value(g).unwrap(), raw["entries"]["tablet"]);
}

#[test]
fn recovered_counts_revocation_and_nonconsumption() {
    let (root, store, mut app) = fixture();
    let mut g = grant(&["*"]);
    g.lifecycle = Some(
        Lifecycle {
            max_uses: Some(7),
            ..policy()
        }
        .into(),
    );
    let generation = add(&store, &mut app, g.clone());
    let listed = store
        .manage(&app.client_token_authority().unwrap(), None)
        .unwrap();
    let id = listed.entries["tablet"]
        .as_ref()
        .unwrap()
        .lifecycle
        .as_ref()
        .unwrap()
        .issuance
        .as_ref()
        .unwrap()
        .id
        .clone();
    let mut r = request(&g, &g.scopes);
    r.verb = "agent.stop";
    let mut consumed = BTreeMap::new();
    let destination = "b".repeat(64);
    let path = root.path().join("gateway-grants/grants.json");
    let before = std::fs::read(&path).unwrap();
    let eval = |consumed: &BTreeMap<String, u64>, generation| {
        store
            .evaluate(
                &Evaluation {
                    request: &r,
                    source: PrincipalKind::User,
                    destination: &destination,
                    clock: None,
                    consumed,
                },
                generation,
                |_| Some("w1".into()),
            )
            .unwrap()
    };
    for _ in 0..2 {
        assert!(matches!(
            eval(&consumed, generation),
            Decision::Allowed {
                remaining: Some(7),
                ..
            }
        ));
    }
    assert_eq!(std::fs::read(&path).unwrap(), before);
    consumed.insert(id.clone(), 2);
    assert!(matches!(
        eval(&consumed, generation),
        Decision::Allowed {
            remaining: Some(5),
            ..
        }
    ));
    consumed.insert(id.clone(), 7);
    assert_eq!(
        eval(&consumed, generation),
        Decision::Refused(Refusal::Exhausted)
    );
    store
        .manage(
            &app.client_token_authority().unwrap(),
            Some(Change::Revoke(g.name.clone())),
        )
        .unwrap();
    assert_eq!(
        eval(&consumed, generation),
        Decision::Refused(Refusal::Revoked)
    );
    add(&store, &mut app, g);
    let listed = store
        .manage(&app.client_token_authority().unwrap(), None)
        .unwrap();
    assert_ne!(
        listed.entries["tablet"]
            .as_ref()
            .unwrap()
            .lifecycle
            .as_ref()
            .unwrap()
            .issuance
            .as_ref()
            .unwrap()
            .id,
        id
    );
}

#[test]
fn invalid_issuance_inputs_refuse() {
    let (_root, store, mut app) = fixture();
    for (max_uses, lifetime_ms) in [(Some(0), None), (None, Some(0)), (None, Some(u64::MAX))] {
        let mut g = grant(&["*"]);
        g.lifecycle = Some(
            Lifecycle {
                max_uses,
                lifetime_ms,
                ..policy()
            }
            .into(),
        );
        let result = store.manage_issued(
            &app.client_token_authority().unwrap(),
            Some(Change::Add(g)),
            || Ok(("b".repeat(64), Some(pair(1, 1)))),
        );
        assert_eq!(result.unwrap_err(), E::InvalidParams);
    }
}

#[tokio::test]
async fn agent_holding_a_grant_cannot_issue_one() {
    use crate::api::schema::{Method, Request as ApiRequest};
    let (_root, store, mut app) = fixture();
    let mut g = grant(&["*"]);
    g.lifecycle = Some(
        Lifecycle {
            subject: Subject::Agent,
            ..policy()
        }
        .into(),
    );
    g.principals = ["agent_atlas_1".into()].into();
    add(&store, &mut app, g.clone());
    let mut lease = app
        .session_leases
        .reserve("agent_atlas_1", "grant-fixture", None)
        .unwrap();
    lease.bind(std::process::id()).unwrap();
    assert!(matches!(
        app.session_leases.caller(Some(std::process::id())),
        crate::structured::lease::Caller::Agent { .. }
    ));
    g.name = "delegated".into();
    let response: Value = serde_json::from_str(
        &app.handle_api_request_after_internal_events_drained(ApiRequest {
            id: "grant-fixture".into(),
            method: Method::ClientGrantAdd(g),
        }),
    )
    .unwrap();
    assert_eq!(response["error"]["code"], "operator_only");
}

#[test]
fn newer_role_restriction_cannot_be_ignored() {
    let mut g = grant(&["agent:agent_atlas_1"]);
    g.lifecycle = Some(
        Lifecycle {
            subject: Subject::Role {
                role: Role::Delegate,
                kind: SubjectKind::User,
            },
            ..policy()
        }
        .into(),
    );
    let (_root, store, mut app) = fixture();
    add(&store, &mut app, g);
    let (_lease, mut raw) = store.snapshot().unwrap();
    raw["entries"]["tablet"]["lifecycle"]["subject"]["role"]["future_restriction"] = json!(true);
    assert_eq!(Store::listing(&raw).entries["tablet"], None);
}

#[test]
fn retirement_requires_proven_expiry_in_the_issued_clock_domain() {
    let (_root, store, mut app) = fixture();
    let mut g = grant(&["*"]);
    let mut p = policy();
    p.lifetime_ms = Some(5000);
    g.lifecycle = Some(Box::new(p));
    let listing = store
        .manage_issued(
            &app.client_token_authority().unwrap(),
            Some(Change::Add(g)),
            || Ok(("b".repeat(64), Some(pair(10000, 20000)))),
        )
        .unwrap();
    let p = listing.entries["tablet"]
        .as_ref()
        .unwrap()
        .lifecycle
        .as_ref()
        .unwrap();
    for clock in [None, Some(pair(14999, 25000)), Some(pair(15000, 24999))] {
        assert!(!lifecycle::expired(p, clock.as_ref()));
    }
    assert!(lifecycle::expired(p, Some(&pair(15000, 25000))));
    let mut other = pair(20000, 30000);
    other.domain = "other-fixture-domain".into();
    assert!(!lifecycle::expired(p, Some(&other)));
}

#[test]
fn agent_cannot_obtain_clock_acknowledgement_authority() {
    let (_root, store, mut app) = fixture();
    let mut lease = app
        .session_leases
        .reserve("agent_atlas_1", "clock-fixture", None)
        .unwrap();
    lease.bind(std::process::id()).unwrap();
    match app.client_token_authority() {
        Ok(authority) => {
            store.acknowledge_clock(&authority).unwrap();
            panic!("agent gained operator authority");
        }
        Err(error) => assert_eq!(error, E::OperatorOnly),
    };
}

#[test]
fn elapsed_before_issuance_refuses_without_the_clock_latch() {
    let mut p = policy();
    p.lifetime_ms = Some(10000);
    p.issuance = Some(crate::api::gateway_grants::Issuance {
        id: "fixture-issuance".into(),
        issuer: "fixture-issuer".into(),
        issued: Some(pair(10000, 20000)),
        expires_at_ms: Some(20000),
        elapsed_deadline_ms: Some(30000),
    });
    assert!(!super::lifecycle::unexpired(&p, Some(&pair(11000, 19999))));
    assert!(super::lifecycle::unexpired(&p, Some(&pair(11000, 21000))));
}
