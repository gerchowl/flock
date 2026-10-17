use super::*;
use crate::codex_binding::{self, Key, Launch, Owner, Writer};
use crate::codex_rollout::Turn;
use crate::codex_version::{Compatibility, TESTED_VERSION};
use crate::structured::launch::SessionConfigDir;
use serde_json::json;
use std::{fs, io::Write, time::Duration};

struct Fixture {
    root: SessionConfigDir,
    path: std::path::PathBuf,
}
impl Fixture {
    fn new(meta: Value) -> Self {
        let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let path = root.path().join("physical.jsonl");
        fs::write(&path, line(meta)).unwrap();
        Self { root, path }
    }
    fn append(&self, bytes: &[u8]) {
        fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }
    fn reader(&self, limits: Limits) -> Reader {
        let writer = Writer {
            pid: std::process::id(),
            birth: crate::platform::process_start_time(std::process::id()).unwrap(),
        };
        let key = Key {
            owner: Owner {
                node: "reader.example".into(),
                pane: "fixture".into(),
            },
            launch: 1,
            binding: 1,
            thread: "child".into(),
        };
        let launch = Launch {
            owner: key.owner.clone(),
            generation: key.launch,
            home: self.root.path().to_owned(),
            writer,
        };
        let compatibility = Compatibility {
            metadata_version: None,
            writer_version: Some(TESTED_VERSION.into()),
            probe_failure: None,
        };
        let validated = codex_binding::validate(
            &key,
            &launch,
            Some(&self.path),
            &[writer],
            &compatibility,
            4096,
        )
        .unwrap();
        Reader::new(validated, limits).unwrap()
    }
}
fn limits() -> Limits {
    Limits {
        file_bytes: 2 * 1024 * 1024,
        record_bytes: 256 * 1024,
        turn_ids: 256,
    }
}
fn meta() -> Value {
    json!({"type":"session_meta","payload":{"id":"child","cli_version":TESTED_VERSION}})
}
fn line(record: Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&record).unwrap();
    bytes.push(b'\n');
    bytes
}
fn event(kind: &str, id: &str) -> Vec<u8> {
    line(json!({"type":"event_msg","payload":{"type":kind,"turn_id":id}}))
}

#[test]
fn verified_appends_reduce_once_and_quiet_eof_has_no_age_lease() {
    let f = Fixture::new(meta());
    f.append(&event("task_started", "active"));
    let mut reader = f.reader(limits());
    let mut reducer = limits().reducer(None);
    let now = Instant::now();
    let first = reader.poll(now).unwrap();
    assert_eq!(first.from, 0);
    reducer.feed(&first.bytes, now);
    assert_eq!(reducer.observe(now).turn, Turn::Working);
    for hours in [1, 2, 100] {
        let later = now + Duration::from_secs(hours * 3600);
        let update = reader.poll(later).unwrap();
        assert!(update.bytes.is_empty());
        assert_eq!(update.checkpoint, first.checkpoint);
        assert_eq!(update.eof_at, later);
        reducer.feed(&update.bytes, later);
        assert_eq!(reducer.observe(later).turn, Turn::Working);
    }
    let completion = event("task_complete", "active");
    f.append(&completion);
    let update = reader.poll(now).unwrap();
    assert_eq!(update.bytes, completion);
    assert_eq!(update.from, first.checkpoint.read_offset);
    reducer.feed(&update.bytes, now);
    assert_eq!(reducer.observe(now).turn, Turn::Idle);
    let later = now + Duration::from_secs(100 * 3600);
    reducer.feed(&reader.poll(later).unwrap().bytes, later);
    assert_eq!(reducer.observe(later).turn, Turn::Idle);
}

