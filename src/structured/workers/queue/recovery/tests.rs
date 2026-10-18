use super::*;
use crate::structured::{
    publication::{Provider, Subscription},
    workers::queue::publication::{
        tests::{journal, pages, root, sequences},
        Page,
    },
};
use std::{fs, io::Write, num::NonZeroU64};
const AGENT: &str = "publication-fixture";
fn policy() -> RewritePolicy {
    RewritePolicy::new(NonZeroU64::new(u64::MAX).unwrap())
}
#[test]
fn cold_queue_recovery_preserves_checkpoint_sequences_and_indexed_resume() {
    let root = root();
    let mut j = journal(&root, 2);
    let checkpoint = j.checkpoint().unwrap();
    let high = checkpoint.through;
    drop(j);
    let path = root.path().join("sessions/publication-fixture/queue.jsonl");
    let bytes = fs::read(&path).unwrap();
    let source = Source::open_retained(root.path(), AGENT, policy()).unwrap();
    let mut full = source.subscribe(None).unwrap();
    let replay = pages(&mut full);
    assert_eq!(sequences(&replay), [3, 4]);
    assert!(matches!(replay.last(), Some(Page::Synced(n)) if *n == high));
    assert!(matches!(full.page(), Page::End));
    let mut resumed = source.subscribe(Some(high - 1)).unwrap();
    assert_eq!(sequences(&pages(&mut resumed)), [high]);
    assert_eq!(fs::read(path).unwrap(), bytes);
    // Completion has no retained file or journal lease.
    assert!(crate::structured::queue::Journal::open(root.path(), AGENT).is_ok());
}
#[test]
fn cold_queue_recovery_missing_journal_never_creates_an_empty_source() {
    let root = root();
    let error = Source::open_retained(root.path(), "missing-fixture", policy())
        .err()
        .expect("created a missing queue");
    assert_eq!(error.code.as_str(), "agent_not_found");
    assert!(!root.path().join("sessions/missing-fixture").exists());
}
#[test]
fn cold_queue_recovery_busy_lease_is_distinct_from_io_failure() {
    let root = root();
    let _owner = journal(&root, 1);
    let error = Source::open_retained(root.path(), AGENT, policy())
        .err()
        .expect("duplicated journal ownership");
    assert_eq!(error.code, ErrorCode::SessionBusy);
    assert_eq!(error.source.unwrap().kind(), std::io::ErrorKind::WouldBlock);
}
#[test]
fn cold_queue_recovery_io_failure_is_typed_without_uncertain_stdin_write() {
    let root = root();
    drop(journal(&root, 1));
    let path = root.path().join("sessions/publication-fixture/queue.jsonl");
    let original = path.with_extension("saved");
    fs::rename(&path, &original).unwrap();
    std::os::unix::fs::symlink(&original, &path).unwrap();
    let error = Source::open_retained(root.path(), AGENT, policy())
        .err()
        .expect("followed journal link");
    assert_eq!(error.code.as_str(), "queue_recovery_failed");
    assert!(error.source.is_some());
}
#[test]
fn cold_queue_recovery_rederives_policy_refusal_before_torn_tail_repair() {
    let root = root();
    let mut j = journal(&root, 1);
    let high = j.checkpoint().unwrap().through;
    drop(j);
    let path = root.path().join("sessions/publication-fixture/queue.jsonl");
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(file, "{{\"kind\":\"future\",\"seq\":{}}}", high + 1).unwrap();
    file.write_all(b"{\"torn\":").unwrap();
    file.sync_all().unwrap();
    drop(file);
    let bytes = fs::read(&path).unwrap();
    let source = Source::open_retained(
        root.path(),
        AGENT,
        RewritePolicy::new(NonZeroU64::new(1).unwrap()),
    )
    .unwrap();
    let mut reader = source.subscribe(None).unwrap();
    assert!(matches!(
        reader.page(),
        Page::Refused(ErrorCode::QueueJournalCorrupt)
    ));
    assert!(matches!(reader.page(), Page::End));
    assert_eq!(fs::read(path).unwrap(), bytes);
}
