use super::*;
use crate::structured::{
    launch::SessionConfigDir,
    queue::{model::State, Change},
    transport::{StreamLocation, Transport},
};
use serde_json::Value;
use std::{
    fs,
    os::{fd::OwnedFd, unix::net::UnixStream},
    time::{Duration, Instant},
};
pub(in crate::structured::workers) fn root() -> SessionConfigDir {
    SessionConfigDir::create(&std::env::temp_dir()).unwrap()
}
fn item(n: u32) -> Item {
    let mut item =
        crate::structured::queue::tests::item(&format!("00000000-0000-4000-8000-{n:012}"));
    item.state = State::Held;
    item
}
pub(in crate::structured::workers) fn journal(root: &SessionConfigDir, n: u32) -> Journal {
    let mut j = Journal::open(root.path(), "publication-fixture").unwrap();
    for i in 0..n {
        j.append(Change::Upsert {
            item: Box::new(item(i)),
            position: i as usize,
        })
        .unwrap();
    }
    j
}
fn update(cut: Cut) -> Update {
    Update {
        facts: Vec::new(),
        admitted: None,
        offer: Ok(Offer::NoEligibleItems),
        pending: None,
        cut: Some(cut),
    }
}
pub(in crate::structured::workers) fn pages(reader: &mut Reader) -> Vec<Page> {
    let mut result = vec![];
    for _ in 0..1000 {
        let page = reader.page();
        if matches!(page, Page::Pending | Page::End) {
            return result;
        }
        result.push(page);
    }
    panic!("bounded in-memory replay did not finish");
}
pub(in crate::structured::workers) fn sequences(pages: &[Page]) -> Vec<u64> {
    pages
        .iter()
        .filter_map(|p| {
            if let Page::Record(v) = p {
                v["seq"].as_u64()
            } else {
                None
            }
        })
        .collect()
}
fn transport(root: &SessionConfigDir) -> (Transport, [UnixStream; 2]) {
    let (stdout, producer) = UnixStream::pair().unwrap();
    let (consumer, stdin) = UnixStream::pair().unwrap();
    let t = Transport::start(
        std::fs::File::from(OwnedFd::from(stdout)),
        std::fs::File::from(OwnedFd::from(stdin)),
        StreamLocation {
            data_dir: root.path().into(),
            agent: "input-fixture".into(),
            session: "input-session".into(),
        },
        "fixture-version".into(),
    )
    .unwrap();
    (t, [producer, consumer])
}
fn close(t: Transport) {
    t.cancel();
    let until = Instant::now() + Duration::from_secs(10);
    let mut done = 0;
    while done != 2 {
        match t.try_completion() {
            Ok(_) => done += 1,
            Err(mpsc::TryRecvError::Empty) => {
                assert!(Instant::now() < until, "transport failed to close");
                std::thread::yield_now();
            }
            Err(e) => panic!("missing producer completion: {e}"),
        }
    }
}
#[test]
fn queue_provider_capture_and_registration_are_one_critical_section() {
    let root = root();
    let mut j = journal(&root, 1);
    let (mut p, source) = Publisher::new("publication-fixture", j.replay(0).unwrap());
    let mut reader = source.subscribe(None).unwrap();
    p.capture(
        || {
            assert!(
                source.1.try_lock().is_none(),
                "snapshot callback escaped registration critical section"
            );
            j.subscription().map_err(Into::into)
        },
        None,
    );
    let first = pages(&mut reader);
    assert!(matches!(first.first(), Some(Page::Gap(_))));
    assert_eq!(sequences(&first), [2]);
    assert!(matches!(first.last(), Some(Page::Synced(2))));
    j.append(Change::Upsert {
        item: Box::new(item(2)),
        position: 1,
    })
    .unwrap();
    p.publish(&update(j.replay(2).unwrap()));
    assert_eq!(sequences(&pages(&mut reader)), [3]);
    let persisted =
        fs::read_to_string(root.path().join("sessions/publication-fixture/queue.jsonl")).unwrap();
    assert!(persisted
        .lines()
        .any(|v| serde_json::from_str::<Value>(v).unwrap()["kind"] == "queue_checkpoint"));
}
#[test]
fn queue_provider_replay_is_immutable_across_append_and_rewrite() {
    let root = root();
    let mut j = journal(&root, 2);
    let (mut p, source) = Publisher::new("publication-fixture", j.replay(0).unwrap());
    let mut reader = source.subscribe(None).unwrap();
    p.capture(|| j.subscription().map_err(Into::into), None);
    let barrier = j.replay(0).unwrap().through;
    j.append(Change::Upsert {
        item: Box::new(item(3)),
        position: 2,
    })
    .unwrap();
    p.publish(&update(j.replay(barrier).unwrap()));
    let replacement = j.checkpoint().unwrap();
    let high = replacement.through;
    p.publish(&update(replacement));
    let all = pages(&mut reader);
    let synced = all
        .iter()
        .position(|p| matches!(p, Page::Synced(n) if *n == barrier))
        .unwrap();
    assert_eq!(sequences(&all[..synced]), [3, 4]);
    assert_eq!(sequences(&all[synced + 1..]), [5, 6, 7, 8]);
    assert!(all
        .iter()
        .any(|p| matches!(p, Page::Gap(g) if g.snapshot_to == high)));
    assert_eq!(
        all.iter().filter(|p| matches!(p, Page::Synced(_))).count(),
        1
    );
}
#[test]
fn queue_provider_terminal_snapshot_is_bootstrapped_before_late_subscription() {
    let root = root();
    let (t, peers) = transport(&root);
    let (mut service, client) = Service::open(
        root.path(),
        "terminal-fixture",
        NonZeroU64::new(u64::MAX).unwrap(),
    )
    .unwrap();
    assert!(!service.finish(&t.input_handle(), &|| {}));
    drop(service);
    let text =
        fs::read_to_string(root.path().join("sessions/terminal-fixture/queue.jsonl")).unwrap();
    assert!(
        text.lines()
            .any(|s| serde_json::from_str::<Value>(s).unwrap()["kind"] == "queue_checkpoint"),
        "terminal source served an unpersisted snapshot"
    );
    let mut reader = client.source.subscribe(None).unwrap();
    let all = pages(&mut reader);
    assert!(all
        .iter()
        .any(|p| matches!(p, Page::Gap(g) if g.snapshot_to == 0)));
    assert!(all.iter().any(|p| matches!(p, Page::Synced(0))));
    assert!(matches!(reader.page(), Page::End));
    close(t);
    drop(peers);
}
#[test]
fn queue_provider_bootstrap_refusal_never_claims_a_snapshot_or_sync() {
    let root = root();
    let mut j = journal(&root, 0);
    use std::io::Write;
    drop(j);
    let path = root.path().join("sessions/publication-fixture/queue.jsonl");
    let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(f, "{{\"seq\":1,\"kind\":\"future\"}}").unwrap();
    f.sync_all().unwrap();
    drop(f);
    j = Journal::open(root.path(), "publication-fixture").unwrap();
    let before = fs::read(&path).unwrap();
    let (mut p, source) = Publisher::new("publication-fixture", j.replay(0).unwrap());
    let mut reader = source.subscribe(None).unwrap();
    p.capture(|| j.subscription().map_err(Into::into), None);
    assert!(matches!(
        reader.page(),
        Page::Refused(ErrorCode::QueueJournalCorrupt)
    ));
    assert!(matches!(reader.page(), Page::End));
    let mut again = source.subscribe(None).unwrap();
    p.capture(|| panic!("failed bootstrap was retried"), None);
    assert!(matches!(
        again.page(),
        Page::Refused(ErrorCode::QueueJournalCorrupt)
    ));
    assert_eq!(fs::read(path).unwrap(), before);
}
#[test]
fn queue_provider_ahead_cursor_and_producer_loss_close_explicitly() {
    let root = root();
    let j = journal(&root, 0);
    let (p, source) = Publisher::new("publication-fixture", j.replay(0).unwrap());
    assert!(matches!(
        source.subscribe(Some(100)),
        Err(ErrorCode::CursorAhead)
    ));
    let mut waiting = source.subscribe(None).unwrap();
    drop(p);
    assert!(matches!(
        waiting.page(),
        Page::Refused(ErrorCode::InternalError)
    ));
    assert!(matches!(waiting.page(), Page::End));
}
#[test]
fn queue_provider_zero_read_paging_and_tail_resume_survive_file_removal() {
    let root = root();
    let mut j = journal(&root, 100);
    let (mut p, source) = Publisher::new("publication-fixture", j.replay(0).unwrap());
    let mut reader = source.subscribe(None).unwrap();
    p.capture(|| j.subscription().map_err(Into::into), None);
    let head = j.replay(0).unwrap().through;
    // Quiescent retained source: all future reads would fail. Reader/Source own
    // decoded immutable cuts, not a file descriptor or a recoverable pathname.
    source.1.lock().ended = true;
    drop(p);
    drop(j);
    fs::remove_file(root.path().join("sessions/publication-fixture/queue.jsonl")).unwrap();
    let mut tail = source.subscribe(Some(head - 1)).unwrap();
    assert_eq!(tail.current.as_ref().map_or(0, |p| p.0.records.len()), 0);
    let full = pages(&mut reader);
    let resumed = pages(&mut tail);
    assert_eq!(sequences(&full).len(), 100);
    assert_eq!(sequences(&resumed), [head]);
    assert!(
        resumed.len() <= 3,
        "resume rescanned the prefix instead of seeking its index"
    );
    assert!(matches!(resumed.last(), Some(Page::Synced(n)) if *n == head));
    assert!(matches!(tail.page(), Page::End));
}
#[test]
fn queue_provider_overflow_is_lagged_and_never_silent_loss() {
    let root = root();
    let mut j = journal(&root, 0);
    let (mut p, source) = Publisher::new("publication-fixture", j.replay(0).unwrap());
    let mut reader = source.subscribe(None).unwrap();
    p.capture(|| j.subscription().map_err(Into::into), None);
    pages(&mut reader);
    for n in 0..=LIVE_CUTS {
        let cursor = j.replay(0).unwrap().through;
        j.append(Change::Upsert {
            item: Box::new(item(n as u32)),
            position: n,
        })
        .unwrap();
        p.publish(&update(j.replay(cursor).unwrap()));
    }
    assert!(matches!(reader.page(), Page::Lagged(n) if n == j.replay(0).unwrap().through));
    assert!(matches!(reader.page(), Page::End));
}
#[test]
fn queue_provider_same_code_different_source_emits_both_completions() {
    let root = root();
    let (t, peers) = transport(&root);
    let (mut service, client) = Service::open(
        root.path(),
        "failure-fixture",
        NonZeroU64::new(u64::MAX).unwrap(),
    )
    .unwrap();
    for source in ["entropy source", "validation source"] {
        assert!(!service.publish(
            &t.input_handle(),
            true,
            None,
            Some(Failure {
                code: ErrorCode::InternalError,
                source: Some(io::Error::other(source))
            })
        ));
        service.flush(false, &|| {});
        let update = client
            .update()
            .expect("distinct source failure was suppressed");
        assert_eq!(
            update.offer.unwrap_err().source.unwrap().to_string(),
            source
        );
    }
    close(t);
    drop(peers);
}

