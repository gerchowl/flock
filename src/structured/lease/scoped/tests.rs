use super::*;
use crate::platform::peer::Credentials;

fn peer() -> Credentials {
    Credentials {
        pid: Some(200),
        birth: Some(20),
        uid: Some(7),
    }
}
fn facts(pid: u32) -> Option<(u32, u64)> {
    match pid {
        200 => Some((100, 20)),
        100 => Some((1, 10)),
        _ => None,
    }
}
fn fixture() -> (SessionLeases, Lease) {
    let leases = SessionLeases::default();
    let lease = leases.reserve("agent", "session", None).unwrap();
    bind(&leases);
    (leases, lease)
}
fn bind(leases: &SessionLeases) {
    leases
        .update(|entries| {
            let entry = entries.get_mut("session").unwrap();
            entry.root = Root::Process(100);
            entry.process = Some(ProcessIdentity {
                pid: 100,
                birth: 10,
            });
        })
        .unwrap();
}
fn agent() -> Caller {
    Caller::Agent {
        agent: "agent".into(),
        depth: 0,
    }
}

#[test]
fn scoped_live_lease_is_agent_and_wrong_or_missing_credentials_refuse() {
    let (leases, lease) = fixture();
    assert_eq!(
        leases.scoped_for_test(Some(&lease.credential()), peer(), facts),
        agent()
    );
    assert_eq!(
        leases.scoped_for_test(Some(&LeaseCredential::generate().unwrap()), peer(), facts),
        Caller::Uncertain
    );
    assert_eq!(
        leases.scoped_for_test(None, peer(), facts),
        Caller::Uncertain
    );
}

#[test]
fn scoped_release_rotation_and_restart_never_become_unrelated() {
    let (leases, mut lease) = fixture();
    let old = lease.credential();
    lease.release();
    assert_eq!(
        leases.scoped_for_test(Some(&old), peer(), facts),
        Caller::Uncertain
    );
    let next = leases.reserve("agent", "session", None).unwrap();
    bind(&leases);
    assert_eq!(
        leases.scoped_for_test(Some(&old), peer(), facts),
        Caller::Uncertain
    );
    assert_eq!(
        leases.scoped_for_test(Some(&next.credential()), peer(), facts),
        agent()
    );
    assert_eq!(
        SessionLeases::default().scoped_for_test(Some(&old), peer(), facts),
        Caller::Uncertain
    );
}

#[test]
fn scoped_kernel_identity_loss_reuse_and_orphan_refuse() {
    let (leases, lease) = fixture();
    let credential = lease.credential();
    for captured in [
        Credentials {
            pid: None,
            ..peer()
        },
        Credentials {
            birth: None,
            ..peer()
        },
        Credentials {
            uid: None,
            ..peer()
        },
        Credentials {
            birth: Some(99),
            ..peer()
        },
    ] {
        assert_eq!(
            leases.scoped_for_test(Some(&credential), captured, facts),
            Caller::Uncertain
        );
    }
    for read in [
        |_: u32| None,
        |pid| {
            if pid == 200 {
                Some((1, 20))
            } else {
                facts(pid)
            }
        },
        |pid| {
            if pid == 100 {
                Some((1, 99))
            } else {
                facts(pid)
            }
        },
    ] {
        assert_eq!(
            leases.scoped_for_test(Some(&credential), peer(), read),
            Caller::Uncertain
        );
    }
}

#[test]
fn scoped_contention_refuses_and_revocation_precedes_registry_removal() {
    let (leases, lease) = fixture();
    let credential = lease.credential();
    let held = leases.0.lock();
    assert_eq!(
        leases.scoped_for_test(Some(&credential), peer(), facts),
        Caller::Busy
    );
    lease.revoke();
    drop(held);
    assert!(leases.is_reserved("session").unwrap());
    assert_eq!(
        leases.scoped_for_test(Some(&credential), peer(), facts),
        Caller::Uncertain
    );
}

#[test]
fn scoped_secret_debug_is_redacted() {
    let credential = LeaseCredential::generate().unwrap();
    let serialized = serde_json::to_string(&credential).unwrap();
    let debug = format!("{credential:?}");
    assert!(!debug.contains(serialized.trim_matches('"')));
    assert_eq!(debug, "LeaseCredential([redacted])");
}

#[test]
fn scoped_owner_drop_revokes_even_when_cleanup_retains_the_reservation() {
    let (leases, mut lease) = fixture();
    let credential = lease.credential();
    lease.spawned = true;
    drop(lease);
    assert!(leases.is_reserved("session").unwrap());
    assert_eq!(
        leases.scoped_for_test(Some(&credential), peer(), facts),
        Caller::Uncertain
    );
}

