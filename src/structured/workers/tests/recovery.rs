use super::*;
use crate::structured::{
    publication::{Provider, Subscription},
    queue::{model::State, Change, Journal, RewritePolicy},
    workers::queue::{
        publication::{
            tests::{pages, sequences},
            Page,
        },
        recovery::{Recovered, Recovery, Step},
    },
};
use std::num::NonZeroU64;
fn policy() -> RewritePolicy {
    RewritePolicy::new(NonZeroU64::new(u64::MAX).unwrap())
}
fn seed(case: &Case) -> Vec<u8> {
    let mut j = Journal::open(case.fixture.root.path(), &case.id).unwrap();
    let mut item = crate::structured::queue::tests::item("00000000-0000-4000-8000-000000000001");
    item.state = State::Held;
    j.append(Change::Upsert {
        item: Box::new(item),
        position: 0,
    })
    .unwrap();
    j.checkpoint().unwrap();
    drop(j);
    fs::read(
        case.fixture
            .root
            .path()
            .join("sessions")
            .join(&case.id)
            .join("queue.jsonl"),
    )
    .unwrap()
}
async fn loaded(mut recovery: Recovery) -> Recovered {
    let mut result = None;
    wait(&recovery.waker(), || match recovery.poll() {
        Step::Pending => false,
        Step::Ready(source) => {
            result = Some(source);
            true
        }
        Step::Refused(error) => panic!("I/O producer refused: {error:?}"),
        Step::Closed => panic!("I/O producer closed without a completion"),
    })
    .await;
    assert!(matches!(recovery.poll(), Step::Closed));
    result.unwrap()
}
#[tokio::test]
async fn cold_queue_worker_recovers_and_installs_the_original_replay() {
    let (mut registry, mut workers, _, _) = owner();
    let (case, _, _) = Case::prepare(&mut registry, 941, "idle", vec![]);
    let bytes = seed(&case);
    assert!(matches!(
        workers.queue_source(&registry, &case.id, case.generation),
        Err(ErrorCode::QueueSourceCold)
    ));
    let pending = workers
        .recover_queue(
            &registry,
            case.fixture.root.path().into(),
            &case.id,
            case.generation,
            policy(),
        )
        .unwrap();
    assert_eq!(workers.recovery_slot.as_ref().unwrap().0, case.id);
    let recovered = loaded(pending).await;
    assert!(matches!(
        workers.queue_source(&registry, &case.id, case.generation),
        Err(ErrorCode::QueueSourceCold)
    ));
    recovered.install(&mut workers, &registry).unwrap();
    let source = workers
        .queue_source(&registry, &case.id, case.generation)
        .unwrap();
    let mut reader = source.subscribe(None).unwrap();
    let replay = pages(&mut reader);
    assert_eq!(sequences(&replay), [2]);
    assert!(matches!(replay.last(), Some(Page::Synced(2))));
    assert!(matches!(reader.page(), Page::End));
    let path = case
        .fixture
        .root
        .path()
        .join("sessions")
        .join(&case.id)
        .join("queue.jsonl");
    assert_eq!(fs::read(&path).unwrap(), bytes);
    fs::remove_file(path).unwrap();
    let mut resumed = source.subscribe(Some(1)).unwrap();
    assert_eq!(sequences(&pages(&mut resumed)), [2]);
}
#[tokio::test]
async fn cold_queue_competing_completions_keep_the_first_canonical_source() {
    let (mut registry, mut workers, _, _) = owner();
    let (case, _, _) = Case::prepare(&mut registry, 942, "idle", vec![]);
    seed(&case);
    let mut ready = Vec::new();
    for _ in 0..2 {
        let recovery = workers
            .recover_queue(
                &registry,
                case.fixture.root.path().into(),
                &case.id,
                case.generation,
                policy(),
            )
            .unwrap();
        ready.push(loaded(recovery).await);
    }
    let second = ready.pop().unwrap();
    ready
        .pop()
        .unwrap()
        .install(&mut workers, &registry)
        .unwrap();
    let first = workers
        .queue_source(&registry, &case.id, case.generation)
        .unwrap()
        .subscribe(None)
        .unwrap();
    assert_eq!(
        second.install(&mut workers, &registry),
        Err(ErrorCode::SessionBusy)
    );
    let canonical = workers
        .queue_source(&registry, &case.id, case.generation)
        .unwrap()
        .subscribe(None)
        .unwrap();
    assert!(Arc::ptr_eq(&first.waker(), &canonical.waker()));
}
#[tokio::test]
async fn cold_queue_install_rejects_retired_generations() {
    let (mut registry, mut workers, _, _) = owner();
    let (case, _, _) = Case::prepare(&mut registry, 943, "idle", vec![]);
    seed(&case);
    let recovery = workers
        .recover_queue(
            &registry,
            case.fixture.root.path().into(),
            &case.id,
            case.generation,
            policy(),
        )
        .unwrap();
    let recovered = loaded(recovery).await;
    let next = registry.begin_launch(&case.id, case.generation).unwrap();
    assert_eq!(
        recovered.install(&mut workers, &registry),
        Err(ErrorCode::AgentNotFound)
    );
    assert!(matches!(
        workers.queue_source(&registry, &case.id, next),
        Err(ErrorCode::QueueSourceCold)
    ));
}
#[tokio::test]
async fn cold_queue_install_rejects_a_different_namespace() {
    let (mut registry, mut workers, _, _) = owner();
    let (first, _, _) = Case::prepare(&mut registry, 944, "idle", vec![]);
    let (other, _, _) = Case::prepare(&mut registry, 945, "idle", vec![]);
    seed(&other);
    let recovery = workers
        .recover_queue(
            &registry,
            other.fixture.root.path().into(),
            &other.id,
            other.generation,
            policy(),
        )
        .unwrap();
    loaded(recovery)
        .await
        .install(&mut workers, &registry)
        .unwrap();
    let source = workers
        .queue_source(&registry, &other.id, other.generation)
        .unwrap();
    assert_eq!(
        workers.install_queue_source(&registry, &first.id, first.generation, source),
        Err(ErrorCode::InvalidRequest)
    );
}
#[tokio::test]
async fn cold_queue_same_agent_launch_waits_for_the_io_slot() {
    let (mut registry, mut workers, notify, _) = owner();
    let (case, launch, factory) = Case::prepare(&mut registry, 946, "idle", vec![]);
    seed(&case);
    let slot = Arc::new(());
    workers.recovery_slot = Some((case.id.clone(), Arc::downgrade(&slot)));
    let launched = workers.start(&mut registry, launch, factory);
    if let Ok(generation) = launched {
        workers.stop(&case.id, generation);
        wait(&notify, || {
            workers.poll(&mut registry, Instant::now(), 1000);
            workers.report(&case.id, generation).is_some()
        })
        .await;
    }
    assert_eq!(launched, Err(ErrorCode::SessionBusy));
    drop(slot);
}
#[tokio::test]
async fn cold_queue_preparation_owner_is_rechecked_at_admission_and_installation() {
    let (mut registry, mut workers, notify, _) = owner();
    let (case, launch, factory) = Case::prepare(&mut registry, 947, "idle", vec![]);
    seed(&case);
    let (entered, entered_rx) = mpsc::sync_channel(1);
    let (release, release_rx) = mpsc::sync_channel(1);
    let generation = workers
        .start(&mut registry, launch, move |record| {
            entered.send(()).unwrap();
            release_rx.recv_timeout(LIMIT).map_err(io::Error::other)?;
            factory(record)
        })
        .unwrap();
    entered_rx.recv_timeout(LIMIT).unwrap();
    let attempt = workers.recover_queue(
        &registry,
        case.fixture.root.path().into(),
        &case.id,
        generation,
        policy(),
    );
    let refused = match attempt {
        Err(error) => Some(error.code),
        Ok(recovery) => {
            drop(loaded(recovery).await);
            None
        }
    };
    // Direct I/O completion models an already-issued result arriving after an
    // owner appeared. Installation must independently recheck that owner.
    let recovery = Recovery::start(
        case.fixture.root.path().into(),
        case.id.clone(),
        generation,
        policy(),
        Arc::new(()),
    )
    .unwrap();
    let installed = loaded(recovery).await.install(&mut workers, &registry);
    release.send(()).unwrap();
    workers.stop(&case.id, generation);
    wait(&notify, || {
        workers.poll(&mut registry, Instant::now(), 1000);
        workers.report(&case.id, generation).is_some()
    })
    .await;
    assert_eq!(
        (refused, installed),
        (Some(ErrorCode::SessionBusy), Err(ErrorCode::SessionBusy))
    );
}
#[tokio::test]
async fn cold_queue_live_producer_wins_before_an_older_completion_installs() {
    let (mut registry, mut workers, notify, _) = owner();
    let (case, launch, factory) = Case::prepare(&mut registry, 948, "idle", vec![]);
    seed(&case);
    let recovery = workers
        .recover_queue(
            &registry,
            case.fixture.root.path().into(),
            &case.id,
            case.generation,
            policy(),
        )
        .unwrap();
    let completed = loaded(recovery).await;
    let generation = workers.start(&mut registry, launch, factory).unwrap();
    let mut live = None;
    let mut initial_published = false;
    wait(&notify, || {
        workers.poll(&mut registry, Instant::now(), 1000);
        assert!(
            workers.report(&case.id, generation).is_none(),
            "producer ended before initial publication"
        );
        while workers
            .queue_update(&registry, &case.id, generation)
            .is_some()
        {
            initial_published = true;
        }
        live = workers.queue_source(&registry, &case.id, generation).ok();
        initial_published && live.is_some()
    })
    .await;
    let result = completed.install(&mut workers, &registry);
    let mut reader = live.unwrap().subscribe(None).unwrap();
    let wake = reader.waker();
    let mut synced = false;
    wait(&wake, || loop {
        match reader.page() {
            Page::Synced(2) => {
                synced = true;
                return true;
            }
            Page::Pending => return false,
            Page::Refused(code) => panic!("live source refused: {code:?}"),
            Page::End => panic!("live source ended before sync"),
            _ => (),
        }
    })
    .await;
    workers
        .enqueue(
            &registry,
            &case.id,
            generation,
            crate::structured::workers::queue::Request {
                item: crate::structured::queue::tests::item("00000000-0000-4000-8000-000000000002"),
                principal: crate::structured::queue::admission::Principal::Operator,
                gate: crate::structured::queue::admission::Gate::Hold,
            },
        )
        .unwrap();
    wait(&wake, || {
        workers.poll(&mut registry, Instant::now(), 1000);
        while workers
            .queue_update(&registry, &case.id, generation)
            .is_some()
        {}
        loop {
            match reader.page() {
                Page::Record(record) if record["seq"] == 3 => return true,
                Page::Pending => return false,
                Page::Refused(code) | Page::Status(_, Some(code)) => {
                    panic!("live continuation refused: {code:?}")
                }
                Page::End | Page::Lagged(_) => panic!("live continuation closed before append"),
                _ => (),
            }
        }
    })
    .await;
    workers.stop(&case.id, generation);
    wait(&notify, || {
        workers.poll(&mut registry, Instant::now(), 1000);
        workers.report(&case.id, generation).is_some()
    })
    .await;
    assert!(synced);
    assert_eq!(result, Err(ErrorCode::AgentNotFound));
}

#[tokio::test]
async fn cold_queue_one_io_slot_bounds_recovery_admission() {
    let (mut registry, mut workers, _, _) = owner();
    let (case, _, _) = Case::prepare(&mut registry, 949, "idle", vec![]);
    seed(&case);
    let slot = Arc::new(());
    workers.recovery_slot = Some((case.id.clone(), Arc::downgrade(&slot)));
    let refused = workers
        .recover_queue(
            &registry,
            case.fixture.root.path().into(),
            &case.id,
            case.generation,
            policy(),
        )
        .err()
        .map(|e| e.code);
    assert_eq!(refused, Some(ErrorCode::SessionBusy));
    drop(slot);
}