#[test]
fn queue_provider_empty_replacement_and_overlapping_replay_are_not_lost_or_duplicated() {
    let root = root();
    let mut j = journal(&root, 1);
    let (mut p, source) = Publisher::new("publication-fixture", j.replay(0).unwrap());
    let mut reader = source.subscribe(None).unwrap();
    p.capture(|| j.subscription().map_err(Into::into), None);
    pages(&mut reader);
    j.append(Change::Upsert {
        item: Box::new(item(1)),
        position: 1,
    })
    .unwrap();
    // Service's completion cursor may still precede subscription bootstrap.
    p.publish(&update(j.replay(0).unwrap()));
    assert_eq!(sequences(&pages(&mut reader)), [3]);
    j.append(Change::Remove {
        item_id: item(0).item_id,
    })
    .unwrap();
    j.append(Change::Remove {
        item_id: item(1).item_id,
    })
    .unwrap();
    p.publish(&update(j.replay(3).unwrap()));
    pages(&mut reader);
    let empty = j.checkpoint().unwrap();
    let through = empty.through;
    assert!(empty.records.is_empty());
    p.publish(&update(empty));
    let replacement = pages(&mut reader);
    assert!(replacement
        .iter()
        .any(|p| matches!(p, Page::Gap(g) if g.snapshot_to == through)));
    let mut resumed = source.subscribe(None).unwrap();
    p.capture(|| j.subscription().map_err(Into::into), None);
    let snapshot = pages(&mut resumed);
    assert!(sequences(&snapshot).is_empty());
    assert!(matches!(snapshot.last(), Some(Page::Synced(n)) if *n == through));
}

