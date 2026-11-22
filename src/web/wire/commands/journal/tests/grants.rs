use super::super::{
    grants::{Claim, Consumption},
    Admission, Fault, Journal, Operation, Writer,
};
use crate::structured::permissions::auth::gateway;
use crate::structured::permissions::auth::{
    gateway::{
        lifecycle::{PrincipalKind, Refusal},
        Store,
    },
    tokens::Scope,
};
use crate::web::wire::commands::{Command, ErrorCode};
use serde_json::json;
use std::{
    sync::{mpsc, Arc},
    time::Duration,
};
use tokio::sync::Notify;

fn request(root: &std::path::Path, generation: u64, issuance: &str, id: &str) -> Claim {
    Claim {
        store: Store::open(root).unwrap(),
        generation,
        issuance: issuance.into(),
        home: "a".repeat(64),
        principal: "client-1".into(),
        source: PrincipalKind::User,
        scopes: [Scope::Operate].into(),
        destination: "b".repeat(64),
        target: "agent_atlas_1".into(),
        command: Command {
            id: id.into(),
            verb: "agent.stop".into(),
            args: json!({"target":"agent_atlas_1"}),
            resume_only: false,
            read_only: false,
        },
        now: 100,
        environment: Box::new(|| (None, Some("w1".into()))),
    }
}
fn claimed(result: Result<Admission, ErrorCode>) -> Consumption {
    match result.unwrap() {
        Admission::Granted { consumption, state } => {
            assert!(matches!(*state, Admission::Claimed(100)));
            consumption
        }
        _ => panic!("expected a durable new claim"),
    }
}
fn refused(result: Result<Admission, ErrorCode>, expected: Refusal) {
    assert!(matches!(result, Ok(Admission::Refused(reason)) if reason == expected));
}
#[test]
fn final_lifecycle_use_is_not_admitted_twice() {
    crate::web::wire::tests::runtime(async {
        let (root, _store, generation, issuance) = gateway::claim_fixture();
        let data = root.path().to_owned();
        let (entered, ready) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let writer = Writer::spawn(
            move || {
                let mut journal = Journal::open(&data)?;
                journal.before_sync = Some(Box::new(move || {
                    entered.send(()).unwrap();
                    resume.recv_timeout(Duration::from_secs(5)).unwrap();
                }));
                Ok(journal)
            },
            Arc::new(Notify::new()),
        )
        .await
        .unwrap();
        let one = request(root.path(), generation, &issuance, "one");
        let two = request(root.path(), generation, &issuance, "two");
        let (tx, rx) = mpsc::channel();
        let (start, go) = mpsc::channel();
        let other = writer.clone();
        let thread = std::thread::spawn(move || {
            go.recv_timeout(Duration::from_secs(5)).unwrap();
            tx.send(other.submit(Operation::Grant(Box::new(two))).unwrap())
                .unwrap();
        });
        let first = writer.submit(Operation::Grant(Box::new(one))).unwrap();
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        start.send(()).unwrap();
        let second = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        thread.join().unwrap();
        release.send(()).unwrap();
        let results = [
            tokio::time::timeout(Duration::from_secs(5), first)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            tokio::time::timeout(Duration::from_secs(5), second)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
        ];
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, Admission::Granted { .. }))
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, Admission::Refused(Refusal::Exhausted)))
                .count(),
            1
        );
        tokio::time::timeout(Duration::from_secs(5), writer.shutdown())
            .await
            .unwrap()
            .unwrap();
        let journal = Journal::open(root.path()).unwrap();
        assert_eq!(journal.index.records.len(), 1);
        assert_eq!(journal.index.consumed[&issuance], 1);
    });
}
#[test]
fn retry_is_free_but_payload_target_and_destination_are_bound() {
    let (root, _store, generation, issuance) = gateway::claim_fixture();
    let mut journal = Journal::open(root.path()).unwrap();
    let first = claimed(journal.grant(request(root.path(), generation, &issuance, "one")));
    let bytes = journal.bytes;
    let mut retry = request(
        root.path(),
        generation,
        "different-preflight-selection",
        "one",
    );
    retry.command.resume_only = true;
    match journal.grant(retry).unwrap() {
        Admission::Granted { consumption, state } => {
            assert_eq!(consumption, first);
            assert!(matches!(*state, Admission::Pending(100)));
        }
        _ => panic!("retry did not return the existing claim"),
    }
    for change in 0..4 {
        let mut c = request(root.path(), generation, &issuance, "one");
        match change {
            0 => c.command.args["reason"] = json!("changed"),
            1 => c.target = "agent_atlas_2".into(),
            2 => c.destination = "c".repeat(64),
            _ => c.command.args["target"] = json!("agent_atlas_2"),
        }
        assert!(matches!(journal.grant(c), Err(ErrorCode::InvalidParams)));
    }
    assert_eq!(journal.bytes, bytes);
    assert_eq!(journal.index.consumed[&issuance], 1);
    let key = ("a".repeat(64), "client-1".into(), "one".into());
    journal.finish(&key, 100, Ok(json!({"ok":true}))).unwrap();
    drop(journal);
    let mut journal = Journal::open(root.path()).unwrap();
    assert_eq!(journal.index.consumed[&issuance], 1);
    assert!(
        matches!(journal.grant(request(root.path(), generation, &issuance, "one")), Ok(Admission::Granted {state, ..}) if matches!(*state, Admission::Replayed(_)))
    );
}
#[test]
fn failed_append_and_precommit_drop_spend_nothing() {
    for fault in [Fault::Partial, Fault::Sync] {
        let (root, _store, generation, issuance) = gateway::claim_fixture();
        let mut journal = Journal::open(root.path()).unwrap();
        // Inject a crash before processing the prepared admission.
        drop(request(root.path(), generation, &issuance, "discarded"));
        drop(journal);
        journal = Journal::open(root.path()).unwrap();
        assert!(journal.index.consumed.is_empty());
        journal.fault = Some(fault);
        assert!(matches!(
            journal.grant(request(root.path(), generation, &issuance, "one")),
            Err(ErrorCode::SessionBusy)
        ));
        assert!(journal.index.consumed.is_empty());
        assert!(journal.index.records.is_empty());
        drop(journal);
        let mut journal = Journal::open(root.path()).unwrap();
        assert!(journal.index.consumed.is_empty());
        claimed(journal.grant(request(root.path(), generation, &issuance, "one")));
    }
}
#[test]
fn crash_after_commit_before_publication_recovers_spent_pending_claim() {
    let (root, _store, generation, issuance) = gateway::claim_fixture();
    let mut journal = Journal::open(root.path()).unwrap();
    journal.claim_checkpoint = Some(Box::new(|committed| {
        if committed {
            panic!("injected crash after durable claim");
        }
    }));
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| journal.grant(request(
            root.path(),
            generation,
            &issuance,
            "one"
        ))))
        .is_err()
    );
    assert!(journal.index.consumed.is_empty());
    drop(journal);
    let mut journal = Journal::open(root.path()).unwrap();
    assert_eq!(journal.index.consumed[&issuance], 1);
    assert!(
        matches!(journal.grant(request(root.path(), generation, &issuance, "one")), Ok(Admission::Granted {state, ..}) if matches!(*state, Admission::Pending(100)))
    );
    refused(
        journal.grant(request(root.path(), generation, &issuance, "two")),
        Refusal::Exhausted,
    );
    let mut absent = request(root.path(), generation, &issuance, "absent");
    absent.command.resume_only = true;
    assert!(!matches!(
        journal.grant(absent),
        Ok(Admission::Granted { .. })
    ));
}
#[test]
fn revoke_first_fences_claim_and_retry_without_spending() {
    for claim_first in [false, true] {
        let (root, store, generation, issuance) = gateway::claim_fixture();
        let mut journal = Journal::open(root.path()).unwrap();
        if claim_first {
            claimed(journal.grant(request(root.path(), generation, &issuance, "one")));
        }
        gateway::claim_revoke(&store).unwrap();
        for id in ["one", "two"] {
            refused(
                journal.grant(request(root.path(), generation, &issuance, id)),
                Refusal::Revoked,
            );
        }
        assert_eq!(
            journal.index.consumed.get(&issuance).copied().unwrap_or(0),
            if claim_first { 1 } else { 0 }
        );
    }
}
#[test]
fn compaction_waits_for_p3b_retention() {
    let (root, _store, generation, issuance) = gateway::claim_fixture();
    let mut journal = Journal::open(root.path()).unwrap();
    claimed(journal.grant(request(root.path(), generation, &issuance, "one")));
    let (tx, _rx) = mpsc::sync_channel(1);
    journal.start_compaction(100 + super::super::super::super::IDEMPOTENCY_WINDOW_MS, tx);
    assert!(journal.compaction.is_none());
    drop(journal);
    let journal = Journal::open(root.path()).unwrap();
    assert_eq!(journal.index.consumed[&issuance], 1);
}

