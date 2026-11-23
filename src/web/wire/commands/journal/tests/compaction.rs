use super::std;
use super::*;
use std::sync::{Mutex, OnceLock};
const NOW: u64 = super::super::super::super::IDEMPOTENCY_WINDOW_MS + 1000;
fn receive(rx: &mpsc::Receiver<Message>) -> io::Result<compact::Snapshot> {
    match rx
        .recv_timeout(Duration::from_secs(5))
        .expect("compactor exited or failed to report")
    {
        Message::Compacted(result) => result,
        _ => panic!("unexpected journal job"),
    }
}
pub(super) fn start(journal: &mut Journal, now: u64) -> mpsc::Receiver<Message> {
    let (tx, rx) = mpsc::sync_channel(QUEUED);
    journal.next_compaction = 1;
    journal.start_compaction(now, tx);
    rx
}
pub(super) fn finish_compaction(journal: &mut Journal, rx: &mpsc::Receiver<Message>) {
    while journal.compaction.is_some() {
        journal.compacted(receive(rx));
    }
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(5)),
        Err(mpsc::RecvTimeoutError::Disconnected)
    ));
}
#[test]
fn expiry_threshold_drops_only_expired_records_and_releases_exact_capacity() {
    let (root, mut journal) = setup();
    journal
        .claim("local", "p", &command("expired"), 100)
        .unwrap();
    journal
        .claim("local", "p", &command("boundary"), 1000)
        .unwrap();
    journal
        .claim("local", "q", &command("retained"), 1001)
        .unwrap();
    let first = journal
        .finish(
            &("local".into(), "q".into(), "retained".into()),
            1001,
            Ok(json!({"ok":true})),
        )
        .unwrap();
    let (tx, rx) = mpsc::sync_channel(QUEUED);
    journal.start_compaction(1002, tx);
    assert!(
        journal.compaction.is_none(),
        "unexpired entries below the growth threshold must not compact"
    );
    assert!(matches!(
        rx.try_recv(),
        Err(mpsc::TryRecvError::Disconnected)
    ));
    let rx = start(&mut journal, NOW);
    finish_compaction(&mut journal, &rx);
    assert_eq!(journal.index.records.len(), 1);
    assert_eq!(
        journal.index.counts,
        HashMap::from([(("local".into(), "q".into()), 1)])
    );
    assert!(
        matches!(journal.claim("local", "q", &command("retained"), NOW).unwrap(), Admission::Replayed(frame) if frame == first)
    );
    let mut resume = command("expired");
    resume.resume_only = true;
    assert_eq!(
        journal.claim("local", "p", &resume, NOW).err(),
        Some(ErrorCode::SessionUnconfirmed)
    );
    journal
        .claim("local", "p", &command("new-one"), NOW)
        .unwrap();
    journal
        .claim("local", "p", &command("new-two"), NOW)
        .unwrap();
    assert_eq!(
        journal.claim("local", "p", &command("overflow"), NOW).err(),
        Some(ErrorCode::SessionBusy)
    );
    assert_eq!(
        journal.bytes,
        filesystem::fs::metadata(root.path().join("wire/commands.jsonl"))
            .unwrap()
            .len()
    );
    let expected = journal.index.counts.clone();
    drop(journal);
    let recovered = Journal::open(root.path()).unwrap();
    assert!(!recovered.blocked);
    assert_eq!(recovered.index.counts, expected);
    assert_eq!(recovered.index.records.len(), 3);
}

