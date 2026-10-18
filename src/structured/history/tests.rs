use super::*;
use crate::structured::{
    claude::{SessionEvent, ShapeKey},
    launch::SessionConfigDir,
    transport::RecordOrigin,
};
use serde_json::{json, Value};
use std::{
    io::Write,
    time::{Duration, Instant},
};

fn record(seq: u64, raw: Value) -> StreamRecord {
    StreamRecord {
        seq,
        known: Some(true),
        shape: None,
        received_at_ms: 1,
        origin: RecordOrigin::Claude,
        result_subtype: None,
        permission_grant_request_id: None,
        raw: raw.to_string(),
    }
}
pub(crate) struct Fixture {
    _root: SessionConfigDir,
    pub(crate) file: File,
    pub(crate) source: History,
}
impl Fixture {
    pub(crate) fn new(records: Vec<StreamRecord>) -> Self {
        let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let sessions = root.path().join("sessions");
        crate::platform::structured::ensure_private_dir(&sessions).unwrap();
        let directory = sessions.join("fixture-agent");
        crate::platform::structured::ensure_private_dir(&directory).unwrap();
        let path = directory.join("history.jsonl");
        drop(crate::platform::structured::create_private_file(&path).unwrap());
        let mut file = crate::platform::structured::open_append_file(&path).unwrap();
        for record in &records {
            writeln!(file, "{}", serde_json::to_string(record).unwrap()).unwrap()
        }
        file.sync_all().unwrap();
        let source = History::default();
        source.bind("fixture-agent", "history");
        source
            .initialize(
                &file,
                records.last().map_or(0, |r| r.seq),
                file.metadata().unwrap().len(),
            )
            .unwrap();
        Self {
            _root: root,
            file,
            source,
        }
    }
    pub(crate) fn corrupt_wrapper(&mut self, index: usize, invalid_json: bool) {
        let path = self
            ._root
            .path()
            .join("sessions/fixture-agent/history.jsonl");
        let mut bytes = std::fs::read(path).unwrap();
        let start = if index == 0 {
            0
        } else {
            bytes
                .iter()
                .enumerate()
                .filter(|(_, byte)| **byte == b'\n')
                .nth(index - 1)
                .unwrap()
                .0
                + 1
        };
        if invalid_json {
            bytes[start] = b'!';
        } else {
            let at = bytes[start..]
                .windows(5)
                .position(|part| part == b"\"raw\"")
                .unwrap();
            bytes[start + at + 1] = b'x';
        }
        self.file.set_len(0).unwrap();
        self.file.write_all(&bytes).unwrap();
        self.file.sync_all().unwrap();
    }
    pub(crate) fn append(&mut self, record: StreamRecord) {
        let bytes = serde_json::to_vec(&record).unwrap();
        self.file.write_all(&bytes).unwrap();
        self.file.write_all(b"\n").unwrap();
        self.file.sync_all().unwrap();
        self.source
            .publish(Arc::new(record), bytes.len() + 1)
            .unwrap();
    }
}
fn next(cut: &mut Cut) -> Item {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let item = cut.page().unwrap();
        assert!(cut.buffer.capacity() <= PAGE_BYTES);
        if !matches!(item, Item::Pending | Item::Progress) {
            return item;
        }
        assert!(
            Instant::now() < deadline,
            "replay producer failed to reach cut/barrier"
        );
    }
}
fn seq(item: Item) -> u64 {
    let Item::Record(record) = item else {
        panic!("expected record: {item:?}")
    };
    record.seq
}

