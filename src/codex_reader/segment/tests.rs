use super::*;
use crate::codex_binding::{self, Decision, Launch, Owner, Writer};
use crate::codex_rollout::Turn;
use crate::codex_version::{Compatibility, TESTED_VERSION};
use crate::structured::launch::SessionConfigDir;
use serde_json::json;
use std::{fs, io::Write};

pub(super) struct Fixture {
    pub(super) root: SessionConfigDir,
    pub(super) path: std::path::PathBuf,
}
impl Fixture {
    pub(super) fn new() -> Self {
        let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let path = root.path().join("rollout.jsonl");
        fs::write(&path, line(json!({"type":"session_meta","payload":{"id":"child","cli_version":TESTED_VERSION}}))).unwrap();
        Self { root, path }
    }
    pub(super) fn append(&self, bytes: &[u8]) {
        fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }
    fn snapshot(&self) -> Snapshot {
        let mut capture =
            Capture::new(File::open(&self.path).unwrap(), self.path.clone(), limits()).unwrap();
        for _ in 0..10000 {
            if let Some(snapshot) = capture.step(Instant::now()).unwrap() {
                return snapshot;
            }
        }
        panic!("snapshot did not reach EOF");
    }
    pub(super) fn validated(&self, key: &Key) -> Result<Validated, Failure> {
        let writer = Writer {
            pid: std::process::id(),
            birth: crate::platform::process_start_time(std::process::id()).unwrap(),
        };
        codex_binding::validate(
            key,
            &Launch {
                owner: key.owner.clone(),
                generation: key.launch,
                home: self.root.path().to_owned(),
                writer,
            },
            Some(&self.path),
            &[writer],
            &Compatibility {
                metadata_version: None,
                writer_version: Some(TESTED_VERSION.into()),
                probe_failure: None,
            },
            4096,
        )
    }
}
pub(super) fn limits() -> Limits {
    Limits {
        work_bytes: 1024 * 1024,
        record_bytes: 4096,
        turn_ids: 64,
    }
}
pub(super) fn owner() -> Owner {
    Owner {
        node: "segment.example".into(),
        pane: "fixture".into(),
    }
}
pub(super) fn key(b: &mut Binding) -> Key {
    b.begin(&owner(), b.launch_generation(), "child".into())
        .unwrap()
}
pub(super) fn line(value: Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    bytes
}
pub(super) fn event(kind: &str, id: &str) -> Vec<u8> {
    line(json!({"type":"event_msg","payload":{"type":kind,"turn_id":id}}))
}
fn delimiter(thread: &str) -> Vec<u8> {
    line(
        json!({"type":"event_msg","payload":{"type":"thread_settings_applied","thread_id":thread}}),
    )
}
fn admit(
    f: &Fixture,
    b: &mut Binding,
    owners: &mut Ownership,
    mode: Mode,
) -> Result<Segment, Error> {
    let k = key(b);
    admit_ready(
        b,
        &k,
        f.validated(&k),
        mode,
        owners,
        limits(),
        Instant::now(),
    )
}

fn admit_ready(
    b: &mut Binding,
    k: &Key,
    validated: Result<Validated, Failure>,
    mode: Mode,
    owners: &mut Ownership,
    limits: Limits,
    now: Instant,
) -> Result<Segment, Error> {
    let mut admission = Admission::begin(b, k, validated, mode, owners, limits)?;
    for _ in 0..10000 {
        if let Some(segment) = admission.step(b, owners, now)? {
            return Ok(segment);
        }
    }
    panic!("admission did not reach EOF");
}

#[test]
fn resume_ignores_completed_and_unfinished_history_and_inherited_partial_start() {
    for ended in [false, true] {
        for partial in [false, true] {
            let f = Fixture::new();
            f.append(&event("task_started", "old"));
            if ended {
                f.append(&event("task_complete", "old"));
            }
            let old_partial = event("task_started", "inherited");
            if partial {
                f.append(&old_partial[..old_partial.len() - 1]);
            }
            let snapshot = f.snapshot();
            if partial {
                f.append(b"\n");
                f.append(&event("task_complete", "inherited"));
            }
            f.append(&event("task_started", "current"));
            let mut b = Binding::new(owner());
            let mut owners = Ownership::default();
            let mut segment = admit(
                &f,
                &mut b,
                &mut owners,
                Mode::Resume {
                    snapshot,
                    copied: false,
                },
            )
            .unwrap();
            assert_eq!(segment.observe(Instant::now()).turn, Turn::Working);
            assert_eq!(
                segment.observe(Instant::now()).boundary.unwrap().turn_id,
                "current"
            );
            f.append(&event("task_complete", "current"));
            segment.poll(Instant::now()).unwrap();
            assert_eq!(segment.observe(Instant::now()).turn, Turn::Idle);
        }
    }
}

