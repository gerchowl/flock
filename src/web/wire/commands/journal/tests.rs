use super::*;
use serde_json::json;
use std::{fs, time::Duration};
fn root() -> crate::structured::launch::SessionConfigDir {
    crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap()
}
fn setup() -> (crate::structured::launch::SessionConfigDir, Journal) {
    let root = root();
    let mut journal = Journal::open(root.path()).unwrap();
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
fn key(id: &str) -> Key {
    ("local".into(), "p".into(), id.into())
}
fn operation(id: &str) -> Operation {
    Operation::Claim {
        namespace: "local".into(),
        principal: "p".into(),
        command: command(id),
        now: 100,
    }
}
fn claim(j: &mut Journal, id: &str) {
    assert!(matches!(
        j.claim("local", "p", &command(id), 100).unwrap(),
        Admission::Claimed(100)
    ));
}
#[test]
fn replay_rebuilds_index_counts_and_first_outcome() {
    let (root, mut journal) = setup();
    claim(&mut journal, "one");
    claim(&mut journal, "two");
    let first = journal
        .finish(&key("one"), 100, Ok(json!({"ok":true})))
        .unwrap();
    assert_eq!(
        journal
            .finish(&key("one"), 100, Err(ErrorCode::InvalidParams))
            .unwrap(),
        first
    );
    let bytes = journal.bytes;
    drop(journal);
    let mut journal = Journal::open(root.path()).unwrap();
    assert_eq!(journal.bytes, bytes);
    assert_eq!(journal.index.records.len(), 2);
    assert_eq!(journal.index.counts[&("local".into(), "p".into())], 2);
    assert!(
        matches!(journal.claim("local","p",&command("one"),101).unwrap(),Admission::Replayed(frame) if frame == first)
    );
    assert!(matches!(
        journal.claim("local", "p", &command("two"), 101).unwrap(),
        Admission::Pending(100)
    ));
    assert_eq!(
        fs::read_to_string(root.path().join("wire/commands.jsonl"))
            .unwrap()
            .lines()
            .count(),
        3
    );
}
#[test]
fn claim_and_finish_write_one_record_independent_of_retained_size() {
    let mut deltas = Vec::new();
    for size in [0, 2000] {
        let root = root();
        let mut journal = Journal::open(root.path()).unwrap();
        // Seed a real replayable log without thousands of fixture fsyncs.
        for n in 0..size {
            let record = Record {
                version: 1,
                namespace: "local".into(),
                principal: "seed".into(),
                cmd_id: format!("id-{n}"),
                digest: [0; 32],
                admitted_ms: 100,
                outcome: None,
            };
            serde_json::to_writer(&mut journal.file, &record).unwrap();
            journal.file.write_all(b"\n").unwrap();
        }
        journal.file.sync_all().unwrap();
        drop(journal);
        let mut journal = Journal::open(root.path()).unwrap();
        let before = journal.bytes;
        let written = journal.file.written;
        let original = fs::read(root.path().join("wire/commands.jsonl")).unwrap();
        claim(&mut journal, "one");
        let middle = journal.file.written;
        let claim_bytes = serde_json::to_vec(&journal.index.records[&key("one")])
            .unwrap()
            .len() as u64
            + 1;
        journal
            .finish(&key("one"), 100, Ok(json!({"ok":true})))
            .unwrap();
        deltas.push((middle - written, journal.file.written - middle));
        let outcome_bytes = serde_json::to_vec(&journal.index.records[&key("one")])
            .unwrap()
            .len() as u64
            + 1;
        assert_eq!(*deltas.last().unwrap(), (claim_bytes, outcome_bytes));
        let bytes = fs::read(root.path().join("wire/commands.jsonl")).unwrap();
        assert_eq!(&bytes[..before as usize], &original);
        assert!(deltas.last().unwrap().0 < ENTRY_BYTES as u64);
        assert!(deltas.last().unwrap().1 < ENTRY_BYTES as u64);
    }
    assert_eq!(deltas[0], deltas[1]);
}
#[test]
fn principal_global_and_byte_caps_refuse_without_eviction() {
    let (root, mut journal) = setup();
    claim(&mut journal, "one");
    claim(&mut journal, "two");
    assert_eq!(
        journal.claim("local", "p", &command("three"), 101).err(),
        Some(ErrorCode::SessionBusy)
    );
    journal.claim("local", "q", &command("three"), 101).unwrap();
    let before = fs::read(root.path().join("wire/commands.jsonl")).unwrap();
    assert_eq!(
        journal.claim("local", "r", &command("four"), 102).err(),
        Some(ErrorCode::SessionBusy)
    );
    assert_eq!(
        before,
        fs::read(root.path().join("wire/commands.jsonl")).unwrap()
    );
    assert!(matches!(
        journal.claim("local", "p", &command("one"), 103).unwrap(),
        Admission::Pending(100)
    ));
    journal.global_limit = 4;
    journal.bytes = LOG_BYTES;
    assert_eq!(
        journal.claim("local", "r", &command("four"), 102).err(),
        Some(ErrorCode::SessionBusy)
    );
    assert_eq!(journal.index.records.len(), 3);
}
#[test]
fn namespace_payload_target_and_delivery_flag_are_bound_correctly() {
    let (_root, mut journal) = setup();
    claim(&mut journal, "one");
    let mut retry = command("one");
    retry.resume_only = true;
    assert!(matches!(
        journal.claim("local", "p", &retry, 101).unwrap(),
        Admission::Pending(100)
    ));
    retry.args["agent"] = json!("agent-2");
    assert_eq!(
        journal.claim("local", "p", &retry, 101).err(),
        Some(ErrorCode::InvalidParams)
    );
    retry = command("one");
    retry.verb = "agent.stop".into();
    assert_eq!(
        journal.claim("local", "p", &retry, 101).err(),
        Some(ErrorCode::InvalidParams)
    );
    assert!(matches!(
        journal
            .claim("home-node", "p", &command("one"), 101)
            .unwrap(),
        Admission::Claimed(101)
    ));
}
#[test]
fn absent_expired_and_lost_claims_fail_closed() {
    let (root, mut journal) = setup();
    let mut cmd = command("one");
    cmd.resume_only = true;
    assert_eq!(
        journal.claim("local", "p", &cmd, 100).err(),
        Some(ErrorCode::SessionUnconfirmed)
    );
    claim(&mut journal, "one");
    let expiry = 100 + super::super::super::IDEMPOTENCY_WINDOW_MS;
    assert_eq!(
        journal.claim("local", "p", &cmd, expiry).err(),
        Some(ErrorCode::SessionUnconfirmed)
    );
    assert_eq!(
        journal.claim("local", "p", &command("one"), expiry).err(),
        Some(ErrorCode::SessionBusy)
    );
    assert_eq!(
        journal.finish(&key("one"), 101, Ok(json!({}))).err(),
        Some(ErrorCode::SessionUnconfirmed)
    );
    fs::remove_file(root.path().join("wire/commands.jsonl")).unwrap();
    assert!(matches!(
        journal.apply(operation("one")),
        Err(ErrorCode::SessionUnconfirmed)
    ));
    drop(journal);
    let mut journal = Journal::open(root.path()).unwrap();
    assert_eq!(
        journal.claim("local", "p", &cmd, 101).err(),
        Some(ErrorCode::SessionUnconfirmed)
    );
}
#[test]
fn torn_tail_is_archived_before_truncation_and_append_remains_readable() {
    let (root, mut journal) = setup();
    claim(&mut journal, "one");
    let before = journal.bytes;
    journal.file.write_all(b"{\"outcome\":").unwrap();
    journal.file.sync_all().unwrap();
    drop(journal);
    let mut journal = Journal::open(root.path()).unwrap();
    assert_eq!(journal.bytes, before);
    let archived: Value =
        serde_json::from_slice(&fs::read(root.path().join("wire/commands.torn.jsonl")).unwrap())
            .unwrap();
    assert_eq!(archived["raw"], json!(b"{\"outcome\":".to_vec()));
    claim(&mut journal, "two");
    drop(journal);
    let journal = Journal::open(root.path()).unwrap();
    assert_eq!(journal.index.records.len(), 2);
}
#[test]
fn complete_corruption_and_oversized_rows_preserve_raw_and_other_results() {
    for bad in [b"{\"version\":99}\n".to_vec(), vec![b'x'; ENTRY_BYTES + 30]] {
        let (root, mut journal) = setup();
        claim(&mut journal, "one");
        let first = journal
            .finish(&key("one"), 100, Ok(json!({"ok":true})))
            .unwrap();
        journal.file.write_all(&bad).unwrap();
        journal.file.write_all(b"\n").unwrap();
        // A valid later row proves recovery skips the damaged row.
        let record = Record {
            version: 1,
            namespace: "local".into(),
            principal: "q".into(),
            cmd_id: "two".into(),
            digest: [0; 32],
            admitted_ms: 100,
            outcome: None,
        };
        serde_json::to_writer(&mut journal.file, &record).unwrap();
        journal.file.write_all(b"\n").unwrap();
        drop(journal);
        let before = fs::read(root.path().join("wire/commands.jsonl")).unwrap();
        let mut journal = Journal::open(root.path()).unwrap();
        assert!(journal.blocked);
        assert_eq!(journal.index.records.len(), 2);
        assert!(
            matches!(journal.claim("local","p",&command("one"),101).unwrap(),Admission::Replayed(frame) if frame==first)
        );
        assert_eq!(
            journal.claim("local", "r", &command("new"), 101).err(),
            Some(ErrorCode::SessionBusy)
        );
        assert_eq!(
            before,
            fs::read(root.path().join("wire/commands.jsonl")).unwrap()
        );
    }
}
#[test]
fn archive_full_preserves_the_active_tail() {
    let (root, mut journal) = setup();
    claim(&mut journal, "one");
    journal.file.write_all(b"torn").unwrap();
    drop(journal);
    let archive = private(&root.path().join("wire/commands.torn.jsonl")).unwrap();
    archive.set_len((ENTRY_BYTES * 32) as u64).unwrap();
    drop(archive);
    let before = fs::read(root.path().join("wire/commands.jsonl")).unwrap();
    assert!(Journal::open(root.path()).is_err());
    assert_eq!(
        before,
        fs::read(root.path().join("wire/commands.jsonl")).unwrap()
    );
}
#[test]
fn append_and_sync_failures_roll_back_before_publishing_index() {
    for fault in [Fault::Partial, Fault::Sync] {
        let (root, mut journal) = setup();
        claim(&mut journal, "one");
        let before = fs::read(root.path().join("wire/commands.jsonl")).unwrap();
        journal.fault = Some(fault);
        assert_eq!(
            journal.claim("local", "p", &command("two"), 100).err(),
            Some(ErrorCode::SessionBusy)
        );
        assert_eq!(journal.index.records.len(), 1);
        assert_eq!(journal.index.counts[&("local".into(), "p".into())], 1);
        assert_eq!(
            before,
            fs::read(root.path().join("wire/commands.jsonl")).unwrap()
        );
        assert!(journal.blocked);
        drop(journal);
        let mut journal = Journal::open(root.path()).unwrap();
        claim(&mut journal, "two");
    }
}
#[test]
fn failed_outcome_keeps_pending_and_failed_rollback_requires_recovery() {
    for fault in [Fault::Partial, Fault::Sync, Fault::Rollback] {
        let (root, mut journal) = setup();
        claim(&mut journal, "one");
        journal.fault = Some(fault);
        assert_eq!(
            journal
                .finish(&key("one"), 100, Ok(json!({"ok":true})))
                .err(),
            Some(ErrorCode::SessionUnconfirmed)
        );
        assert!(journal.index.records[&key("one")].outcome.is_none());
        assert!(journal.blocked);
        drop(journal);
        let mut journal = Journal::open(root.path()).unwrap();
        assert!(matches!(
            journal.claim("local", "p", &command("one"), 101).unwrap(),
            Admission::Pending(100)
        ));
    }
}
#[test]
fn oversized_outcome_does_not_append_or_publish() {
    let (_root, mut journal) = setup();
    claim(&mut journal, "one");
    let before = journal.bytes;
    assert_eq!(
        journal
            .finish(&key("one"), 100, Ok(json!("x".repeat(ENTRY_BYTES))))
            .err(),
        Some(ErrorCode::SessionUnconfirmed)
    );
    assert_eq!(journal.bytes, before);
    assert!(journal.index.records[&key("one")].outcome.is_none());
}
#[test]
fn exclusive_writer_lease_prevents_stale_parallel_indexes() {
    let (root, journal) = setup();
    assert!(Journal::open(root.path()).is_err());
    drop(journal);
    assert!(Journal::open(root.path()).is_ok());
}

struct Closed(Option<oneshot::Sender<()>>);
impl Drop for Closed {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}
fn submit(writer: &Writer, operation: Operation) -> Result<Response, ErrorCode> {
    let writer = writer.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(writer.submit(operation));
    });
    rx.recv_timeout(Duration::from_secs(5))
        .expect("submission blocked or producer exited")
}

