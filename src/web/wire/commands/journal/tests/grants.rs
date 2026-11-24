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

pub(super) fn request(root: &std::path::Path, generation: u64, issuance: &str, id: &str) -> Claim {
    request_from(
        Arc::new(Store::open(root).unwrap()),
        generation,
        issuance,
        id,
    )
}
pub(super) fn request_from(store: Arc<Store>, generation: u64, issuance: &str, id: &str) -> Claim {
    Claim {
        store,
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
        environment: Box::new(|| Ok((None, Some("w1".into())))),
    }
}
pub(super) fn claimed(result: Result<Admission, ErrorCode>) -> Consumption {
    match result.unwrap() {
        Admission::Granted { consumption, state } => {
            assert!(matches!(*state, Admission::Claimed(100)));
            consumption
        }
        _ => panic!("expected a durable new claim"),
    }
}
fn refused(result: Result<Admission, ErrorCode>, expected: Refusal) {
    assert!(
        matches!(result, Ok(Admission::Refused(reason) | Admission::RefusedReplay { reason, .. }) if reason == expected)
    );
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
fn compaction_retains_grant_totals_and_outcomes_beyond_replay_window() {
    for limit in [Some(1), Some(3)] {
        let (root, _store, generation, issuance) = gateway::claim_fixture_limit(limit);
        let mut journal = Journal::open(root.path()).unwrap();
        let mut outcomes = Vec::new();
        for ordinal in 1..=limit.unwrap() {
            let id = ordinal.to_string();
            claimed(journal.grant(request(root.path(), generation, &issuance, &id)));
            outcomes.push(
                journal
                    .finish(
                        &("a".repeat(64), "client-1".into(), id),
                        100,
                        Ok(json!({"use":ordinal})),
                    )
                    .unwrap(),
            );
        }
        journal
            .claim("local", "ordinary", &super::command("expired"), 100)
            .unwrap();
        let now = 101 + super::super::super::super::IDEMPOTENCY_WINDOW_MS;
        for _ in 0..2 {
            let rx = super::compaction::start(&mut journal, now);
            super::compaction::finish_compaction(&mut journal, &rx);
            assert!(journal.index.records.is_empty());
            drop(journal);
            journal = Journal::open(root.path()).unwrap();
            assert!(!journal.blocked);
            assert_eq!(journal.index.consumed[&issuance], limit.unwrap());
            for index in 0..outcomes.len() {
                let mut retry =
                    request(root.path(), generation, &issuance, &(index + 1).to_string());
                retry.now = now;
                retry.command.resume_only = true;
                assert!(matches!(
                    journal.grant(retry),
                    Err(ErrorCode::SessionUnconfirmed)
                ));
            }
            let mut next = request(root.path(), generation, &issuance, "next");
            next.now = now;
            refused(journal.grant(next), Refusal::Exhausted);
        }
    }
}

#[test]
fn retained_grants_do_not_occupy_local_capacity() {
    let (root, _store, generation, issuance) = gateway::claim_fixture_limit(None);
    let mut journal = Journal::open(root.path()).unwrap();
    journal.principal_limit = 2;
    journal.global_limit = 2;
    for id in ["one", "two", "three"] {
        claimed(journal.grant(request(root.path(), generation, &issuance, id)));
    }
    for id in ["operator-one", "operator-two"] {
        assert!(matches!(
            journal
                .claim("local", "p", &super::command(id), 100)
                .unwrap(),
            Admission::Claimed(100)
        ));
    }
    assert!(matches!(
        journal.claim("local", "p", &super::command("full"), 100),
        Err(ErrorCode::SessionBusy)
    ));
}

#[test]
fn grant_crash_child() {
    let Ok(root) = std::env::var("FLOCK_GRANT_CRASH_ROOT") else {
        return;
    };
    let root = std::path::Path::new(&root);
    let stage = std::env::var("FLOCK_GRANT_CRASH_STAGE").unwrap();
    let generation = std::env::var("FLOCK_GRANT_CRASH_GENERATION")
        .unwrap()
        .parse()
        .unwrap();
    let issuance = std::env::var("FLOCK_GRANT_CRASH_ISSUANCE").unwrap();
    let mut journal = Journal::open(root).unwrap();
    if stage == "claim" {
        journal.claim_checkpoint = Some(Box::new(|committed| {
            if committed {
                std::process::exit(crate::cli::exit::SUCCESS);
            }
        }));
    }
    claimed(journal.apply(Operation::Grant(Box::new(request(
        root, generation, &issuance, "one",
    )))));
    // A synced external effect lets the parent distinguish dispatch from admission.
    let mut effect = super::super::private(&root.join("wire/effect")).unwrap();
    use std::io::Write;
    effect.write_all(b"x").unwrap();
    effect.sync_all().unwrap();
    if stage == "dispatch" {
        std::process::exit(crate::cli::exit::SUCCESS);
    }
    let key = ("a".repeat(64), "client-1".into(), "one".into());
    if stage == "result_failed" {
        journal.fault = Some(Fault::Sync);
        assert!(journal.finish(&key, 100, Ok(json!({"done":true}))).is_err());
    } else {
        assert_eq!(stage, "result_synced");
        journal.finish(&key, 100, Ok(json!({"done":true}))).unwrap();
    }
    // Exit without unwinding or acknowledging the claim or result.
    std::process::exit(crate::cli::exit::SUCCESS);
}

#[test]
fn crash_at_claim_dispatch_and_result_never_redispatches_after_recovery() {
    for stage in ["claim", "dispatch", "result_failed", "result_synced"] {
        let (root, _store, generation, issuance) = gateway::claim_fixture();
        let mut child =
            crate::process::TracedCommand::new(std::env::current_exe().unwrap(), "journal-test");
        child
            .args([
                "--exact",
                "web::wire::commands::journal::tests::grants::grant_crash_child",
                "--nocapture",
            ])
            .env("FLOCK_GRANT_CRASH_ROOT", root.path())
            .env("FLOCK_GRANT_CRASH_STAGE", stage)
            .env("FLOCK_GRANT_CRASH_GENERATION", generation.to_string())
            .env("FLOCK_GRANT_CRASH_ISSUANCE", &issuance)
            .env_remove("FLOCK_SOCKET_PATH")
            .env_remove("FLOCK_PANE_ID")
            .env_remove("FLOCK_AGENT_ID");
        let mut child = child.spawn_traced().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("crash child never reached {stage}");
            }
            std::thread::yield_now();
        };
        assert_eq!(status.code(), Some(crate::cli::exit::SUCCESS), "{stage}");
        let effect = super::super::filesystem::fs::read(root.path().join("wire/effect"));
        if stage == "claim" {
            assert_eq!(effect.unwrap_err().kind(), std::io::ErrorKind::NotFound);
        } else {
            assert_eq!(effect.unwrap(), b"x");
        }
        let mut journal = Journal::open(root.path()).unwrap();
        let now = 101;
        let rx = super::compaction::start(&mut journal, now);
        super::compaction::finish_compaction(&mut journal, &rx);
        drop(journal);
        let mut journal = Journal::open(root.path()).unwrap();
        let mut retry = request(root.path(), generation, &issuance, "one");
        retry.now = now;
        retry.command.resume_only = true;
        match journal.apply(Operation::Grant(Box::new(retry))).unwrap() {
            Admission::Granted { state, .. } => match *state {
                Admission::Pending(100) => assert_ne!(stage, "result_synced"),
                Admission::Replayed(frame) => {
                    assert_eq!(stage, "result_synced");
                    assert_eq!(
                        frame,
                        super::super::ack("one".into(), Ok(json!({"done":true})))
                    );
                }
                _ => panic!("recovered command must never dispatch again"),
            },
            _ => panic!("missing recovered consumption"),
        }
        assert_eq!(journal.index.consumed[&issuance], 1);
        refused(
            journal.grant(request(root.path(), generation, &issuance, "other")),
            Refusal::Exhausted,
        );
    }
}