#[test]
fn partial_eof_and_prelaunch_snapshot_retain_exact_suffix_and_complete_offset() {
    let f = Fixture::new(meta());
    f.append(&event("task_started", "active"));
    let mut reader = f.reader(limits());
    let mut reducer = limits().reducer(None);
    let now = Instant::now();
    let initial = reader.poll(now).unwrap();
    reducer.feed(&initial.bytes, now);
    let completion = event("task_complete", "active");
    f.append(&completion[..completion.len() - 1]);
    let partial = reader.poll(now).unwrap();
    reducer.feed(&partial.bytes, now);
    assert_eq!(reducer.observe(now).turn, Turn::Working);
    assert_eq!(
        partial.checkpoint.position(),
        (
            initial.checkpoint.read_offset,
            &completion[..completion.len() - 1]
        )
    );
    assert_eq!(reader.poll(now).unwrap().checkpoint, partial.checkpoint);
    f.append(b"\n");
    reducer.feed(&reader.poll(now).unwrap().bytes, now);
    assert_eq!(reducer.observe(now).turn, Turn::Idle);
    let start = event("task_started", "new");
    f.append(&start[..start.len() - 1]);
    let snapshot = reader.poll(now).unwrap();
    reducer.feed(&snapshot.bytes, now);
    assert_eq!(reducer.observe(now).turn, Turn::Unknown);
    assert_eq!(reducer.observe(now).reason, Some(Reason::PartialRecord));
    assert_eq!(snapshot.checkpoint.partial, start[..start.len() - 1]);
}