#[test]
fn scoped_busy_registry_is_typed_nonblocking_and_does_no_fact_work() {
    let (leases, lease) = fixture();
    let _busy = leases.0.lock();
    let result = leases.scoped_with(Some(&lease.credential()), peer(), |_| {
        panic!("no fact read under contention")
    });
    assert_eq!(result, Err(ScopedRefusal::RegistryContended));
}

#[test]
fn scoped_process_fact_work_has_a_closed_budget() {
    let (leases, lease) = fixture();
    let mut reads = 0;
    let result = leases.scoped_with(Some(&lease.credential()), peer(), |pid| {
        reads += 1;
        Some((pid + 1, 20))
    });
    assert_eq!(reads, 16);
    assert_eq!(result, Err(ScopedRefusal::AncestryBudget));
}

#[test]
fn hardened_comparison_checks_every_encoded_position() {
    let credential = LeaseCredential::generate().unwrap();
    assert!(credential.matches(&credential.clone()));
    for i in 0..credential.0.len() {
        let mut wrong = credential.0.to_string().into_bytes();
        wrong[i] ^= 1;
        assert!(
            !credential.matches(&super::super::credential::LeaseCredential(
                zeroize::Zeroizing::new(String::from_utf8(wrong).unwrap())
            ))
        );
    }
}

#[test]
fn scoped_contention_is_retryable_but_ancestry_budget_is_permanent() {
    let (leases, lease) = fixture();
    let busy = {
        let _guard = leases.0.lock();
        leases.scoped_for_test(Some(&lease.credential()), peer(), facts)
    };
    let exhausted =
        leases.scoped_for_test(Some(&lease.credential()), peer(), |pid| Some((pid + 1, 20)));
    assert_eq!(busy, Caller::Busy);
    assert_eq!(
        busy.refusal().unwrap().retry(),
        crate::cli::exit::Retry::Later
    );
    assert_eq!(exhausted, Caller::AncestryLimit);
    let code = exhausted.refusal().unwrap();
    assert_eq!(code.as_str(), "lease_ancestry_limit");
    assert_eq!(code.retry(), crate::cli::exit::Retry::Never);
}

#[test]
fn continuation_cell_refuses_revoke_and_expiry_without_the_table() {
    let (leases, lease) = fixture();
    let (_, proof) = leases
        .admit_with(Some(&lease.credential()), peer(), facts)
        .unwrap();
    assert_eq!(proof.verify_with(facts).unwrap(), agent());
    crate::api::scoped::tests::with_revocation_under_contention(&leases, lease, || {
        assert_eq!(proof.verify_with(facts).unwrap(), Caller::Uncertain);
    });
    assert!(leases.is_empty());
    let (leases, lease) = fixture();
    let (_, mut proof) = leases
        .admit_with(Some(&lease.credential()), peer(), facts)
        .unwrap();
    // Give only this fixture cell an already elapsed monotonic deadline.
    proof.entry.generation.cell = Arc::new(GenerationCell {
        revoked: AtomicBool::new(false),
        deadline: Some(Instant::now()),
    });
    assert_eq!(proof.verify_with(facts).unwrap(), Caller::Uncertain);
}

#[test]
fn root_rebinding_revokes_the_old_continuation_cell_before_publication() {
    let leases = SessionLeases::default();
    let mut lease = leases.reserve("agent", "session", None).unwrap();
    lease.bind_test_peer(std::process::id());
    let peer =
        crate::platform::peer::credentials(&std::os::unix::net::UnixStream::pair().unwrap().0);
    let (_, proof) = leases.admit(Some(&lease.credential()), peer).unwrap();
    assert!(matches!(proof.caller(), Caller::Agent { .. }));
    lease.bind_test_peer(std::process::id());
    assert_eq!(proof.caller(), Caller::Uncertain);
    assert!(matches!(
        leases.scoped_caller(Some(&lease.credential()), peer),
        Caller::Agent { .. }
    ));
}

#[test]
fn revocation_during_fresh_ancestry_read_refuses_the_boundary() {
    let (leases, lease) = fixture();
    let (_, proof) = leases
        .admit_with(Some(&lease.credential()), peer(), facts)
        .unwrap();
    leases.with_registry_locked(|| {
        assert_eq!(
            proof
                .verify_with(|pid| {
                    if pid == 100 {
                        lease.revoke();
                    }
                    facts(pid)
                })
                .unwrap(),
            Caller::Uncertain
        );
    });
}

impl Lease {
    /// Publish revocation without making a test wait for bounded cleanup.
    pub(crate) fn revoke_for_test(&self) {
        self.revoke();
    }
}