#[test]
fn missing_authoritative_log_refuses_restart_without_recreating_uses() {
    let (root, _store, generation, issuance) = gateway::claim_fixture();
    let mut journal = Journal::open(root.path()).unwrap();
    claimed(journal.grant(request(root.path(), generation, &issuance, "one")));
    drop(journal);
    let path = root.path().join("wire/commands.jsonl");
    super::super::filesystem::fs::remove_file(&path).unwrap();
    let mut journal = Journal::open(root.path()).unwrap();
    refused(
        journal.grant(request(root.path(), generation, &issuance, "two")),
        Refusal::AccountingLost,
    );
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
    journal
        .claim("local", "ordinary", &super::command("expired"), 100)
        .unwrap();
    journal.next_compaction = 0;
    let (tx, rx) = mpsc::sync_channel(1);
    let now = 101 + super::super::super::super::IDEMPOTENCY_WINDOW_MS;
    journal.start_compaction(now, tx);
    let result = match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
        super::super::Message::Compacted(result) => result,
        _ => panic!("unexpected compaction message"),
    };
    let mut c = request(root.path(), generation, &issuance, "one");
    c.now = now;
    assert!(
        matches!(journal.grant(c).unwrap(), Admission::Granted {state, ..} if matches!(*state, Admission::Claimed(_)))
    );
    let outcome = journal
        .finish(
            &("a".repeat(64), "client-1".into(), "one".into()),
            now,
            Ok(json!({"first": true})),
        )
        .unwrap();
    journal.compacted(result);
    assert!(journal.compaction.is_none());
    assert_eq!(journal.index.records.len(), 1);
    assert_eq!(journal.index.consumed[&issuance], 1);
    drop(journal);
    let mut journal = Journal::open(root.path()).unwrap();
    assert_eq!(journal.index.consumed[&issuance], 1);
    let mut retry = request(root.path(), generation, &issuance, "one");
    retry.now = now;
    assert!(
        matches!(journal.grant(retry).unwrap(), Admission::Granted {state, ..} if matches!(*state, Admission::Replayed(ref frame) if frame == &outcome))
    );
}