#[test]
fn old_completion_or_partial_record_never_bootstraps_resume_idle_or_trust() {
    for partial in [false, true] {
        let f = Fixture::new();
        f.append(&event("task_started", "old"));
        let snapshot = f.snapshot();
        f.append(&event("task_complete", "old"));
        if partial {
            f.append(b"{\"type\":\"event_msg\"");
        }
        let mut b = Binding::new(owner());
        let mut owners = Ownership::default();
        assert!(matches!(
            admit(
                &f,
                &mut b,
                &mut owners,
                Mode::Resume {
                    snapshot,
                    copied: false
                }
            ),
            Err(Error::Attribution)
        ));
        assert_eq!(
            b.decision(&owners),
            Decision::BindingValidationFailed(Failure::Metadata)
        );
    }
}

#[test]
fn copied_resume_needs_exact_prefix_explicit_copy_and_local_validation() {
    for copy in [false, true] {
        for changed in [false, true] {
            let f = Fixture::new();
            f.append(&event("task_started", "old"));
            let snapshot = f.snapshot();
            let dest = Fixture::new();
            fs::copy(&f.path, &dest.path).unwrap();
            if changed {
                let bytes = fs::read(&dest.path).unwrap();
                fs::write(
                    &dest.path,
                    String::from_utf8(bytes).unwrap().replace("old", "bad"),
                )
                .unwrap();
            }
            dest.append(&event("task_started", "current"));
            let mut b = Binding::new(owner());
            let mut owners = Ownership::default();
            let result = admit(
                &dest,
                &mut b,
                &mut owners,
                Mode::Resume {
                    snapshot,
                    copied: copy,
                },
            );
            assert_eq!(result.is_ok(), copy && !changed);
        }
    }
    let f = Fixture::new();
    let snapshot = f.snapshot();
    f.append(&event("task_started", "current"));
    let mut b = Binding::new(owner());
    let mut owners = Ownership::default();
    let k = key(&mut b);
    assert!(matches!(
        admit_ready(
            &mut b,
            &k,
            Err(Failure::Writer),
            Mode::Resume {
                snapshot,
                copied: true
            },
            &mut owners,
            limits(),
            Instant::now()
        ),
        Err(Error::Binding(Failure::Writer))
    ));
    assert_eq!(
        b.decision(&owners),
        Decision::BindingValidationFailed(Failure::Writer)
    );
}

#[test]
fn fork_delimiter_excludes_parent_boundaries_in_copied_and_referenced_history() {
    for referenced in [false, true] {
        let f = Fixture::new();
        let mut ordinal = 41;
        if referenced {
            fs::write(&f.path,line(json!({"type":"session_meta","ordinal":40,"payload":{"id":"child","cli_version":TESTED_VERSION,"history_mode":"paginated","history_base":{"thread_id":"parent","end_ordinal_exclusive":40,"end_byte_offset":400}}}))).unwrap();
        } else {
            f.append(&event("task_started", "parent"));
            f.append(&event("task_complete", "parent"));
            f.append(&delimiter("parent"));
        }
        for bytes in [delimiter("child"), event("task_started", "current")] {
            let mut record: Value = serde_json::from_slice(&bytes).unwrap();
            if referenced {
                record["ordinal"] = ordinal.into();
                ordinal += 1;
            }
            f.append(&line(record));
        }
        let mut b = Binding::new(owner());
        let mut owners = Ownership::default();
        let segment = admit(&f, &mut b, &mut owners, Mode::Fork).unwrap();
        assert_eq!(
            segment.observe(Instant::now()).boundary.unwrap().turn_id,
            "current"
        );
        assert_eq!(segment.observe(Instant::now()).turn, Turn::Working);
    }
}

