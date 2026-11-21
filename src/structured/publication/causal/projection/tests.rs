use super::*;
use std::time::Duration;
#[tokio::test]
async fn capture_either_excludes_a_later_cause_or_returns_its_receipt() {
    let (source, mut reader) = Projection::channel();
    source.observe(1);
    reader.changed().await;
    let old = reader.take().unwrap();
    let (cause, ack) = Cause::hold();
    source.reserve(cause).unwrap().send(2);
    source.observe(3);
    assert_eq!(old.0, 1);
    assert!(
        old.1.is_none(),
        "previous immutable cut excludes the later effect"
    );
    reader.changed().await;
    let (value, held) = reader.take().unwrap();
    assert_eq!(
        value, 2,
        "ordinary latest state cannot bypass the caused entry"
    );
    let held = held.unwrap();
    drop(ack);
    held.released().await;
    tokio::time::timeout(Duration::from_secs(5), reader.changed())
        .await
        .unwrap();
    let next = reader.take().unwrap();
    assert_eq!(next.0, 3);
    assert!(next.1.is_none());
}
#[tokio::test]
async fn separate_causes_survive_coalescing_and_reservations_remain_bounded() {
    let (source, mut reader) = Projection::channel();
    let (first, first_ack) = Cause::hold();
    let (second, second_ack) = Cause::hold();
    let mut reservations = Vec::new();
    for _ in 0..32 {
        reservations.push(source.reserve(first.clone()).unwrap());
    }
    assert!(matches!(source.reserve(first.clone()), Err(e) if e.kind()==io::ErrorKind::WouldBlock));
    drop(reservations);
    source.reserve(first).unwrap().send(1);
    source.observe(2);
    source.reserve(second).unwrap().send(3);
    source.observe(4);
    source.observe(5);
    let (value, cause) = reader.take().unwrap();
    assert_eq!(value, 1);
    drop(first_ack);
    cause.unwrap().released().await;
    let (value, cause) = reader.take().unwrap();
    assert_eq!(value, 3);
    drop(second_ack);
    cause.unwrap().released().await;
    assert_eq!(reader.take().unwrap().0, 5);
    assert!(reader.take().is_none());
}