type Pause = (mpsc::Sender<()>, mpsc::Receiver<()>);
static PAUSE: OnceLock<Mutex<Option<Pause>>> = OnceLock::new();
fn pause_prefix(step: &str) {
    if step == "created" {
        let channels = PAUSE.get().unwrap().lock().unwrap().take().unwrap();
        channels.0.send(()).unwrap();
        channels.1.recv_timeout(Duration::from_secs(5)).unwrap();
    }
}
#[test]
fn concurrent_appends_and_multiple_catchup_rounds_keep_exact_index_and_first_outcomes() {
    let (root, mut journal) = setup();
    journal.principal_limit = 100;
    journal.global_limit = 100;
    journal
        .claim("local", "p", &command("expired"), 100)
        .unwrap();
    journal.claim("local", "p", &command("keep"), 1001).unwrap();
    let (entered, ready) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    *PAUSE.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some((entered, wait));
    journal.compaction_hook = Some(pause_prefix);
    let rx = start(&mut journal, NOW);
    ready.recv_timeout(Duration::from_secs(5)).unwrap();
    let first = journal
        .finish(&key("keep"), 1001, Ok(json!({"ok":true})))
        .unwrap();
    for n in 0..10 {
        let id = format!("appended-{n}");
        journal.claim("local", "p", &command(&id), NOW).unwrap();
        journal
            .finish(&key(&id), NOW, Ok(json!("x".repeat(20000))))
            .unwrap();
    }
    release.send(()).unwrap();
    // The initial prefix excludes all of these concurrently appended outcomes.
    journal.compacted(receive(&rx));
    assert!(
        journal.compaction.is_some(),
        "large tail must return to the separate worker"
    );
    let caught_up = receive(&rx);
    // Append during the final publication race as well, in another namespace.
    journal
        .claim("home-node", "other", &command("final"), NOW + 1)
        .unwrap();
    journal.compacted(caught_up);
    finish_compaction(&mut journal, &rx);
    assert!(!journal.blocked);
    assert_eq!(journal.index.records.len(), 12);
    assert_eq!(
        journal.index.counts,
        HashMap::from([
            (("local".into(), "p".into()), 11),
            (("home-node".into(), "other".into()), 1)
        ])
    );
    assert!(!journal.index.records.contains_key(&key("expired")));
    assert!(
        matches!(journal.claim("local", "p", &command("keep"), NOW).unwrap(), Admission::Replayed(frame) if frame == first)
    );
    let records = serde_json::to_value(journal.index.records.values().collect::<Vec<_>>()).unwrap();
    let counts = journal.index.counts.clone();
    drop(journal);
    let recovered = Journal::open(root.path()).unwrap();
    assert!(!recovered.blocked);
    assert_eq!(recovered.index.counts, counts);
    for record in records.as_array().unwrap() {
        let key = (
            record["namespace"].as_str().unwrap().into(),
            record["principal"].as_str().unwrap().into(),
            record["cmd_id"].as_str().unwrap().into(),
        );
        assert_eq!(
            serde_json::to_value(&recovered.index.records[&key]).unwrap(),
            *record
        );
    }
}
#[test]
fn opaque_rows_fence_compaction_and_remain_byte_exact() {
    for opaque in [
        b"{\"version\":99}\n".to_vec(),
        b"bad json\n".to_vec(),
        [vec![b'x'; ENTRY_BYTES + 1], b"\n".to_vec()].concat(),
    ] {
        let (root, mut journal) = setup();
        claim(&mut journal, "expired");
        journal.file.write_all(&opaque).unwrap();
        journal.file.sync_all().unwrap();
        drop(journal);
        let mut journal = Journal::open(root.path()).unwrap();
        let before = filesystem::fs::read(root.path().join("wire/commands.jsonl")).unwrap();
        let rx = start(&mut journal, NOW);
        assert!(journal.compaction.is_none());
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        assert_eq!(
            filesystem::fs::read(root.path().join("wire/commands.jsonl")).unwrap(),
            before
        );
        assert_eq!(journal.index.counts[&("local".into(), "p".into())], 1);
        assert_eq!(
            journal.claim("local", "q", &command("new"), NOW).err(),
            Some(ErrorCode::SessionBusy)
        );
    }
}
#[test]
fn failed_append_during_snapshot_cannot_publish_over_uncertain_tail() {
    let (root, mut journal) = setup();
    claim(&mut journal, "expired");
    let rx = start(&mut journal, NOW);
    let snapshot = receive(&rx);
    journal.fault = Some(Fault::Rollback);
    assert!(journal.claim("local", "p", &command("torn"), NOW).is_err());
    let before = filesystem::fs::read(root.path().join("wire/commands.jsonl")).unwrap();
    journal.compacted(snapshot);
    assert!(journal.blocked);
    assert_eq!(
        filesystem::fs::read(root.path().join("wire/commands.jsonl")).unwrap(),
        before
    );
    assert!(journal.index.records.contains_key(&key("expired")));
}
#[test]
fn shutdown_drains_cancelled_jobs_closes_every_clone_and_releases_lease_before_reopen() {
    super::super::super::super::tests::runtime(async {
        let root = root();
        let data = root.path().to_owned();
        let (entered, ready) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let writer = Writer::spawn(
            move || {
                let mut journal = Journal::open(&data)?;
                let mut first = true;
                journal.before_sync = Some(Box::new(move || {
                    if first {
                        first = false;
                        entered.send(()).unwrap();
                        wait.recv_timeout(Duration::from_secs(5)).unwrap();
                    }
                }));
                Ok(journal)
            },
            Arc::new(Notify::new()),
        )
        .await
        .unwrap();
        let clone = writer.clone();
        assert!(matches!(
            Writer::open(root.path().to_owned(), Arc::new(Notify::new())).await,
            Err(ErrorCode::SessionBusy)
        ));
        let first = writer.submit(operation("first")).unwrap();
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        let outcome = writer
            .submit(Operation::Finish {
                key: key("first"),
                admitted_ms: 100,
                result: Ok(json!({"ok":true})),
            })
            .unwrap();
        for n in 1..QUEUED {
            drop(writer.submit(operation(&format!("queued-{n}"))).unwrap());
        }
        let mut shutdown = Box::pin(writer.shutdown());
        assert!(matches!(
            futures_util::poll!(&mut shutdown),
            std::task::Poll::Pending
        ));
        assert_eq!(
            clone.submit(operation("after-close")).err(),
            Some(ErrorCode::SessionBusy)
        );
        drop(shutdown);
        release.send(()).unwrap();
        assert!(matches!(
            first.await.unwrap().unwrap(),
            Admission::Claimed(100)
        ));
        assert!(matches!(
            outcome.await.unwrap().unwrap(),
            Admission::Replayed(ServerFrame::Ack { ok: true, .. })
        ));
        // The outcome proves at least two queue slots have drained.
        assert_eq!(
            clone.submit(operation("after-drain")).err(),
            Some(ErrorCode::SessionBusy)
        );
        tokio::time::timeout(Duration::from_secs(5), clone.shutdown())
            .await
            .unwrap()
            .unwrap();
        writer.shutdown().await.unwrap();
        // Both old handles remain alive. Successful reopen proves an explicit lease barrier.
        let replacement = Writer::open(root.path().to_owned(), Arc::new(Notify::new()))
            .await
            .unwrap();
        replacement.shutdown().await.unwrap();
        let recovered = Journal::open(root.path()).unwrap();
        assert_eq!(recovered.index.records.len(), QUEUED);
        assert!(recovered.index.records[&key("first")].outcome.is_some());
        assert!(!recovered.index.records.contains_key(&key("after-close")));
    });
}