#[test]
fn missing_or_ambiguous_fork_delimiter_preserves_launch_trust_scope() {
    for trusted in [false, true] {
        for count in [0, 2] {
            let f = Fixture::new();
            let mut b = Binding::new(owner());
            let mut owners = Ownership::default();
            if trusted {
                f.append(&event("task_started", "previous"));
                admit(&f, &mut b, &mut owners, Mode::Fresh).unwrap();
            }
            f.append(&delimiter("parent"));
            for _ in 0..count {
                f.append(&delimiter("child"));
            }
            f.append(&event("task_started", "current"));
            assert!(matches!(
                admit(&f, &mut b, &mut owners, Mode::Fork),
                Err(Error::Attribution)
            ));
            assert_eq!(
                b.decision(&owners),
                if trusted {
                    Decision::Unknown
                } else {
                    Decision::BindingValidationFailed(Failure::Metadata)
                }
            );
        }
    }
}

#[test]
fn compaction_transfers_owned_reducer_only_with_exact_continuity() {
    for changed in [false, true] {
        let f = Fixture::new();
        f.append(&event("task_started", "current"));
        let mut b = Binding::new(owner());
        let mut owners = Ownership::default();
        let segment = admit(&f, &mut b, &mut owners, Mode::Fresh).unwrap();
        let before = segment.observe(Instant::now()).boundary.unwrap().clone();
        let handoff = segment.handoff(&b, &owners, Instant::now()).unwrap();
        if changed {
            let tmp = f.root.path().join("replacement");
            fs::copy(&f.path, &tmp).unwrap();
            fs::rename(tmp, &f.path).unwrap();
        }
        let result = admit(&f, &mut b, &mut owners, Mode::Compact(handoff));
        if changed {
            assert!(matches!(result, Err(Error::Compaction)));
            assert_eq!(
                Error::Compaction.as_str(),
                "codex_compaction_continuity_unproven"
            );
            assert_eq!(b.decision(&owners), Decision::Unknown);
        } else {
            let mut segment = result.unwrap();
            assert_eq!(segment.observe(Instant::now()).boundary, Some(&before));
            f.append(&event("task_complete", "current"));
            segment.poll(Instant::now()).unwrap();
            assert_eq!(segment.observe(Instant::now()).turn, Turn::Idle);
        }
    }
}

#[test]
fn revoked_handoff_and_stale_callback_cannot_change_new_claim() {
    let f = Fixture::new();
    f.append(&event("task_started", "current"));
    let mut b = Binding::new(owner());
    let mut owners = Ownership::default();
    let segment = admit(&f, &mut b, &mut owners, Mode::Fresh).unwrap();
    let stale = segment.reader.validated.key().clone();
    let current = key(&mut b);
    assert!(matches!(
        segment.handoff(&b, &owners, Instant::now()),
        Err(Error::Compaction)
    ));
    assert!(matches!(
        admit_ready(
            &mut b,
            &stale,
            f.validated(&stale),
            Mode::Fresh,
            &mut owners,
            limits(),
            Instant::now()
        ),
        Err(Error::Binding(Failure::Route))
    ));
    assert!(b.pending(&current));
    admit_ready(
        &mut b,
        &current,
        f.validated(&current),
        Mode::Fresh,
        &mut owners,
        limits(),
        Instant::now(),
    )
    .unwrap();
}

#[test]
fn admitted_source_loss_invalidates_cached_working_and_known_retirement_resets_trust() {
    let f = Fixture::new();
    f.append(&event("task_started", "current"));
    let mut b = Binding::new(owner());
    let mut owners = Ownership::default();
    let mut segment = admit(&f, &mut b, &mut owners, Mode::Fresh).unwrap();
    fs::write(&f.path, b"").unwrap();
    assert!(segment.poll(Instant::now()).is_err());
    assert_eq!(segment.observe(Instant::now()).turn, Turn::Unknown);
    b.retire(&mut owners).unwrap();
    assert!(admit(&f, &mut b, &mut owners, Mode::Fork).is_err());
    assert!(matches!(
        b.decision(&owners),
        Decision::BindingValidationFailed(_)
    ));
}

