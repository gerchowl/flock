use super::*;
use crate::{
    mesh::store::{IdentityPin, PinSource},
    structured::{
        registry::{Record, StructuredRuntime},
        view::Target as ViewTarget,
    },
    workspace::Workspace,
};

pub(super) fn placed_app(f: &Fixture) -> crate::app::App {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = crate::app::App::new(
        &crate::config::Config::default(),
        true,
        None,
        rx,
        crate::api::EventHub::default(),
    );
    app.node_id = Some(f.body.destination_node_id.clone());
    let id = match &f.body.target {
        Target::Agent(id) => id.clone(),
        _ => panic!("agent fixture"),
    };
    let generation = app
        .state
        .structured_agents
        .register(Record {
            runtime: StructuredRuntime::Structured,
            agent_id: id.clone(),
            name: None,
            harness: "claude".into(),
            session_id: "fixture-session".into(),
            harness_established: false,
            profile_identity: None,
            cwd: f._root.path().to_owned(),
            model: None,
        })
        .unwrap();
    let mut ws = Workspace::test_new("admission-member");
    ws.id = "w1".into();
    app.state.workspaces = vec![ws];
    app.state
        .attach_native_view(
            ViewTarget {
                agent_id: id,
                generation,
            },
            0,
            0,
            ratatui::layout::Direction::Horizontal,
        )
        .unwrap();
    app
}
pub(super) fn ingress(f: &Fixture) -> Ingress {
    Ingress {
        frame: f.frame(),
        since: crate::platform::suspend_clock::now(&crate::platform::suspend_clock::Kernel).ok(),
        command: f.command.clone(),
    }
}
fn pin(f: &Fixture) -> IdentityPin {
    IdentityPin {
        node_id: f.body.home_node_id.clone(),
        public_key: NodeIdentity::fixture([7; 32]).public_key().to_vec(),
    }
}
pub(super) fn enroll(f: &Fixture) {
    crate::mesh::hello::with_store(|s| {
        s.put_pin("explicit.example", &pin(f))
            .map_err(|e| e.to_string())
    })
    .unwrap();
}
#[test]
fn fix1_signed_command_id_cannot_be_rebound_to_a_fresh_key() {
    run(async {
        let f = Fixture::new(false, Some(2), false).await;
        claimed(f.submit(destination()).await, 1);
        let observation = f.owner.observe_writes_test();
        let mut command = f.command.clone();
        command.id = "rebound-key".into();
        let (result, grant) = f.submit_frame(f.frame(), command, destination()).await;
        assert!(matches!(
            result,
            journal::Admission::Refused(Refusal::Binding)
        ));
        assert!(grant.is_none());
        assert_eq!(observation.sample().appended, 0);
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn fix1_requested_scope_must_be_operate() {
    run(async {
        let mut f = Fixture::new(false, Some(1), false).await;
        f.body.scopes.insert(crate::api::client_tokens::Scope::Read);
        f.body.requested_scope = crate::api::client_tokens::Scope::Read;
        f.refused(destination(), Refusal::Scope).await;
        f.body.requested_scope = crate::api::client_tokens::Scope::Operate;
        claimed(f.submit(destination()).await, 1);
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn fix1_read_only_cannot_enter_lifecycle_dispatch() {
    run(async {
        let mut f = Fixture::new(false, Some(1), false).await;
        f.command.read_only = true;
        f.refused(destination(), Refusal::Verb).await;
        f.command.read_only = false;
        claimed(f.submit(destination()).await, 1);
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn fix1_signed_node_argument_must_match_destination() {
    run(async {
        let mut f = Fixture::new(false, Some(1), false).await;
        f.command.args["node_id"] = json!("c".repeat(64));
        f.body.args_digest = args_digest(&f.command.args);
        f.refused(destination(), Refusal::Binding).await;
        f.command.args["node_id"] = json!(f.body.destination_node_id);
        f.body.args_digest = args_digest(&f.command.args);
        claimed(f.submit(destination()).await, 1);
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn fix1_native_view_without_structured_member_refuses() {
    run(async {
        let f = Fixture::new(false, Some(1), false).await;
        let mut app = placed_app(&f);
        assert_eq!(app.gateway_lifecycle_member(&f.body), Ok("w1".into()));
        let Target::Agent(id) = &f.body.target else {
            panic!("agent fixture")
        };
        let generation = app.state.structured_agents.get(id).unwrap().2;
        app.state.structured_agents.remove(id, generation).unwrap();
        assert_eq!(app.gateway_lifecycle_member(&f.body), Err(Refusal::Target));
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn fix1_final_clock_latch_refuses_an_unexpired_grant() {
    run(async {
        let f = Fixture::new(false, Some(1), true).await;
        let mut reads = 0;
        let mut dest = destination();
        dest.read = Box::new(move |_| {
            reads += 1;
            let clock = if reads == 1 {
                pair(11000, 21000)
            } else {
                pair(10500, 21500)
            };
            Ok((
                Some(clock),
                Some("w1".into()),
                Some(Duration::from_millis(21000)),
            ))
        });
        f.refused(dest, Refusal::Expired).await;
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn fix1_final_membership_evaluation_refuses_without_clock_failure() {
    run(async {
        let f = Fixture::new(false, Some(1), false).await;
        let mut reads = 0;
        let mut dest = destination();
        dest.read = Box::new(move |_| {
            reads += 1;
            Ok((
                None,
                (reads == 1).then(|| "w1".into()),
                Some(Duration::from_millis(21000)),
            ))
        });
        f.refused(dest, Refusal::Target).await;
        claimed(f.submit(destination()).await, 1);
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn fix1_tofu_pins_are_not_explicit_enrollment() {
    run(async {
        let _mesh = crate::mesh::runtime_store::TestStore::new();
        let f = Fixture::new(false, Some(1), false).await;
        let mut app = placed_app(&f);
        let observation = f.owner.observe_writes_test();
        crate::mesh::hello::with_store(|s| {
            s.put_pin_from(PinSource::Inbound, "home.example", &pin(&f))
                .map_err(|e| e.to_string())
        })
        .unwrap();
        assert!(matches!(
            execute(&mut app, &f.owner, ingress(&f)).await.unwrap(),
            journal::Admission::Refused(Refusal::Enrollment)
        ));
        crate::mesh::hello::with_store(|s| {
            s.put_inbound_configured_pin("home.example", &pin(&f))
                .map_err(|e| e.to_string())
        })
        .unwrap();
        assert!(matches!(
            execute(&mut app, &f.owner, ingress(&f)).await.unwrap(),
            journal::Admission::Refused(Refusal::Enrollment)
        ));
        assert_eq!(observation.sample().appended, 0);
        enroll(&f);
        assert!(
            matches!(execute(&mut app, &f.owner, ingress(&f)).await.unwrap(), journal::Admission::Granted { consumption, .. } if consumption.ordinal == 1)
        );
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn fix1_phase_changed_after_claim_never_dispatches() {
    run(async {
        let _mesh = crate::mesh::runtime_store::TestStore::new();
        let f = Fixture::new(false, Some(1), false).await;
        let mut app = placed_app(&f);
        enroll(&f);
        let gate = f.owner.gate();
        f.owner
            .configure_test(
                None,
                Some(Box::new(move || {
                    gate.fetch_add(2, Ordering::AcqRel);
                })),
            )
            .await;
        assert!(matches!(
            execute(&mut app, &f.owner, ingress(&f)).await,
            Err(ErrorCode::SessionUnconfirmed)
        ));
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn fix1_phase_change_before_admission_writes_no_claim_or_use() {
    run(async {
        let _mesh = crate::mesh::runtime_store::TestStore::new();
        let f = Fixture::new(false, Some(1), false).await;
        let mut app = placed_app(&f);
        enroll(&f);
        let observation = f.owner.observe_writes_test();
        BEFORE_ADMIT.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(|owner| {
                Box::pin(async move {
                    owner.freeze().await.unwrap();
                    owner.acquire().await.unwrap();
                })
            }))
        });
        assert!(matches!(
            execute(&mut app, &f.owner, ingress(&f)).await,
            Err(ErrorCode::SessionBusy)
        ));
        assert_eq!(observation.sample().appended, 0);
        claimed(f.submit(destination()).await, 1);
        f.owner.shutdown().await.unwrap();
    });
}