#[test]
fn absent_valid_ahead_cursors_and_empty_barrier_use_real_jsonl() {
    let fixture = Fixture::new(
        (1..=3)
            .map(|s| record(s, json!({"type":"assistant","message":{}})))
            .collect(),
    );
    let mut cut = fixture.source.subscribe(None).unwrap();
    assert_eq!(
        [
            seq(next(&mut cut)),
            seq(next(&mut cut)),
            seq(next(&mut cut))
        ],
        [1, 2, 3]
    );
    assert!(matches!(next(&mut cut), Item::Synced(3)));
    assert!(matches!(cut.page().unwrap(), Item::Pending));
    let mut cut = fixture.source.subscribe(Some(2)).unwrap();
    assert_eq!(seq(next(&mut cut)), 3);
    assert!(matches!(next(&mut cut), Item::Synced(3)));
    assert!(
        matches!(fixture.source.subscribe(Some(4)), Err(e) if e.kind()==io::ErrorKind::InvalidInput)
    );
    let empty = Fixture::new(vec![]);
    let mut cut = empty.source.subscribe(None).unwrap();
    assert!(matches!(next(&mut cut), Item::Synced(0)));
    empty.source.finish(false);
    assert!(matches!(next(&mut cut), Item::End));
}
#[test]
fn retained_prefix_and_legacy_provenance_gaps_continue_without_snapshot() {
    let mut legacy = record(5, json!({"type":"future"}));
    legacy.known = None;
    let fixture = Fixture::new(vec![
        legacy,
        record(6, json!({"type":"assistant","message":{}})),
    ]);
    let mut cut = fixture.source.subscribe(None).unwrap();
    assert!(matches!(
        next(&mut cut),
        Item::Gap {
            from: 1,
            to: 4,
            reason: GapReason::RetainedPrefix
        }
    ));
    assert!(matches!(
        next(&mut cut),
        Item::Gap {
            from: 5,
            to: 5,
            reason: GapReason::ProvenanceMissing
        }
    ));
    assert_eq!(seq(next(&mut cut)), 6);
    assert!(matches!(next(&mut cut), Item::Synced(6)));
}
#[test]
fn original_unknown_wrapper_and_blob_references_do_not_reclassify() {
    let raw = json!({"type":"assistant","message":{}});
    assert!(matches!(
        SessionEvent::parse(&raw.to_string(), "current"),
        SessionEvent::Assistant(_)
    ));
    let shape = ShapeKey::derive(&raw, "original-version");
    let mut unknown = record(1, raw);
    unknown.known = Some(false);
    unknown.shape = Some(shape.clone());
    let blob = json!({"type":"assistant","message":{"content":[{"blob":"a".repeat(64),"size":99,"preview":"snippet"}]}});
    let fixture = Fixture::new(vec![
        unknown,
        record(2, blob.clone()),
        record(3, json!({"type":"future-known"})),
    ]);
    let mut cut = fixture.source.subscribe(None).unwrap();
    let Item::Record(original) = next(&mut cut) else {
        panic!("unknown wrapper lost")
    };
    assert_eq!(original.known, Some(false));
    assert_eq!(original.shape, Some(shape.clone()));
    let wrapper = crate::structured_wire::UnknownEvent {
        r#type: crate::structured_wire::UnknownTag::Unknown,
        raw: original.raw.clone(),
        shape: original.shape.as_ref().unwrap().0.clone(),
    };
    assert_eq!(
        serde_json::to_value(wrapper).unwrap(),
        json!({"type":"unknown","raw":original.raw,"shape":shape.0})
    );
    let Item::Record(stored) = next(&mut cut) else {
        panic!("blob record lost")
    };
    assert_eq!(serde_json::from_str::<Value>(&stored.raw).unwrap(), blob);
    let Item::Record(stored) = next(&mut cut) else {
        panic!("known classification lost")
    };
    assert_eq!(stored.known, Some(true));
    assert!(matches!(
        SessionEvent::parse(&stored.raw, "current"),
        SessionEvent::Unknown { .. }
    ));
}
#[test]
fn page_budget_and_concurrent_publication_cannot_cross_the_barrier() {
    let mut fixture = Fixture::new(vec![record(
        1,
        json!({"type":"assistant","message":{"text":"x".repeat(3*PAGE_BYTES)}}),
    )]);
    let source = fixture.source.clone();
    let mut cut = source.subscribe(None).unwrap();
    assert!(matches!(cut.page().unwrap(), Item::Progress));
    assert_eq!(cut.offset, PAGE_BYTES as u64);
    assert!(cut.buffer.capacity() <= PAGE_BYTES);
    let (published, ack) = std::sync::mpsc::sync_channel(1);
    let (stop, release) = std::sync::mpsc::sync_channel(1);
    let (done, finished) = std::sync::mpsc::sync_channel(1);
    let producer = std::thread::spawn(move || {
        fixture.append(record(2, json!({"type":"user","message":{}})));
        published
            .try_send(())
            .expect("publication consumer remains present");
        release
            .recv_timeout(Duration::from_secs(10))
            .expect("replay must release its live producer");
        done.send(fixture)
            .unwrap_or_else(|_| panic!("completion consumer gone"));
    });
    ack.recv_timeout(Duration::from_secs(5))
        .expect("producer must publish during replay");
    let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_eq!(seq(next(&mut cut)), 1);
        assert!(matches!(next(&mut cut), Item::Synced(1)));
        assert_eq!(seq(next(&mut cut)), 2);
        assert!(matches!(cut.page().unwrap(), Item::Pending));
        let mut other = source.subscribe(Some(1)).unwrap();
        assert_eq!(seq(next(&mut other)), 2);
    }));
    stop.try_send(())
        .expect("producer stays alive and silent until released");
    let _fixture = finished
        .recv_timeout(Duration::from_secs(5))
        .expect("producer must close");
    producer.join().unwrap();
    if let Err(panic) = observed {
        std::panic::resume_unwind(panic)
    }
}

