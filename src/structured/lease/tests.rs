use super::*;

#[test]
fn duplicate_resume_or_in_place_fork_is_busy_but_new_session_is_allowed() {
    let leases = SessionLeases::default();
    let _held = leases.reserve("agent", "session", None).unwrap();
    for _ in 0..3 {
        let error = leases
            .reserve("other", "session", None)
            .err()
            .expect("exclusive");
        assert_eq!(error, crate::api::error_code::ErrorCode::SessionBusy);
        assert_eq!(error.as_str(), "session_busy");
    }
    assert!(leases
        .reserve("child", "fork-session", Some(("agent", 0)))
        .is_ok());
    assert_eq!(
        crate::api::error_code::ErrorCode::SessionBusy.retry(),
        crate::cli::exit::Retry::Later
    );
}

#[test]
fn unspawned_failures_and_confirmed_cleanup_release_the_session() {
    let leases = SessionLeases::default();
    drop(leases.reserve("agent", "session", None).unwrap());
    let mut lease = leases.reserve("agent", "session", None).unwrap();
    lease.spawned();
    lease.release();
    drop(lease);
    assert!(leases.reserve("agent", "session", None).is_ok());
}

#[test]
fn unconfirmed_cleanup_retains_the_session_and_never_grants_operator() {
    let leases = SessionLeases::default();
    let mut lease = leases.reserve("agent", "session", None).unwrap();
    lease.spawned();
    drop(lease);
    assert!(leases.reserve("agent", "session", None).is_err());
    assert_eq!(
        leases.resolve(Some(11), |_| Some((0, 10))),
        Caller::Uncertain
    );
}

fn bound() -> SessionLeases {
    let leases = SessionLeases::default();
    let mut lease = leases
        .reserve("agent", "session", Some(("parent", 1)))
        .unwrap();
    lease.spawned();
    leases.0.lock().unwrap().get_mut("session").unwrap().process =
        Some(ProcessIdentity { pid: 10, birth: 20 });
    std::mem::drop(lease);
    leases
}

#[test]
fn descendants_are_attributed_from_exact_process_birth() {
    let leases = bound();
    assert_eq!(
        leases.resolve(Some(11), |pid| match pid {
            11 => Some((10, 30)),
            10 => Some((1, 20)),
            _ => None,
        }),
        Caller::Agent {
            agent: "agent".into(),
            depth: 2
        }
    );
}
#[test]
fn reused_pid_cannot_gain_agent_or_operator_authority() {
    assert_eq!(
        bound().resolve(Some(10), |_| Some((1, 21))),
        Caller::Uncertain
    );
}
#[test]
fn unknown_birth_or_missing_peer_cannot_gain_operator_authority() {
    let leases = bound();
    assert_eq!(leases.resolve(Some(10), |_| None), Caller::Uncertain);
    assert_eq!(leases.resolve(None, |_| None), Caller::Uncertain);
}
#[test]
fn cyclic_ancestry_is_unknown_and_unrelated_processes_remain_unrelated() {
    let leases = bound();
    assert_eq!(
        leases.resolve(Some(11), |_| Some((11, 30))),
        Caller::Uncertain
    );
    assert_eq!(
        leases.resolve(Some(1), |_| Some((0, 30))),
        Caller::Unrelated
    );
}

#[test]
fn a_pending_spawn_and_registry_contention_cannot_grant_operator_authority() {
    let leases = SessionLeases::default();
    let _lease = leases.reserve("agent", "session", None).unwrap();
    assert_eq!(
        leases.resolve(Some(1), |_| Some((0, 30))),
        Caller::Uncertain
    );
    let _lock = leases.0.lock().unwrap();
    assert_eq!(leases.caller(Some(1)), Caller::Uncertain);
}

#[test]
fn an_already_released_guard_cannot_remove_a_new_reservation() {
    let leases = SessionLeases::default();
    let mut old = leases.reserve("agent", "session", None).unwrap();
    old.release();
    let _new = leases.reserve("agent", "session", None).unwrap();
    old.release();
    assert!(leases.reserve("third", "session", None).is_err());
}

#[test]
fn a_pending_child_cannot_be_mistaken_for_its_leased_parent() {
    let leases = bound();
    let _child = leases.reserve("child", "fork", Some(("agent", 2))).unwrap();
    assert_eq!(
        leases.resolve(Some(10), |_| Some((1, 20))),
        Caller::Uncertain
    );
}