fn crash_hook(step: &str) {
    if std::env::var("FLOCK_JOURNAL_CRASH_STEP").unwrap() == step {
        // Exit without unwinding, matching the blob publication crash fixture.
        std::process::exit(crate::cli::exit::SUCCESS);
    }
}
#[test]
fn compaction_crash_child() {
    let Ok(root) = std::env::var("FLOCK_JOURNAL_CRASH_ROOT") else {
        return;
    };
    let mut journal = Journal::open(Path::new(&root)).unwrap();
    journal.compaction_hook = Some(crash_hook);
    let rx = start(&mut journal, NOW);
    finish_compaction(&mut journal, &rx);
    panic!("crash stage never reached");
}
#[test]
fn crash_at_each_compaction_step_leaves_a_complete_replayable_old_or_new_log() {
    for step in [
        "created",
        "row_written",
        "prefix_copied",
        "catchup_copied",
        "synced",
        "renamed",
        "directory_synced",
    ] {
        let (root, mut journal) = setup();
        journal
            .claim("local", "p", &command("expired"), 100)
            .unwrap();
        journal.claim("local", "p", &command("keep"), 1001).unwrap();
        let first = journal
            .finish(&key("keep"), 1001, Ok(json!({"first":true})))
            .unwrap();
        let before = filesystem::fs::read(root.path().join("wire/commands.jsonl")).unwrap();
        drop(journal);
        let mut child =
            crate::process::TracedCommand::new(std::env::current_exe().unwrap(), "journal-test");
        child
            .args([
                "--exact",
                "web::wire::commands::journal::tests::compaction::compaction_crash_child",
                "--nocapture",
            ])
            .env("FLOCK_JOURNAL_CRASH_ROOT", root.path())
            .env("FLOCK_JOURNAL_CRASH_STEP", step)
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
                panic!("crash child failed to reach stage {step}");
            }
            std::thread::yield_now();
        };
        assert_eq!(status.code(), Some(crate::cli::exit::SUCCESS), "{step}");
        let after = filesystem::fs::read(root.path().join("wire/commands.jsonl")).unwrap();
        if matches!(
            step,
            "created" | "row_written" | "prefix_copied" | "catchup_copied" | "synced"
        ) {
            assert_eq!(after, before, "{step}");
        }
        let mut recovered = Journal::open(root.path()).unwrap();
        assert!(!recovered.blocked, "{step}");
        assert!(
            matches!(recovered.claim("local", "p", &command("keep"), NOW).unwrap(), Admission::Replayed(frame) if frame == first)
        );
        assert_eq!(
            recovered.index.counts[&("local".into(), "p".into())],
            if matches!(step, "renamed" | "directory_synced") {
                1
            } else {
                2
            }
        );
    }
}

