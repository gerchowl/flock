use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture {
    root: PathBuf,
    home: PathBuf,
    path: PathBuf,
    writer: Writer,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "binding-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let home = root.join("launch-home");
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        let path = home.join("sessions/physical-id-unrelated-to-thread.jsonl");
        let writer = Writer {
            pid: std::process::id(),
            birth: platform::process_start_time(std::process::id()).unwrap(),
        };
        let fixture = Self {
            root,
            home,
            path,
            writer,
        };
        fixture.metadata("child", crate::codex_version::TESTED_VERSION);
        fixture
    }
    fn metadata(&self, id: &str, version: &str) {
        std::fs::write(&self.path, format!("{}\n{}\n", serde_json::json!({"type":"session_meta","payload":{"id":id,"cli_version":version}}), serde_json::json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"current"}}))).unwrap();
    }
    fn validate(&self, key: &Key) -> Result<Validated, Failure> {
        validate(
            key,
            &self.home,
            Some(&self.path),
            self.writer,
            &[self.writer],
            &compatibility(),
            4096,
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}
fn compatibility() -> Compatibility {
    Compatibility {
        metadata_version: Some(crate::codex_version::TESTED_VERSION.into()),
        writer_version: Some(crate::codex_version::TESTED_VERSION.into()),
        probe_failure: None,
    }
}
fn owner(pane: &str) -> Owner {
    Owner {
        node: "node.example".into(),
        pane: pane.into(),
    }
}
fn begin(binding: &mut Binding, owners: &mut Ownership) -> Key {
    binding
        .begin(&binding.owner.clone(), "child".into(), owners)
        .unwrap()
}
fn bind(binding: &mut Binding, owners: &mut Ownership, fixture: &Fixture) -> Key {
    let key = begin(binding, owners);
    binding
        .finish(&key, fixture.validate(&key), owners)
        .unwrap();
    key
}

#[test]
fn pending_then_validation_uses_hook_path_meta_and_local_writer() {
    let f = Fixture::new();
    let mut b = Binding::new(owner("pane"));
    let mut o = Ownership::default();
    assert_eq!(b.decision(&o), Decision::AwaitingFirstTurn);
    let key = begin(&mut b, &mut o);
    assert_eq!(b.decision(&o), Decision::Unknown);
    let mut value = b.finish(&key, f.validate(&key), &mut o).unwrap();
    assert_eq!(b.decision(&o), Decision::Bound);
    assert!(b.accepts(&key, &o));
    assert_eq!(value.path, f.path.canonicalize().unwrap());
    assert_eq!(value.writer, f.writer);
    assert_eq!(value.metadata_version, crate::codex_version::TESTED_VERSION);
    let mut rest = String::new();
    value.file.read_to_string(&mut rest).unwrap();
    assert!(rest.contains("task_started"));
    assert!(!rest.contains("session_meta"));
}

#[test]
fn first_failure_is_diagnosed_fallback_and_rejected_callbacks_stay_revoked() {
    let f = Fixture::new();
    let mut b = Binding::new(owner("pane"));
    let mut o = Ownership::default();
    for reason in [
        Failure::Path,
        Failure::Metadata,
        Failure::Writer,
        Failure::Version,
        Failure::DuplicateOwner,
    ] {
        let key = begin(&mut b, &mut o);
        assert_eq!(b.finish(&key, Err(reason), &mut o).unwrap_err(), reason);
        assert_eq!(b.decision(&o), Decision::BindingValidationFailed(reason));
        assert_eq!(b.failure(&o), Some(reason));
        assert!(!b.accepts(&key, &o));
        assert!(b.finish(&key, f.validate(&key), &mut o).is_err());
    }
    // Screen classification and submission eligibility consume this explicit
    // fallback in #1664, without requiring another hook or inventing idle here.
}

#[test]
fn failed_rebind_never_erases_launch_trust_even_across_failed_claims() {
    let f = Fixture::new();
    let mut b = Binding::new(owner("pane"));
    let mut o = Ownership::default();
    let old = bind(&mut b, &mut o, &f);
    for reason in [
        Failure::Path,
        Failure::Metadata,
        Failure::Writer,
        Failure::Version,
        Failure::DuplicateOwner,
    ] {
        let key = begin(&mut b, &mut o);
        b.finish(&key, Err(reason), &mut o).unwrap_err();
        assert_eq!(b.decision(&o), Decision::Unknown);
        assert_eq!(b.failure(&o), Some(reason));
        assert!(!b.accepts(&old, &o));
        assert!(!b.accepts(&key, &o));
    }
    bind(&mut b, &mut o, &f);
    assert_eq!(b.decision(&o), Decision::Bound);
}

#[test]
fn known_launch_retirement_alone_resets_trust_and_revokes_queued_work() {
    let f = Fixture::new();
    let mut b = Binding::new(owner("pane"));
    let mut o = Ownership::default();
    let old = bind(&mut b, &mut o, &f);
    let queued = begin(&mut b, &mut o);
    b.retire(&mut o).unwrap();
    assert_eq!(b.decision(&o), Decision::AwaitingFirstTurn);
    assert!(b.finish(&queued, f.validate(&queued), &mut o).is_err());
    assert!(!b.accepts(&old, &o));
    let new = begin(&mut b, &mut o);
    assert_ne!(old.launch, new.launch);
    b.finish(&new, Err(Failure::Metadata), &mut o).unwrap_err();
    assert_eq!(
        b.decision(&o),
        Decision::BindingValidationFailed(Failure::Metadata)
    );
}

#[test]
fn repeated_thread_and_path_rebinds_revoke_old_callbacks() {
    let f = Fixture::new();
    let mut b = Binding::new(owner("pane"));
    let mut o = Ownership::default();
    let old = bind(&mut b, &mut o, &f);
    let pending = begin(&mut b, &mut o);
    let new = begin(&mut b, &mut o);
    assert_eq!(old.launch, new.launch);
    assert_eq!(old.thread, new.thread);
    assert_ne!(old.binding, new.binding);
    assert!(b.finish(&pending, f.validate(&pending), &mut o).is_err());
    b.finish(&new, f.validate(&new), &mut o).unwrap();
    assert!(!b.accepts(&old, &o));
    assert!(b.accepts(&new, &o));
}

#[test]
fn loss_stays_unknown_until_fresh_validation_not_a_new_claim() {
    let f = Fixture::new();
    let mut b = Binding::new(owner("pane"));
    let mut o = Ownership::default();
    let old = bind(&mut b, &mut o, &f);
    b.lost(&old, &mut o);
    assert_eq!(b.decision(&o), Decision::Unknown);
    assert_eq!(b.failure(&o), Some(Failure::SourceLost));
    let key = begin(&mut b, &mut o);
    b.finish(&key, Err(Failure::Writer), &mut o).unwrap_err();
    assert_eq!(b.decision(&o), Decision::Unknown);
    bind(&mut b, &mut o, &f);
    b.lost(&old, &mut o);
    assert_eq!(b.decision(&o), Decision::Bound);
}

#[test]
fn route_aliases_and_other_nodes_do_not_claim_this_pane() {
    let mut b = Binding::new(owner("pane"));
    let mut o = Ownership::default();
    for wrong in [
        owner("renamed-label"),
        Owner {
            node: "other.example".into(),
            pane: "pane".into(),
        },
    ] {
        assert_eq!(
            b.begin(&wrong, "child".into(), &mut o).unwrap_err(),
            Failure::Route
        );
        assert_eq!(b.decision(&o), Decision::AwaitingFirstTurn);
    }
}

#[test]
fn duplicate_live_ownership_revokes_both_and_blocks_later_claims() {
    let f = Fixture::new();
    let mut a = Binding::new(owner("a"));
    let mut b = Binding::new(owner("b"));
    let mut o = Ownership::default();
    let old = bind(&mut a, &mut o, &f);
    let key = begin(&mut b, &mut o);
    assert_eq!(
        b.finish(&key, f.validate(&key), &mut o).unwrap_err(),
        Failure::DuplicateOwner
    );
    assert_eq!(a.decision(&o), Decision::Unknown);
    assert_eq!(a.failure(&o), Some(Failure::DuplicateOwner));
    assert!(!a.accepts(&old, &o));
    assert_eq!(
        b.decision(&o),
        Decision::BindingValidationFailed(Failure::DuplicateOwner)
    );
    let key = begin(&mut b, &mut o);
    assert_eq!(
        b.finish(&key, f.validate(&key), &mut o).unwrap_err(),
        Failure::DuplicateOwner
    );
    a.retire(&mut o).unwrap();
    bind(&mut b, &mut o, &f);
    assert_eq!(b.decision(&o), Decision::Bound);
}

#[test]
fn duplicate_ownership_after_destination_trust_cannot_regain_fallback() {
    let f = Fixture::new();
    let mut a = Binding::new(owner("a"));
    let mut b = Binding::new(owner("b"));
    let mut o = Ownership::default();
    bind(&mut b, &mut o, &f);
    b.lost(&b.key.clone().unwrap(), &mut o);
    bind(&mut a, &mut o, &f);
    let key = begin(&mut b, &mut o);
    b.finish(&key, f.validate(&key), &mut o).unwrap_err();
    assert_eq!(b.decision(&o), Decision::Unknown);
}

#[test]
fn metadata_identity_is_first_record_not_filename_or_parent_or_root() {
    let f = Fixture::new();
    let mut b = Binding::new(owner("pane"));
    let mut o = Ownership::default();
    let key = begin(&mut b, &mut o);
    for payload in [
        serde_json::json!({"id":"parent","session_id":"child"}),
        serde_json::json!({"session_id":"child"}),
        serde_json::json!({"id":""}),
    ] {
        std::fs::write(
            &f.path,
            format!(
                "{}\n",
                serde_json::json!({"type":"session_meta","payload":payload})
            ),
        )
        .unwrap();
        assert_eq!(f.validate(&key).unwrap_err(), Failure::Metadata);
    }
    f.metadata("child", crate::codex_version::TESTED_VERSION);
    assert!(f.validate(&key).is_ok());
}

#[test]
fn metadata_requires_complete_bounded_first_line_without_scanning_ahead() {
    let f = Fixture::new();
    let mut b = Binding::new(owner("pane"));
    let mut o = Ownership::default();
    let key = begin(&mut b, &mut o);
    for bytes in [
        "{broken}\n",
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"child\"}}",
        "{}\n{\"type\":\"session_meta\",\"payload\":{\"id\":\"child\"}}\n",
    ] {
        std::fs::write(&f.path, bytes).unwrap();
        assert_eq!(f.validate(&key).unwrap_err(), Failure::Metadata);
    }
    f.metadata("child", crate::codex_version::TESTED_VERSION);
    assert_eq!(
        validate(
            &key,
            &f.home,
            Some(&f.path),
            f.writer,
            &[f.writer],
            &compatibility(),
            1
        )
        .unwrap_err(),
        Failure::Metadata
    );
}

#[test]
fn launch_home_override_missing_path_and_escape_are_never_guessed() {
    let f = Fixture::new();
    let mut b = Binding::new(owner("pane"));
    let mut o = Ownership::default();
    let key = begin(&mut b, &mut o);
    let outside = f.root.join("other-profile.jsonl");
    std::fs::copy(&f.path, &outside).unwrap();
    for path in [
        None,
        Some(outside.as_path()),
        Some(Path::new("sessions/relative.jsonl")),
        Some(f.home.as_path()),
    ] {
        assert_eq!(
            validate(
                &key,
                &f.home,
                path,
                f.writer,
                &[f.writer],
                &compatibility(),
                4096
            )
            .unwrap_err(),
            Failure::Path
        );
    }
    let other_home = f.root.join("empty-override");
    std::fs::create_dir(&other_home).unwrap();
    assert_eq!(
        validate(
            &key,
            &other_home,
            Some(&f.path),
            f.writer,
            &[f.writer],
            &compatibility(),
            4096
        )
        .unwrap_err(),
        Failure::Path
    );
}

#[test]
fn symlink_escape_and_non_regular_sources_are_rejected_without_blocking() {
    let f = Fixture::new();
    let mut b = Binding::new(owner("pane"));
    let mut o = Ownership::default();
    let key = begin(&mut b, &mut o);
    let outside = f.root.join("outside.jsonl");
    std::fs::copy(&f.path, &outside).unwrap();
    std::fs::remove_file(&f.path).unwrap();
    std::os::unix::fs::symlink(&outside, &f.path).unwrap();
    assert_eq!(f.validate(&key).unwrap_err(), Failure::Path);
    std::fs::remove_file(&f.path).unwrap();
    let cpath = std::ffi::CString::new(f.path.to_str().unwrap()).unwrap();
    // SAFETY: the path is a terminated string in this fixture's owned directory.
    assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
    assert_eq!(f.validate(&key).unwrap_err(), Failure::Path);
}

#[test]
fn anchored_open_rejects_symlink_components_and_parent_traversal() {
    let f = Fixture::new();
    std::os::unix::fs::symlink(&f.root, f.home.join("linked-directory")).unwrap();
    for relative in ["linked-directory/outside", "../outside", "sessions", ""] {
        assert!(platform::structured::open_regular_beneath(&f.home, Path::new(relative)).is_err());
    }
}

#[test]
fn writer_pid_reuse_missing_and_ambiguous_writer_are_rejected() {
    let f = Fixture::new();
    let mut b = Binding::new(owner("pane"));
    let mut o = Ownership::default();
    let key = begin(&mut b, &mut o);
    let reused = Writer {
        birth: f.writer.birth.wrapping_add(1),
        ..f.writer
    };
    for writers in [vec![], vec![reused], vec![f.writer, f.writer]] {
        assert_eq!(
            validate(
                &key,
                &f.home,
                Some(&f.path),
                f.writer,
                &writers,
                &compatibility(),
                4096
            )
            .unwrap_err(),
            Failure::Writer
        );
    }
    assert_eq!(
        validate(
            &key,
            &f.home,
            Some(&f.path),
            reused,
            &[reused],
            &compatibility(),
            4096
        )
        .unwrap_err(),
        Failure::Writer
    );
}

#[test]
fn binding_consumes_s1_version_evidence_and_records_actual_metadata() {
    let f = Fixture::new();
    let mut b = Binding::new(owner("pane"));
    let mut o = Ownership::default();
    let key = begin(&mut b, &mut o);
    for version in ["0.159.0", "0.160.2", ""] {
        f.metadata("child", version);
        assert_eq!(f.validate(&key).unwrap_err(), Failure::Version);
    }
    f.metadata("child", crate::codex_version::TESTED_VERSION);
    let mut evidence = compatibility();
    evidence.writer_version = Some("0.160.2".into());
    assert_eq!(
        validate(
            &key,
            &f.home,
            Some(&f.path),
            f.writer,
            &[f.writer],
            &evidence,
            4096
        )
        .unwrap_err(),
        Failure::Version
    );
    evidence = compatibility();
    evidence.probe_failure = Some(crate::codex_version::ProbeFailure::Timeout);
    assert_eq!(
        validate(
            &key,
            &f.home,
            Some(&f.path),
            f.writer,
            &[f.writer],
            &evidence,
            4096
        )
        .unwrap_err(),
        Failure::Version
    );
}

#[test]
fn validation_result_cannot_cross_a_binding_generation() {
    let f = Fixture::new();
    let mut b = Binding::new(owner("pane"));
    let mut o = Ownership::default();
    let old = begin(&mut b, &mut o);
    let value = f.validate(&old).unwrap();
    let new = begin(&mut b, &mut o);
    assert_eq!(
        b.finish(&new, Ok(value), &mut o).unwrap_err(),
        Failure::Route
    );
    assert_eq!(
        b.decision(&o),
        Decision::BindingValidationFailed(Failure::Route)
    );
}