#[test]
fn queue_provider_byte_bound_and_uncertain_durability_close_explicitly() {
    let root = root();
    let mut j = journal(&root, 0);
    let (mut p, source) = Publisher::new("publication-fixture", j.replay(0).unwrap());
    let mut reader = source.subscribe(None).unwrap();
    p.capture(|| j.subscription().map_err(Into::into), None);
    pages(&mut reader);
    let mut large = item(1);
    large.body = "x".repeat(LIVE_BYTES);
    j.append(Change::Upsert {
        item: Box::new(large),
        position: 0,
    })
    .unwrap();
    p.publish(&update(j.replay(0).unwrap()));
    assert!(matches!(reader.page(), Page::Lagged(_)));
    let mut failure = source
        .subscribe(Some(j.replay(0).unwrap().through))
        .unwrap();
    p.capture(|| j.subscription().map_err(Into::into), None);
    pages(&mut failure);
    p.publish(&Update {
        facts: Vec::new(),
        admitted: None,
        offer: Err(Failure {
            code: ErrorCode::InputWriteUnknown,
            source: None,
        }),
        pending: None,
        cut: None,
    });
    assert!(matches!(
        failure.page(),
        Page::Refused(ErrorCode::InputWriteUnknown)
    ));
    assert!(matches!(failure.page(), Page::End));
}