#[test]
fn record_and_byte_overflow_close_once_and_require_a_fresh_cut() {
    for large in [false, true] {
        let mut fixture = Fixture::new(vec![]);
        let mut cut = fixture.source.subscribe(None).unwrap();
        let count = if large { 1 } else { LIVE_RECORDS + 1 };
        for s in 1..=count {
            fixture.append(record(
                s as u64,
                json!({"text":if large {"x".repeat(LIVE_BYTES)} else {"x".into()}}),
            ))
        }
        assert!(matches!(next(&mut cut),Item::Lagged(to) if to==count as u64));
        assert!(matches!(next(&mut cut), Item::End));
        assert!(cut.live.lock().queue.is_empty());
        assert_eq!(cut.live.lock().bytes, 0);
        let mut fresh = fixture.source.subscribe(None).unwrap();
        assert_eq!(seq(next(&mut fresh)), 1);
    }
}
#[test]
fn registration_is_live_before_cut_construction_without_a_publication_hole() {
    for before in [true, false] {
        let mut fixture = Fixture::new(vec![]);
        if before {
            fixture.append(record(1, json!({"type":"user","message":{}})));
        }
        let registered = fixture.source.0.lock().register(None).unwrap();
        // This is the deterministic split-lock window: a cut has not been built.
        if !before {
            fixture.append(record(1, json!({"type":"user","message":{}})));
        }
        let mut cut = registered.cut(fixture.source.0.clone(), None);
        let mut seen = Vec::new();
        let mut barriers = Vec::new();
        for _ in 0..3 {
            match cut.page().unwrap() {
                Item::Record(record) => seen.push(record.seq),
                Item::Synced(head) => barriers.push(head),
                Item::Pending | Item::Progress => (),
                item => panic!("{item:?}"),
            }
        }
        assert_eq!(
            seen,
            vec![1],
            "publication between capture and construction cannot disappear"
        );
        assert_eq!(barriers, vec![u64::from(before)]);
    }
}

#[test]
fn legacy_provenance_gap_continues_with_later_classified_records() {
    let mut legacy = record(1, json!({"type":"future"}));
    legacy.known = None;
    legacy.shape = None;
    let fixture = Fixture::new(vec![legacy, record(2, json!({"type":"user","message":{}}))]);
    let mut cut = fixture.source.subscribe(None).unwrap();
    assert!(matches!(
        next(&mut cut),
        Item::Gap {
            from: 1,
            to: 1,
            reason: GapReason::ProvenanceMissing
        }
    ));
    assert_eq!(seq(next(&mut cut)), 2);
    assert!(matches!(next(&mut cut), Item::Synced(2)));
    assert_eq!(
        serde_json::to_value(GapReason::ProvenanceMissing).unwrap(),
        json!("provenance_missing")
    );
    assert_eq!(
        serde_json::to_value(GapReason::RetainedPrefix).unwrap(),
        json!("retained_prefix")
    );
}

