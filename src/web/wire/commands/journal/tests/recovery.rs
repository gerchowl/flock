use super::grants::{claimed, request};
use super::*;
use crate::structured::permissions::auth::gateway;

#[test]
fn deleting_the_wire_directory_cannot_restore_counted_authority() {
    let (root, _store, generation, issuance) = gateway::claim_fixture();
    let mut journal = Journal::open(root.path()).unwrap();
    claimed(journal.grant(request(root.path(), generation, &issuance, "one")));
    drop(journal);
    filesystem::fs::remove_dir_all(root.path().join("wire")).unwrap();
    let mut journal = Journal::open(root.path()).unwrap();
    assert!(matches!(
        journal.grant(request(root.path(), generation, &issuance, "two")),
        Ok(Admission::Refused(_))
    ));
    assert!(matches!(
        journal
            .claim("local", "operator", &command("repair"), 100)
            .unwrap(),
        Admission::Claimed(100)
    ));
}

#[test]
fn standing_history_is_bounded_over_two_months_without_operator_lockout() {
    let (root, store, generation, issuance) = gateway::claim_fixture_limit(None);
    let mut journal = Journal::open(root.path()).unwrap();
    journal.global_limit = 2;
    journal.principal_limit = 2;
    let namespace = Arc::new(gateway::Store::open(root.path()).unwrap());
    for minute in 0..(60 * 1440u64) {
        journal.file.coalesce_sync = true;
        let now = 100 + minute * 60_000;
        let mut c = super::grants::request_from(
            namespace.clone(),
            generation,
            &issuance,
            &minute.to_string(),
        );
        c.now = now;
        assert!(
            matches!(journal.grant(c).unwrap(), Admission::Granted {state, ..} if matches!(*state, Admission::Claimed(_)))
        );
        if minute % 1440 == 1439 {
            journal.file.sync_all().unwrap();
            let rx = super::compaction::start(&mut journal, now);
            super::compaction::finish_compaction(&mut journal, &rx);
            assert!(journal.index.records.len() <= 1441);
            assert!(journal.bytes < 2 * 1024 * 1024);
            assert!(matches!(
                journal
                    .claim(
                        "local",
                        "operator",
                        &command(&format!("local-{minute}")),
                        now
                    )
                    .unwrap(),
                Admission::Claimed(_)
            ));
            journal.file.sync_all().unwrap();
            drop(journal);
            journal = Journal::open(root.path()).unwrap();
            journal.global_limit = 2;
            journal.principal_limit = 2;
            assert_eq!(journal.index.consumed[&issuance], minute + 1);
        }
    }
    gateway::claim_revoke(&store).unwrap();
    let now = 100 + 90 * super::super::super::super::IDEMPOTENCY_WINDOW_MS;
    let rx = super::compaction::start(&mut journal, now);
    super::compaction::finish_compaction(&mut journal, &rx);
    drop(journal);
    let mut journal = Journal::open(root.path()).unwrap();
    journal.global_limit = 2;
    journal.principal_limit = 2;
    assert!(journal.index.consumed.is_empty());
    assert!(matches!(
        journal
            .claim("local", "operator", &command("after-revoke"), now)
            .unwrap(),
        Admission::Claimed(_)
    ));
}

#[test]
fn interrupted_first_start_does_not_lock_out_local_commands() {
    let (root, journal) = setup();
    drop(journal);
    filesystem::fs::remove_file(root.path().join("wire/commands.jsonl")).unwrap();
    let mut journal = Journal::open(root.path()).unwrap();
    assert!(matches!(
        journal
            .claim("local", "operator", &command("repair"), 100)
            .unwrap(),
        Admission::Claimed(100)
    ));
}

#[test]
fn operator_reissuance_recovers_counted_authority_while_standing_stays_available() {
    let (root, store, generation, issuance) = gateway::claim_fixture();
    let mut journal = Journal::open(root.path()).unwrap();
    claimed(journal.grant(request(root.path(), generation, &issuance, "one")));
    drop(journal);
    filesystem::fs::remove_dir_all(root.path().join("wire")).unwrap();
    let mut journal = Journal::open(root.path()).unwrap();
    let (generation, standing) = gateway::claim_install(&store, "standing", None);
    claimed(journal.grant(request(root.path(), generation, &standing, "standing-one")));
    assert!(matches!(
        journal.grant(request(root.path(), generation, &issuance, "lost")),
        Ok(Admission::Refused(
            gateway::lifecycle::Refusal::AccountingLost
        ))
    ));
    gateway::claim_revoke(&store).unwrap();
    let (generation, replacement) = gateway::claim_install(&store, "tablet", Some(1));
    assert_ne!(replacement, issuance);
    claimed(journal.grant(request(root.path(), generation, &replacement, "fresh")));
    assert!(matches!(
        journal.grant(request(root.path(), generation, &replacement, "exhausted")),
        Ok(Admission::Refused(gateway::lifecycle::Refusal::Exhausted))
    ));
}

#[test]
fn a_surviving_prefix_behind_the_a1_watermark_cannot_restore_uses() {
    let (root, _store, generation, issuance) = gateway::claim_fixture_limit(Some(3));
    let mut journal = Journal::open(root.path()).unwrap();
    claimed(journal.grant(request(root.path(), generation, &issuance, "one")));
    let path = root.path().join("wire/commands.jsonl");
    let prefix = filesystem::fs::read(&path).unwrap();
    claimed(journal.grant(request(root.path(), generation, &issuance, "two")));
    drop(journal);
    filesystem::fs::write(&path, prefix).unwrap();
    let mut journal = Journal::open(root.path()).unwrap();
    assert!(matches!(
        journal.grant(request(root.path(), generation, &issuance, "three")),
        Ok(Admission::Refused(
            gateway::lifecycle::Refusal::AccountingLost
        ))
    ));
    assert!(matches!(
        journal
            .claim("local", "operator", &command("inspect"), 100)
            .unwrap(),
        Admission::Claimed(100)
    ));
}

#[test]
fn legacy_counted_policy_requires_explicit_reissuance() {
    let (root, store, generation, issuance) = gateway::claim_fixture();
    let path = root.path().join("gateway-grants/grants.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&filesystem::fs::read(&path).unwrap()).unwrap();
    value.as_object_mut().unwrap().remove("accounting");
    filesystem::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let mut journal = Journal::open(root.path()).unwrap();
    assert!(matches!(
        journal.grant(request(root.path(), generation, &issuance, "legacy")),
        Ok(Admission::Refused(
            gateway::lifecycle::Refusal::AccountingLost
        ))
    ));
    gateway::claim_revoke(&store).unwrap();
    let (generation, replacement) = gateway::claim_install(&store, "tablet", Some(1));
    claimed(journal.grant(request(root.path(), generation, &replacement, "reissued")));
}