#[test]
fn worker_never_blocks_submit_on_sync_and_queue_remains_bounded() {
    super::super::super::tests::runtime(async {
        let root = root();
        let data = root.path().to_owned();
        let (entered, ready) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let (exited, done) = oneshot::channel();
        let writer = Writer::spawn(
            move || {
                let mut journal = Journal::open(&data)?;
                let closed = Closed(Some(exited));
                journal.before_sync = Some(Box::new(move || {
                    let _ = &closed;
                    entered.send(()).unwrap();
                    wait.recv_timeout(Duration::from_secs(5)).unwrap();
                }));
                Ok(journal)
            },
            Arc::new(Notify::new()),
        )
        .await
        .unwrap();
        let mut first = submit(&writer, operation("first")).unwrap();
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            first.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        let mut queued = Vec::new();
        for n in 0..QUEUED {
            queued.push(submit(&writer, operation(&format!("queued-{n}"))).unwrap());
        }
        assert_eq!(
            submit(&writer, operation("overflow")).err(),
            Some(ErrorCode::SessionBusy)
        );
        // The same current-thread executor advances while the worker is held at sync.
        assert_eq!(
            tokio::spawn(async {
                tokio::task::yield_now().await;
                7
            })
            .await
            .unwrap(),
            7
        );
        // Release every accepted append, including those whose waiters are dropped.
        for _ in 0..=QUEUED {
            release.send(()).unwrap();
        }
        let last = queued.pop().unwrap();
        drop(queued);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(5), first)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            Admission::Claimed(100)
        ));
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(5), last)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            Admission::Claimed(100)
        ));
        let final_reply = submit(
            &writer,
            Operation::Finish {
                key: key("first"),
                admitted_ms: 100,
                result: Ok(json!({"ok":true})),
            },
        )
        .unwrap();
        release.send(()).unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(5), final_reply)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            Admission::Replayed(ServerFrame::Ack { ok: true, .. })
        ));
        // Closing the mailbox drains work and releases ownership without a blocking Drop.
        drop(writer);
        tokio::time::timeout(Duration::from_secs(5), done)
            .await
            .unwrap()
            .unwrap();
        let journal = Journal::open(root.path()).unwrap();
        assert_eq!(journal.index.records.len(), QUEUED + 1);
        assert!(journal.index.records[&key("first")].outcome.is_some());
    });
}