fn clock_request(
    root: &std::path::Path,
    generation: u64,
    issuance: &str,
    id: &str,
    wall: u64,
    elapsed: u64,
) -> Claim {
    let mut c = request(root, generation, issuance, id);
    c.environment = Box::new(move || {
        Ok((
            Some(crate::api::gateway_grants::ClockPair {
                domain: "fixture-boot".into(),
                wall_ms: wall,
                elapsed_ms: elapsed,
            }),
            Some("w1".into()),
        ))
    });
    c
}

#[test]
fn backward_step_latches_before_claim_and_survives_restart() {
    let (root, _store, generation, issuance) = gateway::claim_timed_fixture(10000, 20000);
    let mut j = Journal::open(root.path()).unwrap();
    claimed(j.grant(clock_request(
        root.path(),
        generation,
        &issuance,
        "first",
        11000,
        21000,
    )));
    // Still within the issuance drift bound, but behind the durable admission.
    refused(
        j.grant(clock_request(
            root.path(),
            generation,
            &issuance,
            "step",
            10900,
            21100,
        )),
        Refusal::Expired,
    );
    assert_eq!(j.index.consumed[&issuance], 1);
    drop(j);
    let mut j = Journal::open(root.path()).unwrap();
    refused(
        j.grant(clock_request(
            root.path(),
            generation,
            &issuance,
            "restored",
            12000,
            22000,
        )),
        Refusal::Expired,
    );
    assert_eq!(j.index.consumed[&issuance], 1);
}

#[test]
fn forward_discontinuity_latches_other_timed_grants() {
    let (root, _store, generation, issuance) = gateway::claim_timed_fixture(10000, 20000);
    let mut j = Journal::open(root.path()).unwrap();
    refused(
        j.grant(clock_request(
            root.path(),
            generation,
            &issuance,
            "jump",
            13001,
            21000,
        )),
        Refusal::Expired,
    );
    refused(
        j.grant(clock_request(
            root.path(),
            generation,
            &issuance,
            "restored",
            12000,
            22000,
        )),
        Refusal::Expired,
    );
    assert!(j.index.records.is_empty());
}

#[test]
fn cumulative_drift_latches_even_when_each_increment_is_small() {
    let (root, _store, generation, issuance) = gateway::claim_timed_fixture(10000, 20000);
    let mut j = Journal::open(root.path()).unwrap();
    claimed(j.grant(clock_request(
        root.path(),
        generation,
        &issuance,
        "first",
        10600,
        21000,
    )));
    claimed(j.grant(clock_request(
        root.path(),
        generation,
        &issuance,
        "second",
        11200,
        22000,
    )));
    refused(
        j.grant(clock_request(
            root.path(),
            generation,
            &issuance,
            "drift",
            11800,
            23000,
        )),
        Refusal::Expired,
    );
    refused(
        j.grant(clock_request(
            root.path(),
            generation,
            &issuance,
            "restored",
            14000,
            24000,
        )),
        Refusal::Expired,
    );
    assert_eq!(j.index.consumed[&issuance], 2);
}