#[tokio::test]
async fn queue_provider_ready_and_live_wakes_follow_lock_release() {
    let root = root();
    let mut j = journal(&root, 0);
    let (mut p, source) = Publisher::new("publication-fixture", j.replay(0).unwrap());
    let mut reader = source.subscribe(None).unwrap();
    let notify = reader.waker();
    let wake = notify.notified();
    tokio::pin!(wake);
    wake.as_mut().enable();
    p.capture(|| j.subscription().map_err(Into::into), None);
    tokio::time::timeout(Duration::from_secs(5), wake)
        .await
        .expect("capture never woke its registered consumer");
    assert!(source.1.try_lock().is_some());
    pages(&mut reader);
    let wake = notify.notified();
    tokio::pin!(wake);
    wake.as_mut().enable();
    j.append(Change::Upsert {
        item: Box::new(item(1)),
        position: 0,
    })
    .unwrap();
    p.publish(&update(j.replay(0).unwrap()));
    tokio::time::timeout(Duration::from_secs(5), wake)
        .await
        .expect("live publication never woke its registered consumer");
    assert_eq!(sequences(&pages(&mut reader)), [1]);
}

thread_local! {
    static REGISTRATION_WITNESS: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static UNLOCK_WITNESS: std::cell::RefCell<Option<Arc<Mutex<Live>>>> = const { std::cell::RefCell::new(None) };
}
struct Witness(parking_lot::RawMutex);
// SAFETY: exclusion and guard semantics are delegated unchanged to RawMutex.
unsafe impl parking_lot::lock_api::RawMutex for Witness {
    const INIT: Self = Self(<parking_lot::RawMutex as parking_lot::lock_api::RawMutex>::INIT);
    type GuardMarker = <parking_lot::RawMutex as parking_lot::lock_api::RawMutex>::GuardMarker;
    fn lock(&self) {
        parking_lot::lock_api::RawMutex::lock(&self.0);
    }
    fn try_lock(&self) -> bool {
        parking_lot::lock_api::RawMutex::try_lock(&self.0)
    }
    unsafe fn unlock(&self) {
        // SAFETY: the lock_api guard guarantees its paired successful lock.
        unsafe {
            parking_lot::lock_api::RawMutex::unlock(&self.0);
        }
        REGISTRATION_WITNESS.with(|w| {
            let observe = w.borrow_mut().take();
            if let Some(observe) = observe {
                observe();
            }
        });
        UNLOCK_WITNESS.with(|w| {
            if let Some(live) = w.borrow().as_ref() {
                assert!(
                    !live.lock().packets.is_empty(),
                    "capture released its lock before continuation registration"
                );
            }
        });
    }
}
#[test]
fn queue_provider_split_lock_mutation_is_observed_without_racing_threads() {
    let root = root();
    let mut j = journal(&root, 1);
    let book = parking_lot::lock_api::Mutex::<Witness, _>::new(Book {
        retained: Arc::new(j.replay(0).unwrap()),
        bootstrapped: false,
        status: None,
        error: None,
        ended: false,
        refusal: None,
        readers: vec![],
    });
    let live = Arc::new(Mutex::new(Live::default()));
    capture_locked(
        &book,
        Capture(None, live.clone()),
        || {
            let result = j.subscription().map_err(Into::into);
            UNLOCK_WITNESS.with(|w| *w.borrow_mut() = Some(live.clone()));
            result
        },
        None,
    );
    UNLOCK_WITNESS.with(|w| *w.borrow_mut() = None);
    assert_eq!(book.lock().readers.len(), 1);
    assert!(matches!(
        live.lock().packets.front(),
        Some(Packet(_, _, _, true, 0))
    ));
}