#[test]
fn compaction_preserves_partial_cursor_and_rejects_prefix_or_destination_changes() {
    let f = Fixture::new();
    f.append(&event("task_started", "current"));
    let mut b = Binding::new(owner());
    let mut owners = Ownership::default();
    let mut segment = admit(&f, &mut b, &mut owners, Mode::Fresh).unwrap();
    let complete = event("task_complete", "current");
    let middle = complete.len() / 2;
    f.append(&complete[..middle]);
    segment.poll(Instant::now()).unwrap();
    assert_eq!(segment.reader.scan.cursor.partial, complete[..middle]);
    let handoff = segment.handoff(&b, &owners, Instant::now()).unwrap();
    f.append(&complete[middle..]);
    let segment = admit(&f, &mut b, &mut owners, Mode::Compact(handoff)).unwrap();
    assert_eq!(segment.observe(Instant::now()).turn, Turn::Idle);
    let handoff = segment.handoff(&b, &owners, Instant::now()).unwrap();
    let old = String::from_utf8(fs::read(&f.path).unwrap()).unwrap();
    fs::write(&f.path, old.replace("current", "changed")).unwrap();
    assert!(matches!(
        admit(&f, &mut b, &mut owners, Mode::Compact(handoff)),
        Err(Error::Compaction)
    ));

    let f = Fixture::new();
    f.append(&event("task_started", "current"));
    let mut source = Binding::new(owner());
    let mut owners = Ownership::default();
    let segment = admit(&f, &mut source, &mut owners, Mode::Fresh).unwrap();
    let handoff = segment.handoff(&source, &owners, Instant::now()).unwrap();
    source.retire(&mut owners).unwrap();
    let destination = Owner {
        node: "destination.example".into(),
        pane: "destination".into(),
    };
    let mut binding = Binding::new(destination.clone());
    let key = binding.begin(&destination, 0, "child".into()).unwrap();
    assert!(matches!(
        admit_ready(
            &mut binding,
            &key,
            f.validated(&key),
            Mode::Compact(handoff),
            &mut owners,
            limits(),
            Instant::now()
        ),
        Err(Error::Compaction)
    ));
}