#[test]
fn destination_offsets_and_suspend_never_extend_lifetime() {
    for wall in [10000, 1_000_000] {
        let (root, _store, generation, issuance) = gateway::claim_timed_fixture(wall, 20000);
        let mut j = Journal::open(root.path()).unwrap();
        claimed(j.grant(clock_request(
            root.path(),
            generation,
            &issuance,
            "valid",
            wall + 1000,
            21000,
        )));
        // Inject suspend-inclusive progress across expiry, with a behind wall clock.
        refused(
            j.grant(clock_request(
                root.path(),
                generation,
                &issuance,
                "suspended",
                wall + 9000,
                30000,
            )),
            Refusal::Expired,
        );
        assert_eq!(j.index.consumed[&issuance], 1);
    }
}

fn acknowledge(store: &Store, wall: u64, elapsed: u64) {
    gateway::claim_acknowledge(
        store,
        crate::api::gateway_grants::ClockPair {
            domain: "fixture-boot".into(),
            wall_ms: wall,
            elapsed_ms: elapsed,
        },
    );
}

#[test]
fn acknowledgement_rebases_comparisons_without_reviving_expiry() {
    let (root, store, generation, issuance) = gateway::claim_timed_fixture(10000, 20000);
    let mut j = Journal::open(root.path()).unwrap();
    refused(
        j.grant(clock_request(
            root.path(),
            generation,
            &issuance,
            "jump",
            12500,
            21000,
        )),
        Refusal::Expired,
    );
    acknowledge(&store, 12500, 21000);
    claimed(j.grant(clock_request(
        root.path(),
        generation,
        &issuance,
        "rebased",
        13000,
        21500,
    )));
    refused(
        j.grant(clock_request(
            root.path(),
            generation,
            &issuance,
            "expired",
            21000,
            22000,
        )),
        Refusal::Expired,
    );
    acknowledge(&store, 14000, 23000);
    refused(
        j.grant(clock_request(
            root.path(),
            generation,
            &issuance,
            "cannot-revive",
            14500,
            23500,
        )),
        Refusal::Expired,
    );
    assert_eq!(j.index.consumed[&issuance], 1);
}

#[test]
fn lost_boot_evidence_stays_refused_after_acknowledgement() {
    let (root, store, generation, issuance) = gateway::claim_timed_fixture(10000, 20000);
    let mut j = Journal::open(root.path()).unwrap();
    let mut c = clock_request(root.path(), generation, &issuance, "boot", 11000, 21000);
    c.environment = Box::new(|| {
        Ok((
            Some(crate::api::gateway_grants::ClockPair {
                domain: "other-fixture-boot".into(),
                wall_ms: 11000,
                elapsed_ms: 1000,
            }),
            Some("w1".into()),
        ))
    });
    refused(j.grant(c), Refusal::Expired);
    drop(j);
    let mut j = Journal::open(root.path()).unwrap();
    acknowledge(&store, 12000, 22000);
    refused(
        j.grant(clock_request(
            root.path(),
            generation,
            &issuance,
            "lost",
            13000,
            23000,
        )),
        Refusal::Expired,
    );
    assert!(j.index.records.is_empty());
}

#[test]
fn expired_after_preflight_does_not_spend_once_or_claim() {
    let (root, store, generation, issuance) =
        gateway::claim_timed_fixture_limit(10000, 20000, Some(1));
    let mut j = Journal::open(root.path()).unwrap();
    let c = clock_request(root.path(), generation, &issuance, "expired", 20000, 30000);
    let request = gateway::Request {
        home: &c.home,
        principal: &c.principal,
        scopes: &c.scopes,
        verb: &c.command.verb,
        target: gateway::Target::Agent(&c.target),
    };
    assert!(matches!(
        store
            .evaluate(
                &gateway::lifecycle::Evaluation {
                    request: &request,
                    source: PrincipalKind::User,
                    destination: &c.destination,
                    clock: Some(crate::api::gateway_grants::ClockPair {
                        domain: "fixture-boot".into(),
                        wall_ms: 18000,
                        elapsed_ms: 28000,
                    }),
                    consumed: &j.index.consumed,
                },
                generation,
                |_| Some("w1".into())
            )
            .unwrap(),
        gateway::lifecycle::Decision::Allowed { .. }
    ));
    refused(j.grant(c), Refusal::Expired);
    assert!(j.index.records.is_empty());
    assert!(!j.index.consumed.contains_key(&issuance));
}