#[test]
fn queue_provider_active_resume_never_recaptures_or_rescans_the_prefix() {
    let root = root();
    let mut j = journal(&root, 100);
    let (mut publisher, source) = Publisher::new("publication-fixture", j.replay(0).unwrap());
    let mut initial = source.subscribe(None).unwrap();
    publisher.capture(|| j.subscription().map_err(Into::into), None);
    let full = pages(&mut initial);
    let head = full
        .iter()
        .find_map(|p| {
            if let Page::Synced(n) = p {
                Some(*n)
            } else {
                None
            }
        })
        .unwrap();
    let mut resumed = source.subscribe(Some(head - 1)).unwrap();
    publisher.capture(
        || panic!("resume recaptured or rescanned the already cached prefix"),
        None,
    );
    let tail = pages(&mut resumed);
    assert_eq!(sequences(&tail), [head]);
    assert!(tail.len() <= 3);
}

#[test]
fn queue_provider_ahead_cursor_cannot_become_valid_during_bootstrap() {
    let root = root();
    let mut j = journal(&root, 1);
    assert_eq!(j.replay(0).unwrap().through, 1);
    let path = root.path().join("sessions/publication-fixture/queue.jsonl");
    let before = fs::read(&path).unwrap();
    let (mut publisher, source) = Publisher::new("publication-fixture", j.replay(0).unwrap());
    let mut request = source.subscribe(Some(2));
    let observed = if let Ok(reader) = &mut request {
        publisher.capture(|| j.subscription().map_err(Into::into), None);
        pages(reader)
    } else {
        vec![]
    };
    assert!(
        matches!(request, Err(ErrorCode::CursorAhead)),
        "ahead cursor became valid during bootstrap: {observed:?}"
    );
    assert_eq!(fs::read(path).unwrap(), before);
}

#[test]
fn queue_provider_bootstrap_and_real_worker_cursor_do_not_duplicate_a_large_snapshot() {
    let root = root();
    let mut j = journal(&root, 0);
    let mut large = item(0);
    large.body = "x".repeat(LIVE_BYTES + 1);
    j.append(Change::Upsert {
        item: Box::new(large),
        position: 0,
    })
    .unwrap();
    let worker_cursor = j.replay(0).unwrap().through;
    let (mut publisher, source) = Publisher::new("publication-fixture", j.replay(0).unwrap());
    let mut reader = source.subscribe(None).unwrap();
    publisher.capture(|| j.subscription().map_err(Into::into), None);
    assert_eq!(sequences(&pages(&mut reader)), [2]);
    publisher.publish(&update(j.replay(worker_cursor).unwrap()));
    let next = pages(&mut reader);
    assert!(sequences(&next).is_empty(), "duplicate snapshot: {next:?}");
    assert!(
        !next
            .iter()
            .any(|p| matches!(p, Page::Gap(_) | Page::Lagged(_))),
        "bootstrap cursor mismatch: {next:?}"
    );
}