#[test]
fn crash_after_synced_outcome_before_index_publication_replays_the_ack() {
    let (root, mut journal) = setup();
    claim(&mut journal, "one");
    let first = ack("one".into(), Ok(json!({"ok":true})));
    let mut record = journal.index.records[&key("one")].clone();
    record.outcome = Some(serde_json::to_value(&first).unwrap());
    journal.append(&record).unwrap();
    assert!(journal.index.records[&key("one")].outcome.is_none());
    drop(journal);
    let mut journal = Journal::open(root.path()).unwrap();
    assert!(
        matches!(journal.claim("local","p",&command("one"),101).unwrap(),Admission::Replayed(frame) if frame==first)
    );
}

#[test]
fn public_writer_opens_and_wakes_only_after_durable_completion() {
    super::super::super::tests::runtime(async {
        let root = root();
        let wake = Arc::new(Notify::new());
        let writer = Writer::open(root.path().to_owned(), wake.clone())
            .await
            .unwrap();
        let response = submit(&writer, operation("one")).unwrap();
        let notification = wake.notified();
        tokio::time::timeout(Duration::from_secs(5), notification)
            .await
            .unwrap();
        assert!(matches!(
            response.await.unwrap().unwrap(),
            Admission::Claimed(100)
        ));
        let text = fs::read_to_string(root.path().join("wire/commands.jsonl")).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(text.trim()).unwrap()["cmd_id"],
            "one"
        );
        // Lease completion is observed through the worker's last queued job, not a sleep.
        let (tx, rx) = oneshot::channel();
        let owned = writer.clone();
        let data = root.path().to_owned();
        drop(writer);
        std::thread::spawn(move || {
            drop(owned);
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(journal) = Journal::open(&data) {
                    drop(journal);
                    let _ = tx.send(());
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "writer did not release lease"
                );
                std::thread::yield_now();
            }
        });
        tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .unwrap()
            .unwrap();
    });
}

pub(super) struct Stream {
    file: File,
    written: u64,
}
impl From<File> for Stream {
    fn from(file: File) -> Self {
        Self { file, written: 0 }
    }
}
impl std::ops::Deref for Stream {
    type Target = File;
    fn deref(&self) -> &File {
        &self.file
    }
}
impl Write for Stream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = self.file.write(bytes)?;
        self.written += count as u64;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

mod compaction;