static PUBLISHED: OnceLock<Mutex<Option<mpsc::Sender<()>>>> = OnceLock::new();
fn published(step: &str) {
    if step == "directory_synced" {
        if let Some(tx) = PUBLISHED.get().unwrap().lock().unwrap().take() {
            tx.send(()).unwrap();
        }
    }
}
#[test]
fn writer_claim_clock_drives_threshold_compaction_and_shutdown_drains_it() {
    super::super::super::super::tests::runtime(async {
        let (root, mut journal) = setup();
        journal
            .claim("local", "p", &command("expired"), 100)
            .unwrap();
        journal.claim("local", "q", &command("keep"), 1001).unwrap();
        drop(journal);
        let (tx, rx) = mpsc::channel();
        *PUBLISHED.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(tx);
        let data = root.path().to_owned();
        let writer = Writer::spawn(
            move || {
                let mut journal = Journal::open(&data)?;
                journal.next_compaction = 1;
                journal.compaction_hook = Some(published);
                Ok(journal)
            },
            Arc::new(Notify::new()),
        )
        .await
        .unwrap();
        let response = writer
            .submit(Operation::Claim {
                namespace: "local".into(),
                principal: "p".into(),
                command: command("new"),
                now: NOW,
            })
            .unwrap();
        assert!(matches!(
            response.await.unwrap().unwrap(),
            Admission::Claimed(NOW)
        ));
        rx.recv_timeout(Duration::from_secs(5))
            .expect("compactor exited or publication never completed");
        writer.shutdown().await.unwrap();
        let recovered = Journal::open(root.path()).unwrap();
        assert!(!recovered.blocked);
        assert_eq!(recovered.index.records.len(), 2);
        assert_eq!(
            recovered.index.counts,
            HashMap::from([
                (("local".into(), "p".into()), 1),
                (("local".into(), "q".into()), 1)
            ])
        );
    });
}

#[test]
fn replacement_descriptor_supports_later_sweeps_appends_and_future_timestamps() {
    let (root, mut journal) = setup();
    claim(&mut journal, "expired");
    let rx = start(&mut journal, NOW);
    finish_compaction(&mut journal, &rx);
    assert_eq!(journal.bytes, 0);
    journal
        .claim("local", "p", &command("future"), NOW + 100)
        .unwrap();
    let rx = start(&mut journal, NOW);
    finish_compaction(&mut journal, &rx);
    journal
        .claim("local", "p", &command("after-second-sweep"), NOW)
        .unwrap();
    assert_eq!(journal.index.records.len(), 2);
    drop(journal);
    let recovered = Journal::open(root.path()).unwrap();
    assert!(!recovered.blocked);
    assert_eq!(recovered.index.records.len(), 2);
    assert_eq!(recovered.index.counts[&("local".into(), "p".into())], 2);
}

#[test]
fn lost_primary_during_copy_is_fenced_without_resurrecting_the_journal() {
    let (root, mut journal) = setup();
    claim(&mut journal, "one");
    let rx = start(&mut journal, NOW);
    let snapshot = receive(&rx);
    let primary = root.path().join("wire/commands.jsonl");
    let backup = root.path().join("wire/commands.saved");
    let before = filesystem::fs::read(&primary).unwrap();
    filesystem::fs::rename(&primary, &backup).unwrap();
    journal.compacted(snapshot);
    assert!(journal.blocked);
    assert!(!primary.exists());
    assert_eq!(filesystem::fs::read(&backup).unwrap(), before);
    assert!(journal.index.records.contains_key(&key("one")));
}

#[test]
fn opaque_row_discovered_by_copier_preserves_the_log_and_fences_writes() {
    let (root, mut journal) = setup();
    claim(&mut journal, "one");
    let opaque = b"{\"version\":99}\n";
    journal.file.write_all(opaque).unwrap();
    journal.file.sync_all().unwrap();
    journal.bytes += opaque.len() as u64;
    let before = filesystem::fs::read(root.path().join("wire/commands.jsonl")).unwrap();
    let rx = start(&mut journal, NOW);
    finish_compaction(&mut journal, &rx);
    assert!(journal.blocked);
    assert_eq!(
        filesystem::fs::read(root.path().join("wire/commands.jsonl")).unwrap(),
        before
    );
    assert!(journal.index.records.contains_key(&key("one")));
    assert_eq!(
        journal.claim("local", "p", &command("new"), NOW).err(),
        Some(ErrorCode::SessionBusy)
    );
}

