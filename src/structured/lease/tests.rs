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
    lease.spawned().unwrap();
    lease.release();
    drop(lease);
    assert!(leases.reserve("agent", "session", None).is_ok());
}

#[test]
fn unconfirmed_cleanup_retains_the_session_and_never_grants_operator() {
    let leases = SessionLeases::default();
    let mut lease = leases.reserve("agent", "session", None).unwrap();
    lease.spawned().unwrap();
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
    lease.spawned().unwrap();
    leases.0.lock().get_mut("session").unwrap().process =
        Some(ProcessIdentity { pid: 10, birth: 20 });
    leases.update(|_| ()).unwrap();
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
    lease.spawned().unwrap();
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
    pending.spawned().unwrap();
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
    lease.spawned().unwrap();
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
fn registry_contention_preserves_identity_after_explicit_release() {
    #[derive(Debug, PartialEq)]
    enum ReadStep {
        Contended(bool),
        Resolved(Caller),
    }

    let leases = bound();
    let guard = leases.0.lock();
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = leases.clone();
    let worker = std::thread::spawn(move || {
        let contended = reader.0.try_lock().is_none();
        tx.send(ReadStep::Contended(contended)).unwrap();
        tx.send(ReadStep::Resolved(
            reader.resolve(Some(10), |_| Some((1, 20))),
        ))
        .unwrap();
    });
    // ADR-0037 §1: observed contention commands the holder's release. Both
    // coherent registry views may resolve before the observer is scheduled.
    let entered = rx.recv_timeout(Duration::from_secs(1));
    drop(guard);
    assert_eq!(entered, Ok(ReadStep::Contended(true)));
    let resolved = rx.recv_timeout(Duration::from_secs(1));
    assert_eq!(
        resolved,
        Ok(ReadStep::Resolved(Caller::Agent {
            agent: "agent".into(),
            depth: 2,
        }))
    );
    // Validate every stage before joining: a live silent reader must fail,
    // rather than hold nextest open on an unbounded thread join.
    worker.join().unwrap();
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
fn long_registry_contention_retains_only_launcher_ancestry_uncertainty() {
    let leases = SessionLeases::default();
    let mut pending = leases.reserve("agent", "session", None).unwrap();
    pending.spawned().unwrap();
    let _guard = leases.0.lock();
    // The published pre-spawn anchor prevents a child becoming operator during
    // registry contention, while ADR-0036 §1a keeps unrelated callers available.
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

fn assert_mutation_wait_is_bounded(action: u8) {
    let leases = SessionLeases::default();
    let mut lease = (action != 0).then(|| leases.reserve("agent", "session", None).unwrap());
    if action == 2 || action == 3 {
        lease.as_mut().unwrap().spawned().unwrap();
    }
    let guard = leases.0.lock();
    let table = leases.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = match action {
            0 => table.reserve("agent", "session", None).is_err(),
            1 => lease.as_mut().unwrap().spawned().is_err(),
            2 => lease.as_mut().unwrap().bind(std::process::id()).is_err(),
            3 => {
                let lease = lease.as_mut().unwrap();
                lease.release();
                !lease.released
            }
            _ => {
                drop(lease.take());
                true
            }
        };
        let _ = tx.send(result);
    });
    // Release the guard before asserting, so a blocked mutation can finish.
    // Assert before joining, so a live silent worker cannot hang the test.
    let result = rx.recv_timeout(Duration::from_secs(1));
    drop(guard);
    if action == 4 {
        assert!(
            leases.0.lock().contains_key("session"),
            "busy mutation must retain its reservation"
        );
    }
    assert_eq!(
        result,
        Ok(true),
        "mutation {action} exceeded its fail ceiling"
    );
    worker.join().unwrap();
}

#[test]
fn reserve_has_a_bounded_registry_wait() {
    assert_mutation_wait_is_bounded(0);
}

#[test]
fn spawn_marker_has_a_bounded_registry_wait() {
    assert_mutation_wait_is_bounded(1);
}

#[test]
fn bind_has_a_bounded_registry_wait() {
    assert_mutation_wait_is_bounded(2);
}

#[test]
fn release_has_a_bounded_registry_wait() {
    assert_mutation_wait_is_bounded(3);
}

#[test]
fn drop_has_a_bounded_registry_wait() {
    assert_mutation_wait_is_bounded(4);
}

#[test]
fn snapshot_contention_leaves_reservations_unchanged() {
    let leases = SessionLeases::default();
    let guard = leases.1.lock();
    let table = leases.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = table.reserve("agent", "session", None);
        let unchanged = table.is_empty();
        let _ = tx.send((result.is_err(), unchanged));
    });
    let result = rx.recv_timeout(Duration::from_secs(1));
    drop(guard);
    assert_eq!(result, Ok((true, true)));
    worker.join().unwrap();
}

