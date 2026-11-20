use super::*;
use serde_json::json;
fn setup() -> (crate::structured::launch::SessionConfigDir, Journal) {
    let root = crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let mut journal = Journal::new(root.path().to_owned());
    journal.principal_limit = 2;
    journal.global_limit = 3;
    (root, journal)
}
fn command(id: &str) -> Command {
    Command {
        id: id.into(),
        verb: "agent.decide".into(),
        args: json!({"agent":"agent-1","decision":"deny"}),
        resume_only: false,
        read_only: false,
    }
}
#[test]
fn restart_replays_first_ack_and_pending_or_lost_claim_never_readmits() {
    let (root, journal) = setup();
    let mut cmd = command("one");
    assert!(matches!(
        journal.claim("p", &cmd, 100).unwrap(),
        Admission::Claimed(_)
    ));
    let restarted = Journal::new(root.path().to_owned());
    cmd.resume_only = true;
    assert!(matches!(
        restarted.claim("p", &cmd, 101).unwrap(),
        Admission::Pending(_)
    ));
    let first = journal.finish(
        "p",
        "one",
        100,
        Ok(json!({"request_id":"request-1","decision":"deny"})),
    );
    assert!(
        matches!(restarted.claim("p", &cmd, 102).unwrap(), Admission::Replayed(frame) if frame == first)
    );
    fs::remove_file(root.path().join("wire/commands.jsonl")).unwrap();
    assert_eq!(
        restarted.claim("p", &cmd, 103).err(),
        Some(ErrorCode::SessionUnconfirmed)
    );
}
#[test]
fn payload_target_and_verb_are_immutable_but_delivery_flag_is_not_hashed() {
    let (_root, journal) = setup();
    let cmd = command("one");
    journal.claim("p", &cmd, 100).unwrap();
    let first = journal.finish("p", "one", 100, Err(ErrorCode::InvalidParams));
    let mut retry = command("one");
    retry.resume_only = true;
    assert!(
        matches!(journal.claim("p", &retry, 101).unwrap(), Admission::Replayed(frame) if frame == first)
    );
    retry.args["agent"] = json!("agent-2");
    assert_eq!(
        journal.claim("p", &retry, 101).err(),
        Some(ErrorCode::InvalidParams)
    );
    retry.args = cmd.args;
    retry.verb = "agent.stop".into();
    assert_eq!(
        journal.claim("p", &retry, 101).err(),
        Some(ErrorCode::InvalidParams)
    );
}
#[test]
fn per_principal_and_global_caps_refuse_without_evicting_unexpired_claims() {
    let (root, journal) = setup();
    for id in ["one", "two"] {
        journal.claim("p", &command(id), 100).unwrap();
    }
    let before = fs::read(root.path().join("wire/commands.jsonl")).unwrap();
    assert_eq!(
        journal.claim("p", &command("three"), 101).err(),
        Some(ErrorCode::SessionBusy)
    );
    assert_eq!(
        before,
        fs::read(root.path().join("wire/commands.jsonl")).unwrap()
    );
    journal.claim("q", &command("three"), 102).unwrap();
    assert_eq!(
        journal.claim("r", &command("four"), 103).err(),
        Some(ErrorCode::SessionBusy)
    );
    for id in ["one", "two"] {
        assert!(matches!(
            journal.claim("p", &command(id), 104).unwrap(),
            Admission::Pending(_)
        ));
    }
    assert_eq!(journal.load().unwrap().0.len(), 3);
}
#[test]
fn expiry_compacts_only_after_full_window_with_injected_time() {
    let (_root, journal) = setup();
    journal.claim("p", &command("one"), 100).unwrap();
    journal.claim("p", &command("two"), 101).unwrap();
    let expiry = 100 + super::super::super::IDEMPOTENCY_WINDOW_MS;
    assert_eq!(
        journal.claim("p", &command("three"), expiry - 1).err(),
        Some(ErrorCode::SessionBusy)
    );
    journal.claim("p", &command("three"), expiry).unwrap();
    let records = journal.load().unwrap().0;
    assert_eq!(records.len(), 2);
    assert!(!records.values().any(|r| r.cmd_id == "one"));
    assert!(records.values().any(|r| r.cmd_id == "two"));
    let mut retry = command("one");
    retry.resume_only = true;
    assert_eq!(
        journal.claim("p", &retry, expiry).err(),
        Some(ErrorCode::SessionUnconfirmed)
    );
}
#[test]
fn torn_tail_is_ignored_preserved_and_does_not_hide_previous_ack() {
    let (root, journal) = setup();
    journal.claim("p", &command("one"), 100).unwrap();
    let first = journal.finish("p", "one", 100, Ok(json!({"ok":true})));
    let path = root.path().join("wire/commands.jsonl");
    files::open_append_file(&path)
        .unwrap()
        .write_all(b"{\"outcome\":")
        .unwrap();
    assert!(
        matches!(journal.claim("p", &command("one"), 101).unwrap(), Admission::Replayed(frame) if frame == first)
    );
    journal.claim("q", &command("two"), 102).unwrap();
    let (_, raw, opaque) = journal.load().unwrap();
    assert!(!opaque);
    assert_eq!(
        serde_json::from_slice::<Value>(&raw[0]).unwrap()["torn_tail"],
        json!(b"{\"outcome\":".to_vec())
    );
}
#[test]
fn unknown_entry_is_skipped_preserved_and_does_not_hide_valid_ack() {
    let (root, journal) = setup();
    journal.claim("p", &command("one"), 100).unwrap();
    let first = journal.finish("p", "one", 100, Ok(json!({"ok":true})));
    let path = root.path().join("wire/commands.jsonl");
    files::open_append_file(&path)
        .unwrap()
        .write_all(b"{\"version\":99}\n")
        .unwrap();
    let before = fs::read(&path).unwrap();
    assert!(
        matches!(journal.claim("p", &command("one"), 101).unwrap(), Admission::Replayed(frame) if frame == first)
    );
    assert_eq!(
        journal.claim("q", &command("two"), 102).err(),
        Some(ErrorCode::SessionBusy)
    );
    assert_eq!(before, fs::read(path).unwrap());
}
#[test]
fn failed_claim_write_and_oversized_result_cannot_produce_success() {
    let (root, journal) = setup();
    files::ensure_private_dir(&root.path().join("wire")).unwrap();
    fs::create_dir(root.path().join("wire/commands.tmp")).unwrap();
    assert_eq!(
        journal.claim("p", &command("one"), 100).err(),
        Some(ErrorCode::SessionBusy)
    );
    assert!(!root.path().join("wire/commands.jsonl").exists());
    fs::remove_dir(root.path().join("wire/commands.tmp")).unwrap();
    journal.claim("p", &command("one"), 100).unwrap();
    let frame = journal.finish("p", "one", 100, Ok(json!("x".repeat(ENTRY_BYTES))));
    assert!(
        matches!(frame, ServerFrame::Ack {error:Some(error), ..} if error.code == "session_unconfirmed")
    );
    assert!(matches!(
        journal.claim("p", &command("one"), 101).unwrap(),
        Admission::Pending(_)
    ));
}