#[test]
fn queue_provider_initial_replay_does_not_consume_the_four_live_cut_slots() {
    let root = root();
    let mut j = journal(&root, 0);
    let (mut publisher, source) = Publisher::new("publication-fixture", j.replay(0).unwrap());
    let mut reader = source.subscribe(None).unwrap();
    publisher.capture(|| j.subscription().map_err(Into::into), None);
    for n in 0..LIVE_CUTS {
        let cursor = j.replay(0).unwrap().through;
        j.append(Change::Upsert {
            item: Box::new(item(n as u32)),
            position: n,
        })
        .unwrap();
        publisher.publish(&update(j.replay(cursor).unwrap()));
    }
    let all = pages(&mut reader);
    assert_eq!(sequences(&all), [1, 2, 3, 4]);
    assert!(!all.iter().any(|p| matches!(p, Page::Lagged(_))));
}

#[test]
fn queue_provider_readers_do_not_wait_on_another_readers_book_access() {
    let root = root();
    let mut j = journal(&root, 1);
    let (mut publisher, source) = Publisher::new("publication-fixture", j.replay(0).unwrap());
    let mut first = source.subscribe(None).unwrap();
    publisher.capture(|| j.subscription().map_err(Into::into), None);
    let mut second = source.subscribe(None).unwrap();
    publisher.capture(|| panic!("unexpected recapture"), None);
    // Deterministically hold the shared lock that used to serialize all pages.
    // Neither reader needs this lock once its packet has been registered.
    let _held = source.1.lock();
    assert!(matches!(first.page(), Page::Gap(_)));
    assert!(matches!(second.page(), Page::Gap(_)));
}

#[test]
fn queue_provider_bootstrap_status_preserves_the_last_operation_error() {
    let root = root();
    let mut j = journal(&root, 1);
    let (mut publisher, source) = Publisher::new("publication-fixture", j.replay(0).unwrap());
    let mut failed = update(j.replay(0).unwrap());
    failed.offer = Err(Failure {
        code: ErrorCode::InternalError,
        source: None,
    });
    publisher.publish(&failed);
    let mut reader = source.subscribe(None).unwrap();
    publisher.capture(|| j.subscription().map_err(Into::into), None);
    assert!(pages(&mut reader)
        .iter()
        .any(|p| matches!(p, Page::Status(_, Some(ErrorCode::InternalError)))));
}

#[test]
fn queue_provider_final_status_preserves_the_last_operation_error() {
    let root = root();
    let mut j = journal(&root, 1);
    let (mut publisher, source) = Publisher::new("publication-fixture", j.replay(0).unwrap());
    let mut reader = source.subscribe(None).unwrap();
    publisher.capture(|| j.subscription().map_err(Into::into), None);
    pages(&mut reader);
    let mut failed = update(j.replay(2).unwrap());
    failed.offer = Err(Failure {
        code: ErrorCode::InternalError,
        source: None,
    });
    publisher.publish(&failed);
    pages(&mut reader);
    publisher.finish(|| j.checkpoint().map_err(Into::into), None);
    assert!(pages(&mut reader)
        .iter()
        .any(|p| matches!(p, Page::Status(_, Some(ErrorCode::InternalError)))));
}

#[test]
fn queue_provider_status_only_publish_never_copies_the_retained_snapshot() {
    let root = root();
    let mut j = journal(&root, 2);
    let (mut publisher, source) = Publisher::new("publication-fixture", j.replay(0).unwrap());
    let _reader = source.subscribe(None).unwrap();
    publisher.capture(|| j.subscription().map_err(Into::into), None);
    let retained = source.1.lock().retained.clone();
    publisher.publish(&update(j.replay(2).unwrap()));
    assert!(
        Arc::ptr_eq(&retained, &source.1.lock().retained),
        "status update cloned the entire retained snapshot"
    );
}