#[test]
fn compaction_rejects_a_different_live_local_writer() {
    let f = Fixture::new();
    f.append(&event("task_started", "current"));
    let mut b = Binding::new(owner());
    let mut owners = Ownership::default();
    let segment = admit(&f, &mut b, &mut owners, Mode::Fresh).unwrap();
    let handoff = segment.handoff(&b, &owners, Instant::now()).unwrap();
    struct OwnedChild(std::process::Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let child = OwnedChild(
        crate::process::TracedCommand::new("/bin/sh", "codex-segment-test")
            .args(["-c", "read reply"])
            .stdin(std::process::Stdio::piped())
            .spawn_traced()
            .unwrap(),
    );
    let writer = Writer {
        pid: child.0.id(),
        birth: crate::platform::process_start_time(child.0.id()).unwrap(),
    };
    let k = key(&mut b);
    let candidate = codex_binding::validate(
        &k,
        &Launch {
            owner: k.owner.clone(),
            generation: k.launch,
            home: f.root.path().to_owned(),
            writer,
        },
        Some(&f.path),
        &[writer],
        &Compatibility {
            metadata_version: None,
            writer_version: Some(TESTED_VERSION.into()),
            probe_failure: None,
        },
        4096,
    );
    let result = admit_ready(
        &mut b,
        &k,
        candidate,
        Mode::Compact(handoff),
        &mut owners,
        limits(),
        Instant::now(),
    );
    assert!(matches!(result, Err(Error::Compaction)));
}

#[test]
fn child_delimiter_without_child_start_cannot_promote_imported_turns() {
    for ended in [false, true] {
        let f = Fixture::new();
        f.append(&event("task_started", "parent"));
        if ended {
            f.append(&event("task_complete", "parent"));
        }
        f.append(&delimiter("child"));
        let mut binding = Binding::new(owner());
        let mut owners = Ownership::default();
        assert!(matches!(
            admit(&f, &mut binding, &mut owners, Mode::Fork),
            Err(Error::Attribution)
        ));
        assert_eq!(
            binding.decision(&owners),
            Decision::BindingValidationFailed(Failure::Metadata)
        );
    }
}

fn small_limits() -> Limits {
    Limits {
        work_bytes: 11,
        ..limits()
    }
}
fn capture_small(f: &Fixture) -> Snapshot {
    let mut capture =
        Capture::new(File::open(&f.path).unwrap(), f.path.clone(), small_limits()).unwrap();
    for _ in 0..100_000 {
        if let Some(snapshot) = capture.step(Instant::now()).unwrap() {
            return snapshot;
        }
        assert!(capture.scan.cursor.partial.len() <= small_limits().record_bytes);
    }
    panic!("capture never reached EOF");
}

#[test]
fn long_history_snapshot_resume_fork_and_compaction_admit_with_bounded_work() {
    for mode_name in ["resume", "copy", "fork", "fresh"] {
        let f = Fixture::new();
        for _ in 0..100 {
            f.append(&line(
                json!({"type":"future_history","payload":{"text":"bounded"}}),
            ));
        }
        let mode = match mode_name {
            "resume" | "copy" => {
                let old_start = event("task_started", "inherited");
                f.append(&old_start[..old_start.len() - 1]);
                let snapshot = capture_small(&f);
                assert_eq!(
                    snapshot.checkpoint.read_offset,
                    fs::metadata(&f.path).unwrap().len()
                );
                if mode_name == "copy" {
                    let replacement = f.root.path().join("copy");
                    fs::copy(&f.path, &replacement).unwrap();
                    fs::rename(replacement, &f.path).unwrap();
                }
                f.append(b"\n");
                f.append(&event("task_complete", "inherited"));
                Mode::Resume {
                    snapshot,
                    copied: mode_name == "copy",
                }
            }
            "fork" => {
                f.append(&event("task_started", "parent"));
                f.append(&event("task_complete", "parent"));
                f.append(&delimiter("parent"));
                f.append(&delimiter("child"));
                Mode::Fork
            }
            _ => Mode::Fresh,
        };
        f.append(&event("task_started", "current"));
        let mut binding = Binding::new(owner());
        let mut owners = Ownership::default();
        let k = key(&mut binding);
        let mut admission = Admission::begin(
            &mut binding,
            &k,
            f.validated(&k),
            mode,
            &mut owners,
            small_limits(),
        )
        .unwrap();
        assert!(admission
            .step(&mut binding, &mut owners, Instant::now())
            .unwrap()
            .is_none());
        assert_eq!(binding.decision(&owners), Decision::Unknown);
        let mut result = None;
        for _ in 0..100_000 {
            if let Some(segment) = admission
                .step(&mut binding, &mut owners, Instant::now())
                .unwrap()
            {
                result = Some(segment);
                break;
            }
            assert!(binding.pending(&k));
            assert_eq!(binding.decision(&owners), Decision::Unknown);
        }
        let segment = result.expect("admission never reached EOF");
        assert_eq!(binding.decision(&owners), Decision::Bound);
        assert_eq!(
            segment.observe(Instant::now()).boundary.unwrap().turn_id,
            "current"
        );
        let before = segment.observe(Instant::now()).boundary.unwrap().clone();
        let handoff = segment.handoff(&binding, &owners, Instant::now()).unwrap();
        let k = key(&mut binding);
        let segment = admit_ready(
            &mut binding,
            &k,
            f.validated(&k),
            Mode::Compact(handoff),
            &mut owners,
            small_limits(),
            Instant::now(),
        )
        .unwrap();
        assert_eq!(segment.observe(Instant::now()).boundary, Some(&before));
        assert_eq!(segment.observe(Instant::now()).turn, Turn::Working);
    }
}

#[test]
fn late_duplicate_delimiter_and_malformed_history_never_commit_early_trust() {
    for malformed in [false, true] {
        let f = Fixture::new();
        f.append(&delimiter("child"));
        f.append(&event("task_started", "current"));
        for _ in 0..30 {
            f.append(&line(json!({"type":"future_history"})));
        }
        if malformed {
            f.append(b"invalid json\n");
        } else {
            f.append(&delimiter("child"));
        }
        let mut binding = Binding::new(owner());
        let mut owners = Ownership::default();
        let k = key(&mut binding);
        assert!(admit_ready(
            &mut binding,
            &k,
            f.validated(&k),
            Mode::Fork,
            &mut owners,
            small_limits(),
            Instant::now()
        )
        .is_err());
        assert_eq!(
            binding.decision(&owners),
            Decision::BindingValidationFailed(Failure::Metadata)
        );
    }
}

#[test]
fn incremental_admission_cannot_finish_or_reject_a_replacement_claim() {
    let f = Fixture::new();
    f.append(&event("task_started", "current"));
    let mut b = Binding::new(owner());
    let mut owners = Ownership::default();
    let old = key(&mut b);
    let mut admission = Admission::begin(
        &mut b,
        &old,
        f.validated(&old),
        Mode::Fresh,
        &mut owners,
        small_limits(),
    )
    .unwrap();
    assert!(admission
        .step(&mut b, &mut owners, Instant::now())
        .unwrap()
        .is_none());
    let current = key(&mut b);
    assert!(matches!(
        admission.step(&mut b, &mut owners, Instant::now()),
        Err(Error::Binding(Failure::Route))
    ));
    assert!(b.pending(&current));
    assert_eq!(b.decision(&owners), Decision::Unknown);
}