#[test]
fn late_completion_cannot_overwrite_a_new_claim_after_expiry() {
    let (_root, journal) = setup();
    journal.claim("p", &command("one"), 100).unwrap();
    let next = 100 + super::super::super::IDEMPOTENCY_WINDOW_MS;
    journal.claim("p", &command("one"), next).unwrap();
    let stale = journal.finish("p", "one", 100, Ok(json!({"old":true})));
    assert!(
        matches!(stale, ServerFrame::Ack {error:Some(error), ..} if error.code == "session_unconfirmed")
    );
    assert!(
        matches!(journal.claim("p", &command("one"), next).unwrap(), Admission::Pending(stamp) if stamp == next)
    );
}

#[test]
fn invalid_outcome_is_skipped_without_hiding_other_claims() {
    let (root, journal) = setup();
    journal.claim("p", &command("one"), 100).unwrap();
    journal.claim("q", &command("two"), 100).unwrap();
    let path = root.path().join("wire/commands.jsonl");
    let text = fs::read_to_string(&path).unwrap();
    let mut lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    lines[0]["outcome"] = json!({"t":"ping","nonce":"invalid-outcome"});
    fs::write(
        &path,
        lines.iter().map(|v| format!("{v}\n")).collect::<String>(),
    )
    .unwrap();
    let (_, raw, opaque) = journal.load().unwrap();
    assert!(opaque);
    assert_eq!(raw.len(), 1);
    assert!(matches!(
        journal.claim("q", &command("two"), 101).unwrap(),
        Admission::Pending(_)
    ));
    let mut retry = command("one");
    retry.resume_only = true;
    assert_eq!(
        journal.claim("p", &retry, 101).err(),
        Some(ErrorCode::SessionUnconfirmed)
    );
}
