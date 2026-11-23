use super::*;

fn expired_destination() -> Destination {
    let mut dest = destination();
    dest.read = Box::new(|_| {
        Ok((
            Some(pair(20000, 30000)),
            Some("w1".into()),
            Some(Duration::from_millis(21000)),
        ))
    });
    dest
}
#[test]
fn expiry_does_not_withhold_claimed_pending_or_stored_result() {
    run(async {
        let mut f = Fixture::new(false, Some(1), true).await;
        let (first, _) = f.submit(destination()).await;
        let journal::Admission::Granted {
            consumption: original,
            state,
        } = first
        else {
            panic!("claim")
        };
        let journal::Admission::Claimed(admitted_ms) = *state else {
            panic!("claim")
        };
        let observation = f.owner.observe_writes_test();
        let (pending, grant) = f.submit(expired_destination()).await;
        assert!(grant.is_none());
        assert!(
            matches!(pending, journal::Admission::Granted { consumption, state } if consumption == original && matches!(*state, journal::Admission::Pending(_)))
        );
        assert_eq!(observation.sample().appended, 0);
        let stored = f
            .owner
            .submit(
                f.owner.gate().load(Ordering::Acquire),
                journal::Operation::Finish {
                    key: (
                        f.body.home_node_id.clone(),
                        f.body.principal.clone(),
                        f.command.id.clone(),
                    ),
                    admitted_ms,
                    result: Ok(json!({"effect":"complete"})),
                },
            )
            .unwrap()
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(stored, journal::Admission::Granted { consumption, state } if consumption == original && matches!(*state, journal::Admission::Replayed(_)))
        );
        let finished_bytes = observation.sample().appended;
        assert!(finished_bytes > 0);
        f.owner.freeze().await.unwrap();
        f.owner.acquire().await.unwrap();
        let (replay, grant) = f.submit(expired_destination()).await;
        assert!(grant.is_none());
        assert!(
            matches!(replay, journal::Admission::Granted { consumption, state } if consumption == original && matches!(*state, journal::Admission::Replayed(crate::web::wire::ServerFrame::Ack { ok: true, result: Some(ref value), .. }) if value["effect"] == "complete"))
        );
        assert_eq!(observation.sample().appended, finished_bytes);
        drop(observation);
        f.command.id = "fresh-expired".into();
        f.body.cmd_id = f.command.id.clone();
        f.refused(expired_destination(), Refusal::Expired).await;
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn revocation_fences_replay_even_with_current_generation() {
    run(async {
        let mut f = Fixture::new(false, Some(1), false).await;
        claimed(f.submit(destination()).await, 1);
        f.generation = gateway::claim_revoke(&f.store).unwrap().generation;
        f.refused(destination(), Refusal::Revoked).await;
        f.owner.shutdown().await.unwrap();
    });
}

#[test]
fn admission_and_replay_audit_keep_verified_receipt_context() {
    let logs = crate::logging::capture_logs(|| {
        run(async {
            let mut f = Fixture::new(false, Some(1), false).await;
            f.command.args["private"] = json!("receipt-test-sensitive");
            f.body.args_digest = args_digest(&f.command.args);
            let (first, _) = f.submit(destination()).await;
            let journal::Admission::Granted { consumption, .. } = first else {
                panic!("receipt")
            };
            let binding = consumption.binding.as_ref().unwrap();
            assert_eq!(binding.home, f.body.home_node_id);
            assert_eq!(binding.principal, f.body.principal);
            assert_eq!(binding.cmd_id, f.body.cmd_id);
            assert_eq!(binding.args_digest, f.body.args_digest);
            assert_eq!(binding.verb, f.body.verb);
            assert_eq!(binding.generation, f.generation);
            assert_eq!(consumption.destination, f.body.destination_node_id);
            assert_eq!(consumption.target, "agent_atlas_1");
            assert_eq!(consumption.ordinal, 1);
            let issuance = consumption.issuance.clone();
            let (retry, grant) = f.submit(destination()).await;
            assert!(grant.is_none());
            assert!(
                matches!(retry, journal::Admission::Granted { consumption: replay, .. } if replay == consumption)
            );
            let raw = std::fs::read_to_string(f._root.path().join("wire/commands.jsonl")).unwrap();
            assert!(!raw.contains("receipt-test-sensitive"));
            let row: Value = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
            assert_eq!(row["version"], 4);
            assert_eq!(row["consumption"]["issuance"], issuance);
            assert_eq!(
                row["consumption"]["binding"],
                serde_json::to_value(binding).unwrap()
            );
            f.owner.shutdown().await.unwrap();
        })
    });
    assert!(!logs.contains("receipt-test-sensitive"));
    let lines: Vec<_> = logs
        .lines()
        .filter(|line| line.contains("wire.lifecycle.admission"))
        .collect();
    assert_eq!(lines.len(), 2, "{logs}");
    for line in &lines {
        for field in [
            "identity=\"verified\"",
            "home=",
            "principal=\"client-1\"",
            "issuance=Some(",
            "use_ordinal=Some(1)",
            "generation=",
            "verb=\"agent.stop\"",
            "target=\"agent_atlas_1\"",
            "cmd_id=\"verified-command\"",
        ] {
            assert!(line.contains(field), "missing {field}: {line}");
        }
    }
    assert!(lines[0].contains("outcome=\"admitted\""));
    assert!(lines[1].contains("outcome=\"replay\""));
    assert!(lines[1].contains("detail=\"pending\""));
}

#[test]
fn unauthenticated_refusals_are_never_verified_and_have_no_issuance() {
    let logs = crate::logging::capture_logs(|| {
        run(async {
            let f = Fixture::new(false, Some(1), false).await;
            let observation = f.owner.observe_writes_test();
            let mut frame = serde_json::to_value(f.frame()).unwrap();
            frame["envelope"]["signature"][0] =
                json!(frame["envelope"]["signature"][0].as_u64().unwrap() ^ 1);
            let (result, grant) = f
                .submit_frame(
                    serde_json::from_value(frame).unwrap(),
                    f.command.clone(),
                    destination(),
                )
                .await;
            assert!(matches!(
                result,
                journal::Admission::Refused(Refusal::Provenance)
            ));
            assert!(grant.is_none());
            assert_eq!(observation.sample().appended, 0);
            f.owner.shutdown().await.unwrap();
        })
    });
    let line = logs
        .lines()
        .find(|line| line.contains("wire.lifecycle.admission"))
        .unwrap();
    for field in [
        "identity=\"UNVERIFIED\"",
        "issuance=None",
        "use_ordinal=None",
        "outcome=\"refused\"",
        "detail=\"Provenance\"",
    ] {
        assert!(line.contains(field), "missing {field}: {line}");
    }
}

#[test]
fn authenticated_no_grant_and_changed_payload_are_audited_without_a_use() {
    let logs = crate::logging::capture_logs(|| {
        run(async {
            let mut f = Fixture::new(false, Some(1), false).await;
            claimed(f.submit(destination()).await, 1);
            let original = f.command.id.clone();
            f.command.args["extra"] = json!(true);
            f.body.args_digest = args_digest(&f.command.args);
            let ingress = Ingress {
                frame: f.frame(),
                since: Some(Duration::ZERO),
                command: f.command.clone(),
            };
            assert!(matches!(
                admit(
                    &f.owner,
                    f.owner.gate().load(Ordering::Acquire),
                    ingress,
                    f.store.clone(),
                    f.generation,
                    destination()
                )
                .await,
                Err(ErrorCode::InvalidParams)
            ));
            f.command.id = "no-grant-left".into();
            f.body.cmd_id = f.command.id.clone();
            f.refused(destination(), Refusal::Exhausted).await;
            assert_eq!(original, "verified-command");
            f.owner.shutdown().await.unwrap();
        })
    });
    let refused: Vec<_> = logs
        .lines()
        .filter(|line| {
            line.contains("wire.lifecycle.admission") && line.contains("outcome=\"refused\"")
        })
        .collect();
    assert_eq!(refused.len(), 2, "{logs}");
    assert!(
        refused.iter().any(|line| line.contains("invalid_params")),
        "{logs}"
    );
    assert!(
        refused.iter().any(|line| line.contains("Exhausted")),
        "{logs}"
    );
    for line in refused {
        assert!(
            line.contains("identity=\"verified\"") && line.contains("issuance=None"),
            "{line}"
        );
    }
}