#[test]
fn expiry_reclaims_full_capacity_without_log_growth_after_sweep_or_reopen() {
    for reopen in [false, true] {
        for (global, principal) in [(1, 2), (3, 1)] {
            let (root, mut journal) = setup();
            journal.global_limit = global;
            journal.principal_limit = principal;
            claim(&mut journal, "one");
            let rx = start(&mut journal, 101);
            finish_compaction(&mut journal, &rx);
            if reopen {
                drop(journal);
                journal = Journal::open(root.path()).unwrap();
                journal.global_limit = global;
                journal.principal_limit = principal;
            }
            assert!(journal.bytes < journal.next_compaction);
            let (tx, rx) = mpsc::sync_channel(QUEUED);
            journal.start_compaction(101, tx);
            assert!(
                journal.compaction.is_none(),
                "unexpired claims must not trigger expiry sweeps"
            );
            assert!(matches!(
                rx.try_recv(),
                Err(mpsc::TryRecvError::Disconnected)
            ));
            let bytes = journal.bytes;
            assert_eq!(
                journal.claim("local", "p", &command("two"), NOW).err(),
                Some(ErrorCode::SessionBusy)
            );
            assert_eq!(journal.bytes, bytes);
            let (tx, rx) = mpsc::sync_channel(QUEUED);
            journal.start_compaction(NOW, tx);
            assert!(
                journal.compaction.is_some(),
                "expired capacity must recover without log growth"
            );
            finish_compaction(&mut journal, &rx);
            assert!(matches!(
                journal.claim("local", "p", &command("two"), NOW).unwrap(),
                Admission::Claimed(NOW)
            ));
            assert_eq!(journal.index.records.len(), 1);
            assert_eq!(journal.index.counts[&("local".into(), "p".into())], 1);
            assert_eq!(
                journal.claim("local", "p", &command("three"), NOW).err(),
                Some(ErrorCode::SessionBusy)
            );
        }
    }
}

#[test]
fn expired_capacity_sweep_preserves_unexpired_first_outcome() {
    let (_root, mut journal) = setup();
    journal.global_limit = 2;
    claim(&mut journal, "expired");
    journal.claim("local", "q", &command("keep"), 1001).unwrap();
    let keep = ("local".into(), "q".into(), "keep".into());
    let first = journal
        .finish(&keep, 1001, Ok(json!({"first":true})))
        .unwrap();
    let rx = start(&mut journal, 1002);
    finish_compaction(&mut journal, &rx);
    let (tx, rx) = mpsc::sync_channel(QUEUED);
    journal.start_compaction(NOW, tx);
    assert!(journal.compaction.is_some());
    finish_compaction(&mut journal, &rx);
    journal.claim("local", "p", &command("new"), NOW).unwrap();
    assert_eq!(journal.index.records.len(), 2);
    assert_eq!(
        journal.index.counts,
        HashMap::from([
            (("local".into(), "p".into()), 1),
            (("local".into(), "q".into()), 1)
        ])
    );
    assert!(
        matches!(journal.claim("local", "q", &command("keep"), NOW).unwrap(), Admission::Replayed(frame) if frame == first)
    );
}

#[test]
fn writer_capacity_refusal_triggers_expiry_sweep_and_retry_succeeds() {
    super::super::super::super::tests::runtime(async {
        let (root, mut journal) = setup();
        claim(&mut journal, "one");
        let rx = start(&mut journal, 101);
        finish_compaction(&mut journal, &rx);
        drop(journal);
        let (tx, rx) = mpsc::channel();
        *PUBLISHED.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(tx);
        let data = root.path().to_owned();
        let writer = Writer::spawn(
            move || {
                let mut journal = Journal::open(&data)?;
                journal.global_limit = 1;
                journal.principal_limit = 1;
                journal.compaction_hook = Some(published);
                Ok(journal)
            },
            Arc::new(Notify::new()),
        )
        .await
        .unwrap();
        let attempt = || Operation::Claim {
            namespace: "local".into(),
            principal: "p".into(),
            command: command("two"),
            now: NOW,
        };
        assert_eq!(
            writer.submit(attempt()).unwrap().await.unwrap().err(),
            Some(ErrorCode::SessionBusy)
        );
        rx.recv_timeout(Duration::from_secs(5))
            .expect("expiry sweep producer exited or never published");
        assert!(matches!(
            writer.submit(attempt()).unwrap().await.unwrap().unwrap(),
            Admission::Claimed(NOW)
        ));
        writer.shutdown().await.unwrap();
        let recovered = Journal::open(root.path()).unwrap();
        assert!(!recovered.blocked);
        assert_eq!(recovered.index.records.len(), 1);
        assert!(recovered.index.records.contains_key(&key("two")));
    });
}