#[test]
fn replacement_shrink_prefix_edits_and_skipped_bytes_are_sticky_source_loss() {
    for change in [
        "replacement",
        "shrink",
        "edit",
        "skip",
        "missing",
        "directory",
        "symlink",
    ] {
        let f = Fixture::new(meta());
        f.append(&event("task_started", "active"));
        let original = fs::read(&f.path).unwrap();
        let mut reader = f.reader(limits());
        reader.poll(Instant::now()).unwrap();
        match change {
            "replacement" => {
                let other = f.root.path().join("other");
                fs::write(&other, &original).unwrap();
                fs::rename(&other, &f.path).unwrap();
            }
            "shrink" => {
                fs::write(&f.path, &original[..original.len() - 1]).unwrap();
            }
            "edit" => {
                let mut edited = original.clone();
                let n = edited.iter().position(|byte| *byte == b'c').unwrap();
                edited[n] = b'x';
                fs::write(&f.path, edited).unwrap();
            }
            "skip" => {
                fs::write(&f.path, &original[1..]).unwrap();
            }
            "missing" => {
                fs::remove_file(&f.path).unwrap();
            }
            "directory" => {
                fs::remove_file(&f.path).unwrap();
                fs::create_dir(&f.path).unwrap();
            }
            "symlink" => {
                let target = f.root.path().join("target");
                fs::rename(&f.path, &target).unwrap();
                std::os::unix::fs::symlink(&target, &f.path).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            matches!(
                reader.poll(Instant::now()),
                Err(Reason::SourceDiscontinuity)
            ),
            "{change}"
        );
        assert!(
            matches!(
                reader.poll(Instant::now()),
                Err(Reason::SourceDiscontinuity)
            ),
            "sticky {change}"
        );
    }
}

#[test]
fn malformed_gaps_and_mixed_ordinal_modes_fail_before_bytes_are_delivered() {
    for bad in [
        b"{bad}\n".to_vec(),
        line(json!({"type":"event_msg","payload":{}})),
        line(json!({"type":"response_item","payload":null})),
        line(json!({"type":"future","ordinal":1})),
    ] {
        let f = Fixture::new(meta());
        let mut reader = f.reader(limits());
        reader.poll(Instant::now()).unwrap();
        f.append(&bad);
        assert!(reader.poll(Instant::now()).is_err());
        f.append(&event("task_started", "later"));
        assert!(reader.poll(Instant::now()).is_err());
    }
    for wrong in [None, Some(0), Some(2), Some(u64::MAX)] {
        let mut metadata = meta();
        metadata["ordinal"] = 0.into();
        metadata["payload"]["history_mode"] = "paginated".into();
        let f = Fixture::new(metadata);
        let mut reader = f.reader(limits());
        reader.poll(Instant::now()).unwrap();
        let mut record = json!({"type":"future"});
        if let Some(value) = wrong {
            record["ordinal"] = value.into();
        }
        f.append(&line(record));
        assert!(matches!(
            reader.poll(Instant::now()),
            Err(Reason::OrdinalDiscontinuity)
        ));
    }
}

#[test]
fn referenced_history_uses_declared_base_and_unknown_kinds_preserve_continuity() {
    let mut metadata = meta();
    metadata["ordinal"] = 41.into();
    metadata["payload"]["history_mode"] = "paginated".into();
    metadata["payload"]["history_base"] =
        json!({"thread_id":"parent","end_ordinal_exclusive":41,"end_byte_offset":900});
    let f = Fixture::new(metadata.clone());
    f.append(&line(json!({"type":"future","ordinal":42})));
    let mut reader = f.reader(limits());
    let update = reader.poll(Instant::now()).unwrap();
    assert_eq!(update.checkpoint.next_ordinal, Some(43));
    metadata["ordinal"] = 0.into();
    let f = Fixture::new(metadata);
    assert!(matches!(
        f.reader(limits()).poll(Instant::now()),
        Err(Reason::OrdinalDiscontinuity)
    ));
}

#[test]
fn file_record_and_turn_budgets_never_truncate_or_evict_evidence() {
    let f = Fixture::new(meta());
    f.append(&event("task_started", "first"));
    for bounded in [
        Limits {
            file_bytes: 2,
            ..limits()
        },
        Limits {
            record_bytes: 2,
            ..limits()
        },
    ] {
        assert!(matches!(
            f.reader(bounded).poll(Instant::now()),
            Err(Reason::CapacityExceeded)
        ));
    }
    let bounded = Limits {
        turn_ids: 1,
        ..limits()
    };
    let mut reader = f.reader(bounded);
    let mut reducer = bounded.reducer(None);
    let now = Instant::now();
    reducer.feed(&reader.poll(now).unwrap().bytes, now);
    f.append(&event("task_complete", "first"));
    f.append(&event("task_started", "second"));
    reducer.feed(&reader.poll(now).unwrap().bytes, now);
    assert_eq!(reducer.observe(now).reason, Some(Reason::CapacityExceeded));
}

#[test]
fn real_fixture_replays_from_the_bound_handle_in_incremental_record_steps() {
    let bytes = include_bytes!("../../tests/fixtures/codex-rollout/real-0.160.1.jsonl");
    let mut lines = bytes.split_inclusive(|byte| *byte == b'\n');
    let metadata = lines.next().unwrap();
    let mut parsed: Value = serde_json::from_slice(metadata).unwrap();
    parsed["payload"]["id"] = "child".into();
    let f = Fixture::new(parsed);
    let mut reader = f.reader(limits());
    let mut reducer = limits().reducer(Some(0));
    let now = Instant::now();
    reducer.feed(&reader.poll(now).unwrap().bytes, now);
    for record in lines {
        f.append(record);
        let update = reader.poll(now).unwrap();
        assert_eq!(update.bytes, record);
        reducer.feed(&update.bytes, now);
    }
    assert_eq!(reducer.observe(now).turn, Turn::Idle);
    assert!(reducer.observe(now).reason.is_none());
}

#[test]
fn changing_metadata_after_validation_cannot_install_a_different_thread() {
    let f = Fixture::new(meta());
    let mut reader = f.reader(limits());
    let mut changed = meta();
    changed["payload"]["id"] = "other".into();
    fs::write(&f.path, line(changed)).unwrap();
    assert!(matches!(
        reader.poll(Instant::now()),
        Err(Reason::SourceDiscontinuity)
    ));
}

#[test]
fn invalid_history_modes_bases_and_ordinal_overflow_cannot_guess_continuity() {
    for mode in [Value::Null, json!(9), json!("future")] {
        let mut metadata = meta();
        metadata["payload"]["history_mode"] = mode;
        let f = Fixture::new(metadata);
        assert!(matches!(
            f.reader(limits()).poll(Instant::now()),
            Err(Reason::OrdinalDiscontinuity)
        ));
    }
    for base in [
        json!({}),
        json!({"thread_id":"parent","end_byte_offset":0}),
        json!({"thread_id":"","end_byte_offset":0,"end_ordinal_exclusive":0}),
        json!({"thread_id":"parent","end_byte_offset":0,"end_ordinal_exclusive":u64::MAX}),
    ] {
        let mut metadata = meta();
        metadata["payload"]["history_mode"] = "paginated".into();
        metadata["ordinal"] = base
            .get("end_ordinal_exclusive")
            .cloned()
            .unwrap_or(json!(0));
        metadata["payload"]["history_base"] = base;
        let f = Fixture::new(metadata);
        assert!(matches!(
            f.reader(limits()).poll(Instant::now()),
            Err(Reason::OrdinalDiscontinuity)
        ));
    }
}