#[test]
fn producer_panic_and_unopened_failure_are_explicit_not_pending_forever() {
    for ready in [false, true] {
        let fixture = Fixture::new(vec![record(1, json!({"type":"user","message":{}}))]);
        let source = if ready {
            fixture.source.clone()
        } else {
            History::default()
        };
        let producer = source.clone();
        let (done, ack) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || {
            let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _owner = producer.producer();
                panic!("scripted producer failure");
            }));
            done.try_send(failure.is_err())
                .expect("failure consumer remains present");
        });
        assert!(ack
            .recv_timeout(Duration::from_secs(5))
            .expect("producer must fail visibly"));
        thread.join().unwrap();
        if ready {
            let mut cut = source.subscribe(None).unwrap();
            assert_eq!(seq(next(&mut cut)), 1);
            assert!(matches!(next(&mut cut), Item::Synced(1)));
            assert!(cut.page().is_err());
        } else {
            assert!(matches!(source.subscribe(None),Err(e) if e.kind()!=io::ErrorKind::WouldBlock));
        }
    }
}

#[test]
fn cold_recovery_handles_empty_history_and_refuses_torn_or_zero_heads() {
    let mut fixture = Fixture::new(vec![]);
    let path = fixture
        ._root
        .path()
        .join("sessions/fixture-agent/history.jsonl");
    let retained = History::open_retained(&path).unwrap();
    let mut cut = retained.subscribe(None).unwrap();
    assert!(matches!(next(&mut cut), Item::Synced(0)));
    assert!(matches!(next(&mut cut), Item::End));
    writeln!(
        fixture.file,
        "{}",
        serde_json::to_string(&record(0, json!({"type":"future"}))).unwrap()
    )
    .unwrap();
    fixture.file.sync_all().unwrap();
    assert!(History::open_retained(&path).is_err());
    fixture.file.write_all(b"{\"seq\":1").unwrap();
    fixture.file.sync_all().unwrap();
    assert!(History::open_retained(&path).is_err());
}

#[tokio::test]
async fn readiness_publication_and_end_notifications_are_only_hints() {
    let mut fixture = Fixture::new(vec![]);
    let source = History::default();
    let wake = source.waker();
    let ready = wake.notified();
    tokio::pin!(ready);
    ready.as_mut().enable();
    assert!(matches!(source.subscribe(None),Err(e) if e.kind()==io::ErrorKind::WouldBlock));
    source.initialize(&fixture.file, 0, 0).unwrap();
    tokio::time::timeout(Duration::from_secs(5), ready)
        .await
        .expect("quiet empty stream must announce readiness");
    fixture.source = source.clone();
    let mut cut = source.subscribe(None).unwrap();
    assert!(matches!(cut.page().unwrap(), Item::Synced(0)));
    let next = wake.notified();
    tokio::pin!(next);
    next.as_mut().enable();
    fixture.append(record(1, json!({"type":"user","message":{}})));
    tokio::time::timeout(Duration::from_secs(5), next)
        .await
        .expect("publication must wake continuation");
    assert!(matches!(cut.page().unwrap(),Item::Record(record) if record.seq==1));
    wake.notify_waiters();
    assert!(matches!(cut.page().unwrap(), Item::Pending));
    let end = wake.notified();
    tokio::pin!(end);
    end.as_mut().enable();
    source.finish(false);
    tokio::time::timeout(Duration::from_secs(5), end)
        .await
        .expect("producer end must wake continuation");
    assert!(matches!(cut.page().unwrap(), Item::End));
}

