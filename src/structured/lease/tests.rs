use super::*;

impl SessionLeases {
    /// Resolve with a supplied process snapshot reader. Unknown facts fail closed.
    /// The reader must verify stable birth around its parent lookup.
    fn resolve(&self, peer: Option<u32>, read: impl FnMut(u32) -> Option<(u32, u64)>) -> Caller {
        self.resolve_with_pane(peer, None, read)
    }
}

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
        leases.resolve(Some(std::process::id()), |_| Some((1, 10))),
        Caller::Uncertain
    );
}

fn bound() -> SessionLeases {
    let leases = SessionLeases::default();
    let mut lease = leases
        .reserve("agent", "session", Some(("parent", 1)))
        .unwrap();
    lease.spawned();
    leases.0.lock().get_mut("session").unwrap().process =
        Some(ProcessIdentity { pid: 10, birth: 20 });
    leases.publish(&leases.0.lock());
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
fn an_already_released_guard_cannot_remove_a_new_reservation() {
    let leases = SessionLeases::default();
    let mut old = leases.reserve("agent", "session", None).unwrap();
    old.release();
    let _new = leases.reserve("agent", "session", None).unwrap();
    old.release();
    assert!(leases.reserve("third", "session", None).is_err());
}

#[test]
fn an_unspawned_reservation_does_not_erase_a_bound_parent_identity() {
    let leases = bound();
    let _child = leases.reserve("child", "fork", Some(("agent", 2))).unwrap();
    assert_eq!(
        leases.resolve(Some(10), |_| Some((1, 20))),
        Caller::Agent {
            agent: "agent".into(),
            depth: 2
        }
    );
}

#[test]
fn real_operator_ancestry_remains_unrelated_with_a_live_lease() {
    let leases = SessionLeases::default();
    let mut child = crate::process::TracedCommand::new("/bin/sh", "test")
        .args(["-c", "read line"])
        .stdin(std::process::Stdio::piped())
        .spawn_traced()
        .unwrap();
    let mut lease = leases.reserve("agent", "session", None).unwrap();
    lease.spawned();
    lease.bind(child.id()).unwrap();
    // Actual kernel ancestry, including root-owned ancestors on macOS. The
    // fixture child cannot be an ancestor of its own parent test process.
    let caller = leases.caller(Some(std::process::id()));
    let _ = child.kill();
    child.wait().unwrap();
    lease.release();
    assert_eq!(caller, Caller::Unrelated);
}

#[test]
fn pending_launches_only_restrict_their_own_ancestry() {
    let leases = bound();
    let mut pending = leases.reserve("pending", "pending", None).unwrap();
    // ADR-0036 §1a: preparation has no caller; only a spawned root can match.
    assert_eq!(
        leases.resolve(Some(12), |_| Some((1, 40))),
        Caller::Unrelated
    );
    pending.spawned();
    leases.0.lock().get_mut("pending").unwrap().root = Root::Process(12);
    assert_eq!(
        leases.resolve(Some(12), |_| Some((1, 40))),
        Caller::Uncertain
    );
    assert_eq!(
        leases.resolve(Some(13), |_| Some((1, 50))),
        Caller::Unrelated
    );
    assert_eq!(
        leases.resolve(Some(10), |_| Some((1, 20))),
        Caller::Agent {
            agent: "agent".into(),
            depth: 2
        }
    );
    assert_eq!(
        leases.resolve_with_pane(Some(14), Some(14), |_| Some((1, 60))),
        Caller::Unrelated
    );
    assert!(leases.reserve("other", "pending", None).is_err());
}

#[test]
fn failed_bind_and_unconfirmed_cleanup_only_restrict_the_recorded_root() {
    let leases = SessionLeases::default();
    let mut lease = leases.reserve("agent", "session", None).unwrap();
    lease.spawned();
    // PID zero cannot name a live process on either platform.
    assert!(lease.bind(0).is_err());
    assert_eq!(leases.0.lock()["session"].root, Root::Process(0));
    drop(lease);
    assert!(leases.reserve("other", "session", None).is_err());
    // ADR-0036 §1a: a retained session reservation is not a global identity veto.
    assert_eq!(leases.caller(Some(std::process::id())), Caller::Unrelated);
    leases.0.lock().get_mut("session").unwrap().root = Root::Process(12);
    assert_eq!(
        leases.resolve(Some(13), |pid| Some((if pid == 13 { 12 } else { 1 }, 40))),
        Caller::Uncertain
    );
}

#[test]
fn short_registry_contention_waits_for_the_existing_identity() {
    let leases = bound();
    let guard = leases.0.lock();
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = leases.clone();
    let worker = std::thread::spawn(move || {
        tx.send(reader.resolve(Some(10), |_| Some((1, 20))))
            .unwrap();
    });
    // Event-driven deadline proves contention doesn't immediately deny the caller.
    assert!(matches!(
        rx.recv_timeout(Duration::from_millis(25)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    drop(guard);
    let resolved = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    worker.join().unwrap();
    assert_eq!(
        resolved,
        Caller::Agent {
            agent: "agent".into(),
            depth: 2
        }
    );
}

#[test]
fn bounded_contention_keeps_a_proven_pane_identity() {
    let leases = bound();
    let _guard = leases.0.lock();
    // ADR-0036 §1a preserves the independently established PTY boundary.
    assert_eq!(
        leases.resolve_with_pane(Some(11), Some(11), |_| None),
        Caller::Unrelated
    );
    assert_eq!(
        leases.resolve(Some(11), |_| Some((1, 30))),
        Caller::Unrelated
    );
    assert_eq!(
        leases.resolve(Some(10), |_| Some((1, 20))),
        Caller::Agent {
            agent: "agent".into(),
            depth: 2
        }
    );
}

#[test]
fn init_is_a_terminal_ancestor_without_a_metadata_read() {
    assert_eq!(
        bound().resolve(Some(1), |_| panic!("must not query init")),
        Caller::Unrelated
    );
}

#[test]
fn long_spawn_contention_retains_only_launcher_ancestry_uncertainty() {
    let leases = SessionLeases::default();
    let mut pending = leases.reserve("agent", "session", None).unwrap();
    pending.spawned();
    let _guard = leases.0.lock();
    // The published pre-spawn anchor prevents a child becoming operator during
    // the spawn lock, while ADR-0036 §1a keeps unrelated callers available.
    assert_eq!(
        leases.resolve(Some(std::process::id()), |_| Some((1, 10))),
        Caller::Uncertain
    );
    assert_eq!(
        leases.resolve(Some(12), |_| Some((1, 20))),
        Caller::Unrelated
    );
}

#[test]
fn preparation_restricts_launcher_descendants_but_not_bound_agents_or_panes() {
    let leases = bound();
    let _pending = leases.reserve("preparing", "preparing", None).unwrap();
    assert_eq!(
        leases.resolve(Some(std::process::id()), |_| Some((1, 40))),
        Caller::Uncertain
    );
    assert_eq!(
        leases.resolve(Some(12), |_| Some((1, 40))),
        Caller::Unrelated
    );
    assert_eq!(
        leases.resolve_with_pane(
            Some(std::process::id()),
            Some(std::process::id()),
            |_| Some((1, 40))
        ),
        Caller::Unrelated
    );
    assert_eq!(
        leases.resolve_with_pane(Some(11), Some(11), |pid| Some((
            if pid == 11 { 10 } else { 1 },
            if pid == 10 { 20 } else { 40 }
        ))),
        Caller::Agent {
            agent: "agent".into(),
            depth: 2
        }
    );
}
