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
struct Fixture {
    _root: SessionConfigDir,
    file: File,
    source: History,
}
impl Fixture {
    fn new(records: Vec<StreamRecord>) -> Self {
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
        source
            .initialize(&file, records.last().map_or(0, |r| r.seq))
            .unwrap();
        Self {
            _root: root,
            file,
            source,
        }
    }
    fn append(&mut self, record: StreamRecord) {
        let bytes = serde_json::to_vec(&record).unwrap();
        self.file.write_all(&bytes).unwrap();
        self.file.write_all(b"\n").unwrap();
        self.file.sync_all().unwrap();
        self.source.publish(record, bytes.len() + 1).unwrap();
    }
}
fn next(cut: &mut Cut) -> Item {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let item = cut.page().unwrap();
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
        assert!(matches!(next(&mut cut), Item::Synced(0)));
        assert!(matches!(next(&mut cut),Item::Lagged(to) if to==count as u64));
        assert!(matches!(next(&mut cut), Item::End));
        assert!(cut.live.lock().queue.is_empty());
        assert_eq!(cut.live.lock().bytes, 0);
        let mut fresh = fixture.source.subscribe(None).unwrap();
        assert_eq!(seq(next(&mut fresh)), 1);
    }
}
#[test]
fn atomic_registration_under_a_racing_publisher_never_loses_a_record() {
    for _ in 0..32 {
        let mut fixture = Fixture::new(vec![]);
        let source = fixture.source.clone();
        let (go, start) = std::sync::mpsc::sync_channel(1);
        let (done, ack) = std::sync::mpsc::sync_channel(1);
        let producer = std::thread::spawn(move || {
            start
                .recv_timeout(Duration::from_secs(5))
                .expect("subscriber must start race");
            fixture.append(record(1, json!({"type":"user","message":{}})));
            done.send(fixture)
                .unwrap_or_else(|_| panic!("subscriber disappeared"))
        });
        go.try_send(()).expect("racing publisher must remain live");
        let mut cut = source.subscribe(None).unwrap();
        let _fixture = ack
            .recv_timeout(Duration::from_secs(5))
            .expect("publication producer remains bounded");
        producer.join().unwrap();
        let mut seen = Vec::new();
        let mut barriers = 0;
        for _ in 0..3 {
            match cut.page().unwrap() {
                Item::Record(record) => seen.push(record.seq),
                Item::Synced(_) => barriers += 1,
                Item::Pending | Item::Progress => {}
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(seen, vec![1]);
        assert_eq!(barriers, 1);
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
    source.initialize(&fixture.file, 0).unwrap();
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