#[test]
fn fork_exec_callback_holds_neither_registry_lock() {
    let leases = SessionLeases::default();
    let mut lease = leases.reserve("agent", "session", None).unwrap();
    let result = lease.spawn(|| {
        assert!(leases.0.try_lock().is_some());
        assert!(leases.1.try_lock().is_some());
        Err(io::Error::other("owned spawn failure"))
    });
    assert!(result.is_err());
    lease.release();
}

#[derive(Clone, Default)]
struct LogCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for LogCapture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn retained_lease_log(confirmed: bool) -> String {
    let capture = LogCapture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let leases = SessionLeases::default();
        let mut lease = leases.reserve("agent", "session", None).unwrap();
        lease.spawned().unwrap();
        let _guard = leases.0.lock();
        if confirmed {
            lease.release();
        }
        drop(lease);
    });
    let bytes = capture.0.lock().unwrap().clone();
    String::from_utf8(bytes).unwrap()
}

#[test]
fn busy_release_logs_reservation_and_confirmed_cleanup_cause() {
    let output = retained_lease_log(true);
    assert!(output.contains("session=\"session\""), "{output}");
    assert!(output.contains("agent=\"agent\""), "{output}");
    assert_eq!(output.matches("registry busy during release").count(), 1);
    assert!(!output.contains("process cleanup unconfirmed"));
}

#[test]
fn unconfirmed_cleanup_logs_reservation_identity() {
    let output = retained_lease_log(false);
    assert!(output.contains("session=\"session\""), "{output}");
    assert!(output.contains("agent=\"agent\""), "{output}");
    assert_eq!(output.matches("process cleanup unconfirmed").count(), 1);
    assert!(!output.contains("registry busy during release"));
}

#[test]
fn public_lease_state_reports_binding_without_authority_or_waiting() {
    let leases = SessionLeases::default();
    assert_eq!(leases.state("agent", "session"), LeaseState::Absent);
    let mut lease = leases.reserve("agent", "session", None).unwrap();
    assert_eq!(leases.state("agent", "session"), LeaseState::Reserved);
    assert_eq!(leases.state("other", "session"), LeaseState::Absent);
    {
        let _guard = leases.0.lock();
        assert_eq!(leases.state("agent", "session"), LeaseState::Uncertain);
    }
    lease.spawned().unwrap();
    lease.bind_test_peer(std::process::id());
    assert_eq!(leases.state("agent", "session"), LeaseState::Bound);
    lease.generation.revoke();
    assert_eq!(leases.state("agent", "session"), LeaseState::Uncertain);
    lease.release();
    assert_eq!(leases.state("agent", "session"), LeaseState::Absent);
}

#[test]
fn owner_admission_reads_published_snapshot_without_waiting() {
    let leases = SessionLeases::default();
    let lease = leases
        .reserve(
            "fixture-child",
            "fixture-session",
            Some(("fixture-parent", 0)),
        )
        .unwrap();
    let shared = leases.clone();
    let (ready, seen) = std::sync::mpsc::sync_channel(1);
    let (release, wait) = std::sync::mpsc::sync_channel(1);
    let holder = std::thread::spawn(move || {
        let _held = shared.0.lock();
        ready.send(()).unwrap();
        // A blocking-lock regression must fail after this ceiling, never hang CI.
        let _ = wait.recv_timeout(std::time::Duration::from_secs(10));
    });
    seen.recv_timeout(std::time::Duration::from_secs(10))
        .unwrap();
    let snapshot = leases.admitted_identities();
    let workers =
        crate::structured::workers::Workers::new(std::sync::Arc::new(tokio::sync::Notify::new()));
    let counted = workers.counts(&leases, "fixture-parent");
    let released = release.send(());
    holder.join().unwrap();
    assert_eq!(
        snapshot,
        Some(BTreeMap::from([(
            "fixture-child".into(),
            Some("fixture-parent".into())
        )]))
    );
    assert_eq!(counted, Ok((1, 1)));
    drop(lease);
    released.unwrap();
}

#[test]
fn owner_admission_reports_retryable_contention_when_both_views_are_busy() {
    let leases = SessionLeases::default();
    let shared = leases.clone();
    let (ready, seen) = std::sync::mpsc::sync_channel(1);
    let (release, wait) = std::sync::mpsc::sync_channel(1);
    let holder = std::thread::spawn(move || {
        let _live = shared.0.lock();
        let _published = shared.1.lock();
        ready.send(()).unwrap();
        // Bound a blocking-read regression even if the caller never releases.
        let _ = wait.recv_timeout(std::time::Duration::from_secs(10));
    });
    seen.recv_timeout(std::time::Duration::from_secs(10))
        .unwrap();
    let workers =
        crate::structured::workers::Workers::new(std::sync::Arc::new(tokio::sync::Notify::new()));
    let counted = workers.counts(&leases, "fixture-parent");
    let released = release.send(());
    holder.join().unwrap();
    released.unwrap();
    assert_eq!(counted, Err(crate::api::error_code::ErrorCode::SessionBusy));
    let refusal = crate::spawn::SpawnRefusal::RegistryBusy;
    assert_eq!(refusal.code(), "session_busy");
    assert!(refusal.retryable());
}