fn long_history(count: u64) -> Fixture {
    Fixture::new(
        (1..=count)
            .map(|seq| {
                record(
                    seq,
                    json!({"type":"user","message":{"content":"x".repeat(400)}}),
                )
            })
            .collect(),
    )
}
#[test]
fn buffered_replay_reads_each_file_byte_once_instead_of_each_record_page() {
    let fixture = long_history(2_000);
    let length = fixture.file.metadata().unwrap().len();
    let mut cut = fixture.source.subscribe(None).unwrap();
    for expected in 1..=2_000 {
        assert_eq!(seq(next(&mut cut)), expected);
    }
    assert!(matches!(next(&mut cut), Item::Synced(2_000)));
    assert!(
        cut.read_bytes() <= length + PAGE_BYTES as u64,
        "read {} bytes for {length} bytes of JSONL",
        cut.read_bytes()
    );
}
#[test]
fn cursor_only_resume_skips_buffered_prefix_records_in_one_page() {
    let fixture = long_history(2_000);
    let length = fixture.file.metadata().unwrap().len();
    let mut cursor_only = fixture.source.subscribe(Some(1_999)).unwrap();
    assert!(
        cursor_only.checkpoint().is_none(),
        "an unlocated cursor has no invented byte hint"
    );
    let mut steps = 0;
    loop {
        steps += 1;
        match cursor_only.page().unwrap() {
            Item::Progress => assert!(
                steps <= (length / PAGE_BYTES as u64) + 2,
                "skipped records must share a page, not each return Progress"
            ),
            Item::Record(record) => {
                assert_eq!(record.seq, 2_000);
                break;
            }
            other => panic!("{other:?}"),
        }
    }
}
#[test]
fn lag_interrupts_replay_and_a_fenced_hint_resumes_without_reading_the_prefix() {
    let mut fixture = long_history(5_000);
    let mut cut = fixture.source.subscribe(None).unwrap();
    for expected in 1..=4_000 {
        assert_eq!(seq(next(&mut cut)), expected);
    }
    let hint = cut.checkpoint().unwrap();
    let read = cut.read_bytes();
    for seq in 5_001..=5_017 {
        fixture.append(record(seq, json!({"type":"user","message":{}})));
    }
    assert!(
        matches!(cut.page().unwrap(), Item::Lagged(5_017)),
        "lag must win over unread replay"
    );
    assert_eq!(
        cut.read_bytes(),
        read,
        "lagged cut must perform no more file reads"
    );
    let mut resumed = fixture.source.resume(4_000, &hint).unwrap();
    assert_eq!(resumed.offset, hint.offset);
    for expected in 4_001..=5_017 {
        assert_eq!(seq(next(&mut resumed)), expected);
    }
    assert!(matches!(next(&mut resumed), Item::Synced(5_017)));
    let suffix = fixture.file.metadata().unwrap().len() - hint.offset;
    assert!(
        resumed.read_bytes() <= suffix + PAGE_BYTES as u64,
        "resume reread the prefix: {} bytes for {suffix} suffix bytes",
        resumed.read_bytes()
    );
    assert!(fixture.source.resume(3_999, &hint).is_err());
    let other = long_history(5_000);
    assert!(other.source.resume(4_000, &hint).is_err());
}
#[test]
fn gap_and_live_checkpoints_preserve_exact_record_boundaries() {
    let mut fixture = Fixture::new(vec![record(5, json!({"type":"user","message":{}}))]);
    let mut cut = fixture.source.subscribe(None).unwrap();
    assert!(matches!(next(&mut cut), Item::Gap { from: 1, to: 4, .. }));
    let mut resumed = fixture
        .source
        .resume(4, &cut.checkpoint().unwrap())
        .unwrap();
    assert_eq!(seq(next(&mut resumed)), 5);
    assert!(matches!(next(&mut resumed), Item::Synced(5)));
    fixture.append(record(6, json!({"type":"user","message":{}})));
    assert_eq!(seq(next(&mut resumed)), 6);
    let mut latest = fixture
        .source
        .resume(6, &resumed.checkpoint().unwrap())
        .unwrap();
    assert!(matches!(latest.page().unwrap(), Item::Synced(6)));
    assert_eq!(latest.read_bytes(), 0);
}
#[test]
fn cold_tail_recovers_the_final_record_across_bounded_chunks() {
    let fixture = long_history(5_000);
    let length = fixture.file.metadata().unwrap().len();
    assert_eq!(tail_head(&fixture.file, length).unwrap(), 5_000);
    let fixture = Fixture::new(vec![record(
        1,
        json!({"type":"future","payload":"x".repeat(PAGE_BYTES*3)}),
    )]);
    assert_eq!(
        tail_head(&fixture.file, fixture.file.metadata().unwrap().len(),).unwrap(),
        1
    );
}

