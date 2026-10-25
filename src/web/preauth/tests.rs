use super::*;
use crate::web::wire::tests::TestClock;
#[test]
fn one_uid_and_unknown_cannot_exhaust_another_uid_or_operator() {
    let budget = Budget::new(Arc::new(TestClock::new()));
    let operator = crate::platform::peer::effective_uid();
    let peer = operator.wrapping_add(1);
    let held: Vec<_> = (0..PER_UID)
        .map(|_| budget.acquire(Some(peer)).unwrap())
        .collect();
    assert!(matches!(
        budget.acquire(Some(peer)),
        Err(ErrorCode::PreauthCapacity)
    ));
    let unknown: Vec<_> = (0..PER_UID)
        .map(|_| budget.acquire(None).unwrap())
        .collect();
    assert!(matches!(
        budget.acquire(None),
        Err(ErrorCode::PreauthCapacity)
    ));
    assert!(budget.acquire(Some(peer.wrapping_add(1))).is_ok());
    assert!(budget.acquire(Some(operator)).is_ok());
    drop((held, unknown));
    assert!(budget.acquire(Some(peer)).is_ok());
}
#[test]
fn ten_failed_hellos_only_throttle_that_uid_until_the_fixed_boundary() {
    let clock = TestClock::new();
    let budget = Budget::new(Arc::new(clock.clone()));
    for _ in 0..10 {
        budget.acquire(Some(10)).unwrap().failed();
    }
    assert!(matches!(
        budget.acquire(Some(10)),
        Err(ErrorCode::AuthRateLimited)
    ));
    assert!(budget.acquire(Some(11)).is_ok());
    assert!(budget.acquire(None).is_ok());
    clock.advance(999);
    assert!(matches!(
        budget.acquire(Some(10)),
        Err(ErrorCode::AuthRateLimited)
    ));
    clock.advance(1);
    assert!(budget.acquire(Some(10)).is_ok());
    assert_eq!(ErrorCode::PreauthCapacity.http_status(), 503);
    assert_eq!(ErrorCode::AuthRateLimited.http_status(), 429);
    assert_eq!(ErrorCode::PreauthCapacity.retry().as_str(), "later");
    assert_eq!(ErrorCode::AuthRateLimited.retry().as_str(), "later");
}

#[test]
fn a_refusing_peer_cannot_allocate_unbounded_http_error_tasks() {
    let budget = Budget::new(Arc::new(TestClock::new()));
    let held: Vec<_> = (0..PER_UID)
        .map(|_| budget.acquire(Some(10)).unwrap())
        .collect();
    assert!(matches!(
        budget.acquire(Some(10)),
        Err(ErrorCode::PreauthCapacity)
    ));
    let refusal = budget.reject(Some(10)).unwrap();
    assert!(budget.reject(Some(10)).is_none());
    assert!(budget.acquire(Some(11)).is_ok());
    drop(refusal);
    assert!(budget.reject(Some(10)).is_some());
    drop(held);
}