#[test]
fn queue_provider_late_capture_has_synced_before_end_at_every_registration_unlock() {
    let root = root();
    let mut j = journal(&root, 2);
    let (mut publisher, source) = Publisher::new("publication-fixture", j.replay(0).unwrap());
    let mut reader = source.subscribe(None).unwrap();
    publisher.finish(|| j.subscription().map_err(Into::into), None);
    assert!(matches!(reader.page(), Page::Pending));
    // At each registration unlock, observe exactly what a pager could observe.
    // This delegates exclusion to the native mutex, with no racing threads.
    let witnessed = Arc::new(parking_lot::lock_api::Mutex::<Witness, _>::new(
        Live::default(),
    ));
    let watched = witnessed.clone();
    REGISTRATION_WITNESS.with(|w| {
        *w.borrow_mut() = Some(Box::new(move || {
            let live = watched
                .try_lock()
                .expect("witness ran before releasing the lock");
            assert!(
                !live.ended || !live.packets.is_empty(),
                "reader could return End before replay/Synced"
            );
        }))
    });
    let retained = source.1.lock().retained.clone();
    let mut book = Book::<Witness> {
        retained,
        bootstrapped: true,
        status: None,
        error: None,
        ended: true,
        refusal: None,
        readers: vec![],
    };
    book.register(None, witnessed).unwrap();
    REGISTRATION_WITNESS
        .with(|w| assert!(w.borrow().is_none(), "registration unlock was not observed"));
    drop(publisher);
    let replay = pages(&mut reader);
    assert_eq!(sequences(&replay), [3, 4]);
    assert!(matches!(replay.last(), Some(Page::Synced(4))));
    assert!(matches!(reader.page(), Page::End));
}

#[test]
fn queue_provider_high_water_advances_even_without_a_record_packet() {
    let root = root();
    let j = journal(&root, 0);
    let (mut publisher, source) = Publisher::new("publication-fixture", j.replay(0).unwrap());
    publisher.publish(&update(Cut {
        gap: None,
        records: vec![],
        through: 5,
    }));
    assert_eq!(source.1.lock().retained.through, 5);
}

#[test]
fn queue_provider_high_water_never_rewinds_on_a_stale_status_cut() {
    let root = root();
    let j = journal(&root, 0);
    let (mut publisher, source) = Publisher::new("publication-fixture", j.replay(0).unwrap());
    for through in [5, 4, 0] {
        publisher.publish(&update(Cut {
            gap: None,
            records: vec![],
            through,
        }));
        assert_eq!(
            source.1.lock().retained.through,
            5,
            "rewound retained high-water"
        );
    }
}

#[test]
fn queue_global_terminal_facts_keep_order_if_newer_cut_finds_capacity() {
    use crate::api::queue_event::{QueueItemState, QueueState};
    let root = root();
    let (mut service, client) = Service::open(
        root.path(),
        "publication-fixture",
        std::num::NonZeroU64::new(u64::MAX).unwrap(),
    )
    .unwrap();
    let make = |seq| {
        let mut update = update(Cut {
            gap: None,
            records: vec![],
            through: seq,
        });
        update.facts.push(QueueItemState {
            agent: "publication-fixture".into(),
            item_id: format!("item-{seq}"),
            batch_id: None,
            producer: crate::structured::queue::model::Producer::Human,
            from: QueueState::Delivered,
            to: QueueState::Held,
            stream: "queue/publication-fixture".into(),
            stream_seq: seq,
        });
        update
    };
    for seq in 1..=5 {
        service.pending = Some(make(seq));
        service.flush(true, &|| ());
    }
    let mut seqs = client
        .update()
        .unwrap()
        .facts
        .into_iter()
        .map(|f| f.stream_seq)
        .collect::<Vec<_>>();
    service.pending = Some(make(6));
    service.flush(true, &|| ());
    while let Some(update) = client.update() {
        seqs.extend(update.facts.into_iter().map(|f| f.stream_seq));
    }
    assert_eq!(seqs, [1, 2, 3, 4, 5, 6]);
    assert!(!client.has_updates());
}

#[test]
fn queue_global_empty_capture_wakes_app_once_without_a_new_completion() {
    let root = root();
    let (mut service, client) = Service::open(
        root.path(),
        "publication-fixture",
        std::num::NonZeroU64::new(u64::MAX).unwrap(),
    )
    .unwrap();
    let mut reader = client.source.subscribe(None).unwrap();
    let wakes = std::cell::Cell::new(0);
    let notify = || wakes.set(wakes.get() + 1);
    service.capture(&notify);
    assert_eq!(wakes.get(), 1);
    assert!(pages(&mut reader)
        .iter()
        .any(|p| matches!(p, Page::Synced(0))));
    service.capture(&notify);
    assert_eq!(wakes.get(), 1, "idle owner must not generate a wake loop");
}