#[test]
fn one_oversized_publication_lags_a_caught_up_cut_without_copying_the_arc() {
    let mut fixture = Fixture::new(vec![]);
    let mut cut = fixture.source.subscribe(None).unwrap();
    assert!(matches!(cut.page().unwrap(), Item::Synced(0)));
    let record = Arc::new(record(
        1,
        json!({"type":"future","payload":"x".repeat(LIVE_BYTES)}),
    ));
    let bytes = serde_json::to_vec(&*record).unwrap();
    fixture.file.write_all(&bytes).unwrap();
    fixture.file.write_all(b"\n").unwrap();
    fixture.file.sync_all().unwrap();
    fixture
        .source
        .publish(record.clone(), bytes.len() + 1)
        .unwrap();
    assert_eq!(
        Arc::strong_count(&record),
        1,
        "oversize lag must retain no payload copy"
    );
    assert!(matches!(cut.page().unwrap(), Item::Lagged(1)));
}

#[test]
fn live_record_checkpoint_resumes_after_the_complete_record() {
    let mut fixture = Fixture::new(vec![record(1, json!({"type":"user","message":{}}))]);
    let mut cut = fixture.source.subscribe(Some(1)).unwrap();
    assert!(matches!(next(&mut cut), Item::Synced(1)));
    fixture.append(record(
        2,
        json!({"type":"user","message":{"content":"second"}}),
    ));
    assert_eq!(seq(next(&mut cut)), 2);
    let hint = cut.checkpoint().unwrap();
    let boundary = fixture.file.metadata().unwrap().len();
    assert_eq!(hint.offset, boundary);
    fixture.append(record(
        3,
        json!({"type":"user","message":{"content":"third"}}),
    ));
    let mut resumed = fixture.source.resume(2, &hint).unwrap();
    assert_eq!(seq(next(&mut resumed)), 3);
    assert!(matches!(next(&mut resumed), Item::Synced(3)));
}
#[test]
fn registered_at_head_checkpoint_survives_later_publication() {
    let mut fixture = Fixture::new(vec![record(1, json!({"type":"user","message":{}}))]);
    let cut = fixture.source.subscribe(Some(1)).unwrap();
    let hint = cut.checkpoint().unwrap();
    assert_eq!(hint.offset, fixture.file.metadata().unwrap().len());
    fixture.append(record(
        2,
        json!({"type":"user","message":{"content":"later"}}),
    ));
    let mut resumed = fixture.source.resume(1, &hint).unwrap();
    assert_eq!(seq(next(&mut resumed)), 2);
    assert!(matches!(next(&mut resumed), Item::Synced(2)));
}