#[test]
fn crash_before_append_spends_nothing_on_restart() {
    let (root, _store, generation, issuance) = gateway::claim_fixture();
    let mut journal = Journal::open(root.path()).unwrap();
    journal.claim_checkpoint = Some(Box::new(|committed| {
        assert!(!committed);
        panic!("injected crash before durable claim");
    }));
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| journal.grant(request(
            root.path(),
            generation,
            &issuance,
            "one"
        ))))
        .is_err()
    );
    drop(journal);
    let mut journal = Journal::open(root.path()).unwrap();
    assert!(journal.index.consumed.is_empty());
    assert!(journal.index.records.is_empty());
    claimed(journal.grant(request(root.path(), generation, &issuance, "one")));
}
#[test]
fn claim_holds_generation_lease_through_sync() {
    let (root, store, generation, issuance) = gateway::claim_fixture();
    let mut journal = Journal::open(root.path()).unwrap();
    let during_sync = Store::open(root.path()).unwrap();
    journal.before_sync = Some(Box::new(move || {
        assert!(matches!(
            gateway::claim_revoke(&during_sync),
            Err(ErrorCode::SessionBusy)
        ));
    }));
    claimed(journal.grant(request(root.path(), generation, &issuance, "one")));
    journal.before_sync = None;
    gateway::claim_revoke(&store).unwrap();
    refused(
        journal.grant(request(root.path(), generation, &issuance, "two")),
        Refusal::Revoked,
    );
    assert_eq!(journal.index.consumed[&issuance], 1);
}