#[test]
fn latched_clock_leaves_untimed_and_local_claims_working() {
    let (root, store, generation, issuance) = gateway::claim_timed_fixture(10000, 20000);
    let mut j = Journal::open(root.path()).unwrap();
    refused(
        j.grant(clock_request(
            root.path(),
            generation,
            &issuance,
            "jump",
            15000,
            21000,
        )),
        Refusal::Expired,
    );
    let (generation, untimed) = gateway::claim_install(&store, "untimed", Some(1));
    claimed(j.grant(request(root.path(), generation, &untimed, "untimed")));
    assert!(matches!(
        j.claim("local", "operator", &super::command("local"), 100),
        Ok(Admission::Claimed(100))
    ));
    refused(
        j.grant(clock_request(
            root.path(),
            generation,
            &issuance,
            "still-latched",
            12000,
            22000,
        )),
        Refusal::Expired,
    );
}

#[test]
fn admission_pair_survives_outcome_and_compacted_checkpoint() {
    let (root, _store, generation, issuance) = gateway::claim_timed_fixture(10000, 20000);
    let mut j = Journal::open(root.path()).unwrap();
    let consumption = claimed(j.grant(clock_request(
        root.path(),
        generation,
        &issuance,
        "paired",
        11000,
        21000,
    )));
    let clock = consumption.clock.as_ref().unwrap();
    assert_eq!((clock.wall_ms, clock.elapsed_ms), (11000, 21000));
    j.finish(
        &("a".repeat(64), "client-1".into(), "paired".into()),
        100,
        Ok(json!({})),
    )
    .unwrap();
    let now = 101 + super::super::super::super::IDEMPOTENCY_WINDOW_MS;
    let rx = super::compaction::start(&mut j, now);
    super::compaction::finish_compaction(&mut j, &rx);
    drop(j);
    let j = Journal::open(root.path()).unwrap();
    assert!(!j.blocked);
    assert_eq!(j.index.consumed[&issuance], 1);
    let raw = super::super::filesystem::fs::read(root.path().join("wire/commands.jsonl")).unwrap();
    let record: super::super::Record =
        serde_json::from_slice(raw.split(|b| *b == b'\n').next().unwrap()).unwrap();
    assert_eq!(record.version, 3);
    assert_eq!(record.consumption.unwrap(), consumption);
}

#[test]
fn pending_accounting_ahead_of_journal_refuses_without_use() {
    let (root, store, generation, issuance) = gateway::claim_fixture();
    let mut journal = Journal::open(root.path()).unwrap();
    gateway::reserve_ahead_fixture(&store, &issuance);
    refused(
        journal.grant(request(root.path(), generation, &issuance, "pending-ahead")),
        Refusal::AccountingLost,
    );
    assert!(journal.index.records.is_empty());
    assert!(journal.index.consumed.is_empty());
}

#[test]
fn fix1_stalled_selection_has_a_structural_bound_and_spends_nothing() {
    let (root, _store, generation, issuance) = gateway::claim_fixture();
    let data = root.path().to_owned();
    let c = request(root.path(), generation, &issuance, "stalled");
    let (send, receive) = mpsc::channel();
    drop(std::thread::spawn(move || {
        let mut journal = Journal::open(&data).unwrap();
        journal.fault = Some(Fault::SelectionStalled);
        let result = journal.grant(c);
        send.send((
            result,
            journal.index.records.len(),
            journal.index.consumed.len(),
        ))
        .unwrap();
    }));
    let (result, records, consumed) = receive
        .recv_timeout(Duration::from_secs(5))
        .expect("selection producer died or failed to return within its structural bound");
    refused(result, Refusal::SelectionLimit);
    assert_eq!((records, consumed), (0, 0));
}

