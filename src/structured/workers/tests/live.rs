use super::*;
use crate::structured::{
    history::Item as HistoryItem,
    workers::live::{End, Item, CAPACITY},
};

async fn drive(notify: &Arc<Notify>, mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(LIMIT, async {
        loop {
            let wake = notify.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            if condition() {
                break;
            }
            wake.await;
        }
    })
    .await
    .expect("live fixture failed its bounded stage");
}
fn start(
    mode: &str,
) -> (
    Registry,
    Workers,
    Arc<Notify>,
    Case,
    crate::structured::workers::live::Subscription,
) {
    let (mut registry, mut workers, notify, _) = owner();
    let (mut case, mut launch, factory) = Case::prepare(&mut registry, 919, "unused", vec![]);
    let mut record = registry.remove(&case.id, case.generation).unwrap();
    record.model = Some(mode.into());
    launch.generation = registry.register(record).unwrap();
    let executable = case.fixture.root.path().join("claude");
    let interpreter = fs::read_to_string(&executable)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_owned();
    case.fixture.file(
        "claude",
        format!(
            "{interpreter}\n{}",
            include_str!("../../../../tests/fixtures/structured-public/claude.py")
        )
        .as_bytes(),
        0o700,
    );
    case.generation = workers.start(&mut registry, launch, factory).unwrap();
    let sub = workers.live(&registry, &case.id).unwrap();
    (registry, workers, notify, case, sub)
}
async fn ready(registry: &mut Registry, workers: &mut Workers, notify: &Arc<Notify>, case: &Case) {
    let clock = Instant::now();
    drive(notify, || {
        workers.poll(registry, clock, 1000);
        assert!(
            workers.report(&case.id, case.generation).is_none(),
            "producer died before complete assistant"
        );
        workers.entries[&case.id].queue_seq >= 3
    })
    .await;
}
async fn next(sub: &mut crate::structured::workers::live::Subscription) -> Item {
    tokio::time::timeout(LIMIT, sub.recv())
        .await
        .expect("live feed never resolved")
}
#[tokio::test]
async fn partials_order_and_durable_cursor() {
    let (mut registry, mut workers, notify, case, mut sub) = start("s10b-live");
    ready(&mut registry, &mut workers, &notify, &case).await;
    for expected in ["partial", "thinking", "tool-result"] {
        let Item::Delta { record, event } = next(&mut sub).await else {
            panic!("missing delta")
        };
        assert_eq!(
            record.seq, 1,
            "partials retain the preceding durable cursor"
        );
        assert_eq!(record.known, Some(true));
        assert!(record.raw.contains(expected));
        assert!(matches!(
            &*event,
            crate::structured::claude::SessionEvent::ContentBlockDelta(_)
        ));
    }
    let mut cut = workers
        .history(&registry, &case.id)
        .unwrap()
        .subscribe(Some(3))
        .unwrap();
    assert!(matches!(cut.page().unwrap(), HistoryItem::Synced(3)));
    assert!(matches!(cut.page().unwrap(), HistoryItem::Pending));
    workers.shutdown(&mut registry).unwrap();
    assert!(matches!(
        next(&mut sub).await,
        Item::End(End::ProducerEnded)
    ));
}
#[tokio::test]
async fn lag_is_explicit_at_latest_durable_cursor() {
    let (mut registry, mut workers, notify, case, mut sub) = start("s10b-live-burst");
    ready(&mut registry, &mut workers, &notify, &case).await;
    assert!(matches!(next(&mut sub).await, Item::Gap { through: 3 }));
    for _ in 0..CAPACITY {
        assert!(matches!(next(&mut sub).await, Item::Delta { .. }));
    }
    workers.shutdown(&mut registry).unwrap();
}
#[tokio::test]
async fn producer_death_finishes_the_feed() {
    let (mut registry, mut workers, notify, case, mut sub) = start("s10b-live-exit");
    drive(&notify, || {
        workers.poll(&mut registry, Instant::now(), 1000);
        workers.report(&case.id, case.generation).is_some()
    })
    .await;
    for _ in 0..3 {
        assert!(matches!(next(&mut sub).await, Item::Delta { .. }));
    }
    assert!(matches!(
        next(&mut sub).await,
        Item::End(End::ProducerEnded)
    ));
    workers.shutdown(&mut registry).unwrap();
}
#[tokio::test]
async fn generation_retires_old_feed() {
    let (mut registry, mut workers, notify, case, mut sub) = start("s10b-live");
    ready(&mut registry, &mut workers, &notify, &case).await;
    registry.begin_launch(&case.id, case.generation).unwrap();
    workers.poll(&mut registry, Instant::now(), 1000);
    assert!(workers.live(&registry, &case.id).is_none());
    for _ in 0..3 {
        assert!(matches!(next(&mut sub).await, Item::Delta { .. }));
    }
    assert!(matches!(next(&mut sub).await, Item::End(End::Retired)));
    workers.shutdown(&mut registry).unwrap();
}