#[test]
fn count_ordinals_and_standing_claims_survive_outcomes_and_restart() {
    for limit in [Some(3), None] {
        let (root, _store, generation, issuance) = gateway::claim_fixture_limit(limit);
        let mut journal = Journal::open(root.path()).unwrap();
        for ordinal in 1..=3 {
            let id = ordinal.to_string();
            let use_ = claimed(journal.grant(request(root.path(), generation, &issuance, &id)));
            assert_eq!(use_.ordinal, ordinal);
            journal
                .finish(&("a".repeat(64), "client-1".into(), id), 100, Ok(json!({})))
                .unwrap();
            drop(journal);
            journal = Journal::open(root.path()).unwrap();
            assert_eq!(journal.index.consumed[&issuance], ordinal);
        }
        let result = journal.grant(request(root.path(), generation, &issuance, "four"));
        if limit.is_some() {
            refused(result, Refusal::Exhausted);
        } else {
            assert_eq!(claimed(result).ordinal, 4);
        }
    }
}
#[test]
fn in_flight_compaction_cannot_publish_over_consumption() {
    let (root, _store, generation, issuance) = gateway::claim_fixture();
    let mut journal = Journal::open(root.path()).unwrap();
    journal.next_compaction = 0;
    let (tx, rx) = mpsc::sync_channel(1);
    journal.start_compaction(100, tx);
    let result = match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
        super::super::Message::Compacted(result) => result,
        _ => panic!("unexpected compaction message"),
    };
    claimed(journal.grant(request(root.path(), generation, &issuance, "one")));
    journal.compacted(result);
    assert!(journal.compaction.is_none());
    drop(journal);
    let journal = Journal::open(root.path()).unwrap();
    assert_eq!(journal.index.consumed[&issuance], 1);
}