#[test]
fn single_implicitly_selected_latched_grant_reports_expired() {
    let (root, _store, generation, issuance) = gateway::claim_timed_fixture(10000, 20000);
    let mut journal = Journal::open(root.path()).unwrap();
    let c = clock_request(root.path(), generation, &issuance, "latch", 9000, 21000);
    refused(journal.grant(c), Refusal::Expired);
    let c = clock_request(root.path(), generation, "", "implicit", 11000, 21000);
    refused(journal.grant(c), Refusal::Expired);
    assert!(journal.index.records.is_empty());
    assert!(journal.index.consumed.is_empty());
}

#[test]
fn receipts_survive_outcome_recovery_and_unknown_fields_stay_raw() {
    let (root, _store, generation, issuance) = gateway::claim_fixture();
    let mut journal = Journal::open(root.path()).unwrap();
    let original = claimed(journal.grant(request(root.path(), generation, &issuance, "bound")));
    journal
        .claim("local", "local-principal", &super::command("old"), 100)
        .unwrap();
    let key = ("a".repeat(64), "client-1".into(), "bound".into());
    let frame = journal.finish(&key, 100, Ok(json!({"effect":1}))).unwrap();
    for field in ["home", "principal", "cmd_id"] {
        let mut mismatched = serde_json::to_value(&journal.index.records[&key]).unwrap();
        mismatched["consumption"]["binding"][field] = json!("mismatched-binding");
        assert!(super::super::Record::decode(&serde_json::to_vec(&mismatched).unwrap()).is_none());
    }
    drop(journal);
    let mut journal = Journal::open(root.path()).unwrap();
    assert!(!journal.blocked);
    assert_eq!(
        journal.index.records[&key].consumption.as_ref(),
        Some(&original)
    );
    assert!(journal
        .index
        .records
        .values()
        .any(|r| r.version == 1 && r.consumption.is_none()));
    assert!(
        matches!(journal.grant(request(root.path(), generation, &issuance, "bound")).unwrap(), Admission::Granted { consumption, state } if consumption == original && matches!(*state, Admission::Replayed(ref result) if result == &frame))
    );
    assert_eq!(journal.index.consumed[&issuance], 1);
    let path = root.path().join("wire/commands.jsonl");
    drop(journal);
    let raw = super::super::filesystem::fs::read_to_string(&path).unwrap();
    let mut rows: Vec<serde_json::Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let legacy: String = rows
        .iter()
        .cloned()
        .map(|mut row| {
            if row["cmd_id"] == "bound" {
                row["version"] = json!(2);
                row["consumption"]
                    .as_object_mut()
                    .unwrap()
                    .remove("binding");
            }
            format!("{row}\n")
        })
        .collect();
    super::super::filesystem::fs::write(&path, &legacy).unwrap();
    let legacy_journal = Journal::open(root.path()).unwrap();
    assert!(!legacy_journal.blocked);
    assert_eq!(legacy_journal.index.consumed[&issuance], 1);
    let old = legacy_journal.index.records[&key]
        .consumption
        .as_ref()
        .unwrap();
    assert!(old.binding.is_none());
    assert_eq!(old.issuance, original.issuance);
    assert_eq!(old.ordinal, original.ordinal);
    assert_eq!(
        legacy_journal.index.records[&key].outcome,
        Some(serde_json::to_value(&frame).unwrap())
    );
    let mut legacy_journal = legacy_journal;
    refused(
        legacy_journal.grant(request(root.path(), generation, &issuance, "bound")),
        Refusal::AccountingLost,
    );
    drop(legacy_journal);
    for row in &mut rows {
        if row["cmd_id"] == "bound" {
            row["consumption"]["binding"]["future"] = json!({"opaque":true});
        }
    }
    let unknown: String = rows.iter().map(|row| format!("{row}\n")).collect();
    super::super::filesystem::fs::write(&path, &unknown).unwrap();
    let journal = Journal::open(root.path()).unwrap();
    assert!(journal.blocked);
    assert!(journal.index.records.values().any(|r| r.cmd_id == "old"));
    assert!(!journal.index.records.contains_key(&key));
    assert_eq!(
        super::super::filesystem::fs::read_to_string(path).unwrap(),
        unknown
    );
}