#[test]
fn recovery_uses_the_same_captured_length_for_head_and_end() {
    let mut fixture = Fixture::new(vec![record(1, json!({"type":"assistant","message":{}}))]);
    let length = fixture.file.metadata().unwrap().len();
    let head = tail_head(&fixture.file, length).unwrap();
    // Deterministically append between the recovery capture and installation.
    let second = record(2, json!({"type":"user","message":{}}));
    let bytes = serde_json::to_vec(&second).unwrap();
    fixture.file.write_all(&bytes).unwrap();
    fixture.file.write_all(b"\n").unwrap();
    fixture.file.sync_all().unwrap();
    let source = History::default();
    source.initialize(&fixture.file, head, length).unwrap();
    let mut initial = source.subscribe(Some(1)).unwrap();
    assert!(matches!(initial.page().unwrap(), Item::Synced(1)));
    let hint = initial.checkpoint().unwrap();
    assert_eq!(hint.offset, length);
    source.publish(Arc::new(second), bytes.len() + 1).unwrap();
    let mut resumed = source.resume(1, &hint).unwrap();
    assert_eq!(seq(next(&mut resumed)), 2);
    assert!(matches!(next(&mut resumed), Item::Synced(2)));
}

#[test]
fn corrupt_wrappers_gap_and_checkpoint_without_replaying_prior_loss() {
    for invalid_json in [false, true] {
        let mut fixture = Fixture::new(
            (1..=4)
                .map(|seq| record(seq, json!({"type":"assistant","message":{}})))
                .collect(),
        );
        fixture.corrupt_wrapper(1, invalid_json);
        let mut cut = fixture.source.subscribe(None).unwrap();
        assert_eq!(seq(next(&mut cut)), 1);
        assert!(matches!(
            next(&mut cut),
            Item::Gap {
                from: 2,
                to: 2,
                reason: GapReason::CorruptWrapper
            }
        ));
        let hint = cut.checkpoint().unwrap();
        let mut resumed = fixture.source.resume(2, &hint).unwrap();
        assert_eq!(seq(next(&mut resumed)), 3);
        let mut scanned = fixture.source.subscribe(Some(2)).unwrap();
        assert_eq!(seq(next(&mut scanned)), 3);
        assert_eq!(seq(next(&mut scanned)), 4);
        assert!(matches!(next(&mut scanned), Item::Synced(4)));
        assert_eq!(seq(next(&mut cut)), 3);
        assert_eq!(seq(next(&mut cut)), 4);
        assert!(matches!(next(&mut cut), Item::Synced(4)));
    }
}

#[test]
fn corrupt_wrapper_at_captured_head_gaps_once_and_keeps_live_continuation() {
    let mut fixture = Fixture::new(vec![record(1, json!({"type":"assistant","message":{}}))]);
    fixture.corrupt_wrapper(0, true);
    let mut cut = fixture.source.subscribe(None).unwrap();
    fixture.append(record(2, json!({"type":"assistant","message":{}})));
    assert!(matches!(
        next(&mut cut),
        Item::Gap {
            from: 1,
            to: 1,
            reason: GapReason::CorruptWrapper
        }
    ));
    let hint = cut.checkpoint().unwrap();
    assert_eq!(hint.seq, 1);
    assert!(matches!(next(&mut cut), Item::Synced(1)));
    assert_eq!(seq(next(&mut cut)), 2);
    let mut resumed = fixture.source.resume(1, &hint).unwrap();
    assert_eq!(seq(next(&mut resumed)), 2);
    assert!(matches!(next(&mut resumed), Item::Synced(2)));
}

#[test]
fn corrupt_unsequenced_noise_does_not_consume_valid_sequences() {
    let mut fixture = Fixture::new(vec![
        record(1, json!({"type":"assistant","message":{}})),
        record(2, json!({"type":"assistant","message":{}})),
    ]);
    let path = fixture
        ._root
        .path()
        .join("sessions/fixture-agent/history.jsonl");
    let records = std::fs::read(path).unwrap();
    fixture.file.set_len(0).unwrap();
    fixture.file.write_all(b"unsequenced garbage\n").unwrap();
    fixture.file.write_all(&records).unwrap();
    fixture.file.sync_all().unwrap();
    fixture
        .source
        .initialize(&fixture.file, 2, fixture.file.metadata().unwrap().len())
        .unwrap();
    let mut cut = fixture.source.subscribe(None).unwrap();
    assert_eq!(seq(next(&mut cut)), 1);
    assert_eq!(seq(next(&mut cut)), 2);
    assert!(matches!(next(&mut cut), Item::Synced(2)));
}
