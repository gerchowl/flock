use super::fix1::{enroll, ingress, placed_app};
use super::*;
use crate::api::gateway_grants::{Role, Subject, SubjectKind};

fn foreign(f: &Fixture) -> Frame {
    let identity = NodeIdentity::fixture([9; 32]);
    let mut body = f.body.clone();
    body.home_node_id = identity.node_id();
    let mut value = serde_json::to_value(f.frame()).unwrap();
    let mut envelope: crate::mesh::store::Envelope =
        serde_json::from_value(value["envelope"].clone()).unwrap();
    envelope.body = serde_json::to_vec(&body).unwrap();
    envelope.key.origin_node = identity.node_id();
    envelope.return_binding.request = envelope.key.clone();
    crate::mesh::sign::seal(&mut envelope, &identity);
    value["envelope"] = serde_json::to_value(envelope).unwrap();
    let frame: Frame = serde_json::from_value(value).unwrap();
    assert_eq!(frame.verify().unwrap(), body);
    frame
}
async fn result(f: &Fixture) -> journal::grants::Consumption {
    let (first, grant) = f.submit(destination()).await;
    assert!(grant.is_some());
    let journal::Admission::Granted { consumption, state } = first else {
        panic!("claim")
    };
    let journal::Admission::Claimed(admitted_ms) = *state else {
        panic!("claim")
    };
    f.owner
        .submit(
            f.owner.gate().load(Ordering::Acquire),
            journal::Operation::Finish {
                key: (
                    f.body.home_node_id.clone(),
                    f.body.principal.clone(),
                    f.command.id.clone(),
                ),
                admitted_ms,
                result: Ok(json!({"effect":"stored"})),
            },
        )
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    consumption
}
#[test]
fn review_unenrolled_signature_is_not_verified_by_execute_audit() {
    let logs = crate::logging::capture_logs(|| {
        run(async {
            let _mesh = crate::mesh::runtime_store::TestStore::new();
            let f = Fixture::new(false, Some(1), false).await;
            let mut app = placed_app(&f);
            let mut incoming = ingress(&f);
            incoming.frame = foreign(&f);
            assert!(matches!(
                execute(&mut app, &f.owner, incoming).await.unwrap(),
                journal::Admission::Refused(Refusal::Enrollment)
            ));
            f.owner.shutdown().await.unwrap();
        })
    });
    let event = logs
        .lines()
        .find(|line| line.contains("wire.lifecycle.admission"))
        .unwrap();
    assert!(event.contains("identity=\"UNVERIFIED\""), "{logs}");
    assert!(event.contains("detail=\"Enrollment\""), "{logs}");
    assert!(event.contains("issuance=None"), "{logs}");
}
#[test]
fn review_unrelated_change_allows_original_stored_receipt() {
    run(async {
        let mut f = Fixture::new(false, Some(1), false).await;
        let original = result(&f).await;
        f.generation = gateway::claim_install(&f.store, "unrelated", None).0;
        let observed = f.owner.observe_writes_test();
        let (replay, grant) = f.submit(destination()).await;
        assert!(grant.is_none());
        assert!(
            matches!(replay, journal::Admission::Granted { consumption, state } if consumption == original && matches!(*state, journal::Admission::Replayed(crate::web::wire::ServerFrame::Ack { result: Some(ref value), .. }) if value["effect"] == "stored"))
        );
        f.generation = original.binding.as_ref().unwrap().generation;
        assert!(matches!(
            f.submit(destination()).await.0,
            journal::Admission::Granted { .. }
        ));
        assert_eq!(observed.sample().appended, 0);
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn review_revoked_original_issuance_is_audited_on_refusal() {
    let mut issuance = String::new();
    let logs = crate::logging::capture_logs(|| {
        run(async {
            let f = Fixture::new(false, Some(1), false).await;
            issuance = result(&f).await.issuance;
            gateway::claim_revoke(&f.store).unwrap();
            f.refused(destination(), Refusal::Revoked).await;
            f.owner.shutdown().await.unwrap();
        })
    });
    let event = logs
        .lines()
        .find(|line| line.contains("outcome=\"refused\""))
        .unwrap();
    assert!(
        event.contains(&issuance) && event.contains("use_ordinal=Some(1)"),
        "{logs}"
    );
}
#[test]
fn review_role_member_removal_refuses_replay_with_current_state() {
    run(async {
        let mut f = Fixture::new(false, Some(1), false).await;
        f.generation = gateway::claim_edit(&f.store, |g| {
            g.lifecycle.as_mut().unwrap().subject = Subject::Role {
                role: Role::Delegate,
                kind: SubjectKind::User,
            }
        });
        result(&f).await;
        f.generation = gateway::claim_edit(&f.store, |g| g.principals = ["client-2".into()].into());
        f.refused(destination(), Refusal::Principal).await;
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn review_token_revocation_is_rechecked_under_replay_lease() {
    run(async {
        use crate::structured::permissions::auth::tokens::{app_sessions, Scope, TokenError};
        let (_home, tokens, session) = app_sessions::replay_fixture();
        let mut f = Fixture::new(false, Some(1), false).await;
        f.body.principal = session.info.principal.clone();
        f.body.scopes = session.info.scopes.clone();
        f.generation = gateway::claim_edit(&f.store, |g| {
            g.principals = [session.info.principal.clone()].into()
        });
        result(&f).await;
        for revoked in [false, true] {
            if revoked {
                tokens.revoke(&session).unwrap();
            }
            let tokens = tokens.clone();
            let session = session.clone();
            let mut dest = destination();
            dest.read = Box::new(move |body| {
                tokens
                    .admit_command(&session, Scope::Operate, |token| {
                        let (principal, app, scopes) = token.command_facts();
                        assert_eq!(principal, body.principal);
                        assert!(app);
                        assert_eq!(scopes, &body.scopes);
                    })
                    .map_err(|error| match error {
                        TokenError::Revoked => Refusal::Revoked,
                        _ => panic!("unexpected token refusal"),
                    })?;
                Ok((None, Some("w1".into()), Some(Duration::from_millis(21000))))
            });
            if revoked {
                f.refused(dest, Refusal::Revoked).await;
            } else {
                assert!(matches!(
                    f.submit(dest).await.0,
                    journal::Admission::Granted { .. }
                ));
            }
        }
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn review_changed_authenticated_class_is_not_original_identity() {
    run(async {
        let mut f = Fixture::new(true, Some(1), false).await;
        result(&f).await;
        f.body.token_source = TokenSource::App;
        f.refused(destination(), Refusal::Principal).await;
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn review_execute_preverification_refusal_is_audited() {
    let logs = crate::logging::capture_logs(|| {
        run(async {
            let f = Fixture::new(false, Some(1), false).await;
            let mut app = placed_app(&f);
            let mut incoming = ingress(&f);
            let mut frame = serde_json::to_value(incoming.frame).unwrap();
            frame["envelope"]["signature"][0] =
                json!(frame["envelope"]["signature"][0].as_u64().unwrap() ^ 1);
            incoming.frame = serde_json::from_value(frame).unwrap();
            assert!(matches!(
                execute(&mut app, &f.owner, incoming).await.unwrap(),
                journal::Admission::Refused(Refusal::Provenance)
            ));
            f.owner.shutdown().await.unwrap();
        })
    });
    assert!(
        logs.lines()
            .any(|line| line.contains("wire.lifecycle.admission")
                && line.contains("UNVERIFIED")
                && line.contains("Provenance")),
        "{logs}"
    );
}
#[test]
fn review_execute_completion_has_result_audit_with_receipt() {
    let logs = crate::logging::capture_logs(|| {
        run(async {
            let _mesh = crate::mesh::runtime_store::TestStore::new();
            let f = Fixture::new(false, Some(1), false).await;
            enroll(&f);
            let mut app = placed_app(&f);
            assert!(matches!(
                execute(&mut app, &f.owner, ingress(&f)).await.unwrap(),
                journal::Admission::Granted { .. }
            ));
            f.owner.shutdown().await.unwrap();
        })
    });
    assert!(
        logs.lines()
            .any(|line| line.contains("wire.lifecycle.admission")
                && line.contains("outcome=\"result\"")
                && line.contains("use_ordinal=Some(1)")),
        "{logs}"
    );
}
#[test]
fn review_execute_phase_error_after_claim_is_audited() {
    let logs = crate::logging::capture_logs(|| {
        run(async {
            let _mesh = crate::mesh::runtime_store::TestStore::new();
            let f = Fixture::new(false, Some(1), false).await;
            enroll(&f);
            let mut app = placed_app(&f);
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
        })
    });
    assert!(
        logs.lines()
            .any(|line| line.contains("wire.lifecycle.admission")
                && line.contains("outcome=\"refused\"")
                && line.contains("session_unconfirmed")
                && line.contains("use_ordinal=Some(1)")),
        "{logs}"
    );
}

#[test]
fn review_current_target_membership_is_required_for_replay() {
    run(async {
        let f = Fixture::new(false, Some(1), false).await;
        result(&f).await;
        let mut dest = destination();
        dest.read = Box::new(|_| Ok((None, None, Some(Duration::from_millis(21000)))));
        f.refused(dest, Refusal::Target).await;
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn review_different_authenticated_namespace_cannot_read_original_result() {
    run(async {
        let f = Fixture::new(false, Some(1), false).await;
        result(&f).await;
        for home in [false, true] {
            let mut body = f.body.clone();
            let frame = if home {
                foreign(&f)
            } else {
                body.principal = "client-2".into();
                command::tests::admission_frame(body)
            };
            let mut retry = f.command.clone();
            retry.resume_only = true;
            let incoming = Ingress {
                frame,
                command: retry,
                since: Some(Duration::ZERO),
            };
            assert!(matches!(
                admit(
                    &f.owner,
                    f.owner.gate().load(Ordering::Acquire),
                    incoming,
                    f.store.clone(),
                    f.generation,
                    destination()
                )
                .await,
                Err(ErrorCode::SessionUnconfirmed)
            ));
        }
        f.owner.shutdown().await.unwrap();
    });
}

#[test]
fn review_unenrolled_binding_refusal_is_not_authenticated() {
    let logs = crate::logging::capture_logs(|| {
        run(async {
            let _mesh = crate::mesh::runtime_store::TestStore::new();
            let f = Fixture::new(false, Some(1), false).await;
            let mut app = placed_app(&f);
            let mut incoming = ingress(&f);
            incoming.frame = foreign(&f);
            incoming.command.args["changed"] = json!(true);
            assert!(matches!(
                execute(&mut app, &f.owner, incoming).await.unwrap(),
                journal::Admission::Refused(Refusal::Binding)
            ));
            f.owner.shutdown().await.unwrap();
        })
    });
    assert!(
        logs.lines()
            .any(|line| line.contains("wire.lifecycle.admission")
                && line.contains("UNVERIFIED")
                && line.contains("Binding")),
        "{logs}"
    );
}
#[test]
fn review_enrollment_refusal_never_marks_verified() {
    let logs = crate::logging::capture_logs(|| {
        run(async {
            let f = Fixture::new(false, Some(1), false).await;
            enroll(&f);
            let mut dest = destination();
            dest.read = Box::new(|_| Err(Refusal::Enrollment));
            f.refused(dest, Refusal::Enrollment).await;
            f.owner.shutdown().await.unwrap();
        })
    });
    assert!(
        logs.lines()
            .any(|line| line.contains("wire.lifecycle.admission")
                && line.contains("UNVERIFIED")
                && line.contains("Enrollment")),
        "{logs}"
    );
}
