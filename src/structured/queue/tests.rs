use super::model::{Delivery, Kind, Priority, Producer, State};
use super::*;
use std::collections::BTreeMap;
use std::net::Shutdown;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::Duration;

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let mut random = [0; 16];
        getrandom::fill(&mut random).unwrap();
        let id: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let path = std::env::temp_dir().join(format!("queue-{id}"));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn open(&self) -> Journal {
        Journal::open(&self.0, "fixture-agent").unwrap()
    }
    fn identity_prefix(&self) -> Vec<u8> {
        let bytes = fs::read(self.path()).unwrap();
        let line = bytes
            .split_inclusive(|b| *b == b'\n')
            .next()
            .unwrap_or_default();
        if serde_json::from_slice::<Value>(line)
            .ok()
            .is_some_and(|v| v["kind"] == "queue_identity")
        {
            line.to_vec()
        } else {
            vec![]
        }
    }
    fn write_records(&self, records: impl AsRef<[u8]>) -> Vec<u8> {
        let mut bytes = self.identity_prefix();
        bytes.extend(records.as_ref());
        fs::write(self.path(), &bytes).unwrap();
        bytes
    }
    fn path(&self) -> PathBuf {
        self.0.join("sessions/fixture-agent/queue.jsonl")
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn item(id: &str) -> Item {
    Item {
        item_id: id.into(),
        producer: Producer::Human,
        kind: Kind::Prompt,
        state: State::Queued,
        delivery: Delivery::AtIdle,
        priority: Priority::Normal,
        body: format!("text for {id}"),
        attachments: vec![],
        metadata: BTreeMap::new(),
    }
}
fn upsert(id: &str, position: usize) -> Change {
    Change::Upsert {
        item: Box::new(item(id)),
        position,
    }
}
fn socket_file() -> (File, UnixStream) {
    let (left, right) = UnixStream::pair().unwrap();
    (File::from(OwnedFd::from(left)), right)
}

#[test]
fn durable_append_reopen_preserves_item_changes_order_and_independent_sequences() {
    let root = Root::new();
    let mut journal = root.open();
    assert_eq!(journal.append(upsert("a", 0)).unwrap().seq, 1);
    assert_eq!(journal.append(upsert("b", 0)).unwrap().seq, 2);
    let mut edited = item("a");
    edited.body = "edited".into();
    edited.state = State::Held;
    edited
        .metadata
        .insert("anchor".into(), json!({"question_id":"q"}));
    edited
        .metadata
        .insert("future_optional".into(), json!({"nested":[1,2]}));
    journal
        .append(Change::Upsert {
            item: Box::new(edited),
            position: 0,
        })
        .unwrap();
    journal
        .append(Change::Remove {
            item_id: "b".into(),
        })
        .unwrap();
    let expected = journal.items().to_vec();
    assert!(journal.bytes().unwrap() > 0);
    assert_eq!(
        fs::read_to_string(root.path()).unwrap().lines().count(),
        4 + usize::from(!root.identity_prefix().is_empty())
    );
    let other = Journal::open(&root.0, "other-fixture-agent").unwrap();
    assert_eq!(other.seq, 0);
    drop(journal);
    let journal = root.open();
    assert_eq!(journal.items(), expected);
    assert_eq!(journal.replay(0).unwrap().through, 4);
    assert!(journal.replay(5).is_err());
}

#[test]
fn failed_append_or_sync_never_folds_or_publishes_and_requires_reopen() {
    for fail_sync in [false, true] {
        let root = Root::new();
        let mut journal = root.open();
        journal.append(upsert("committed", 0)).unwrap();
        let peer = if fail_sync {
            let (file, peer) = socket_file();
            journal.file = file;
            Some(peer)
        } else {
            journal.file = File::open(root.path()).unwrap();
            None
        };
        assert!(journal.append(upsert("uncommitted", 1)).is_err());
        assert_eq!(journal.items(), &[item("committed")]);
        assert_eq!(journal.seq, 1);
        assert!(journal.replay(0).is_err());
        assert!(journal.bytes().is_err());
        assert!(journal.append(upsert("later", 1)).is_err());
        drop(peer);
        drop(journal);
        let reopened = root.open();
        assert_eq!(reopened.items(), &[item("committed")]);
        assert_eq!(reopened.seq, 1);
    }
    let root = Root::new();
    let mut journal = root.open();
    let bytes = encode(&Record {
        seq: 1,
        change: upsert("unacknowledged", 0),
    })
    .unwrap();
    let result: io::Result<()> = journal.durable(|this| {
        this.file.write_all(&bytes)?;
        Err(problem("interrupted before acknowledgement"))
    });
    assert!(result.is_err());
    assert!(journal.items().is_empty());
    assert_eq!(journal.seq, 0);
    drop(journal);
    // A complete but unacknowledged line may only be published after recovery syncs.
    let (file, mut peer) = socket_file();
    peer.write_all(&bytes).unwrap();
    peer.shutdown(Shutdown::Write).unwrap();
    let mut recovery = root.open();
    recovery.file = file;
    recovery.items.clear();
    recovery.records.clear();
    recovery.seq = 0;
    assert!(
        recovery.recover().is_err(),
        "recovery must refuse failed sync"
    );
    drop(recovery);
    let recovered = root.open();
    assert_eq!(recovered.seq, 1);
    assert_eq!(recovered.items(), &[item("unacknowledged")]);
}

#[test]
fn unknown_kind_is_skipped_but_its_cursor_and_opaque_replay_are_retained() {
    let root = Root::new();
    let mut journal = root.open();
    journal.append(upsert("known", 0)).unwrap();
    drop(journal);
    let mut file = private_fs::open_append_file(&root.path()).unwrap();
    file.write_all(b"{\"seq\":2,\"kind\":\"future_queue_fact\",\"data\":{\"x\":1}}\n")
        .unwrap();
    file.sync_all().unwrap();
    let mut journal = root.open();
    assert_eq!(journal.items(), &[item("known")]);
    assert_eq!(
        journal.replay(1).unwrap().records[0]["data"],
        json!({"x":1})
    );
    assert_eq!(journal.append(upsert("next", 1)).unwrap().seq, 3);
    assert!(
        journal.checkpoint().is_err(),
        "future state cannot be silently lost"
    );
    drop(journal);
    assert_eq!(root.open().seq, 3);
}

#[test]
fn torn_tail_is_repaired_before_the_next_append() {
    let root = Root::new();
    let mut journal = root.open();
    journal.append(upsert("a", 0)).unwrap();
    let length = journal.bytes().unwrap();
    journal
        .file
        .write_all(b"{\"seq\":2,\"kind\":\"upsert\"")
        .unwrap();
    drop(journal);
    let mut journal = root.open();
    assert_eq!(journal.bytes().unwrap(), length);
    assert_eq!(journal.append(upsert("b", 1)).unwrap().seq, 2);
    drop(journal);
    assert_eq!(root.open().items(), &[item("a"), item("b")]);
}

#[test]
fn complete_corruption_gaps_and_incomplete_checkpoints_open_read_only_without_repair() {
    for bytes in [
        "{\"seq\":2,\"kind\":\"future\"}\n",
        "{\"kind\":\"queue_checkpoint\",\"before\":4,\"high_water\":5}\n",
        "{\"kind\":\"queue_checkpoint\",\"before\":4,\"high_water\":5}\n{\"seq\":5",
        "{\"kind\":\"queue_checkpoint\",\"before\":5,\"high_water\":4}\n",
        "{\"seq\":1,\"kind\":\"future\"}\n{\"kind\":\"queue_checkpoint\",\"before\":1,\"high_water\":1}\n",
    ] {
        let root = Root::new();
        drop(root.open());
        let expected = root.write_records(bytes);
        assert!(matches!(root.open().read_only(), Some(ReadOnly::Corrupt(_))), "{bytes}");
        assert_eq!(fs::read(root.path()).unwrap(), expected);
    }
}

#[test]
fn checkpoints_publish_dense_snapshot_cuts_and_preserve_later_changes() {
    let root = Root::new();
    let mut journal = root.open();
    journal.append(upsert("a", 0)).unwrap();
    journal.append(upsert("b", 1)).unwrap();
    let historical = journal.replay(0).unwrap();
    let snapshot = journal.checkpoint().unwrap();
    assert_eq!(
        snapshot.gap,
        Some(Gap {
            from: 3,
            to: 2,
            snapshot_to: 4
        })
    );
    assert_eq!(
        snapshot
            .records
            .iter()
            .map(|v| v["seq"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [3, 4]
    );
    let disk = fs::read_to_string(root.path()).unwrap();
    let header: Value = disk
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|v| v["kind"] == "queue_checkpoint")
        .unwrap();
    assert_eq!(
        header,
        json!({"kind":"queue_checkpoint","before":2,"high_water":4})
    );
    journal
        .append(Change::Remove {
            item_id: "a".into(),
        })
        .unwrap();
    assert_eq!(historical.records.len(), 2);
    assert_eq!(historical.through, 2);
    assert_eq!(snapshot.records.len(), 2);
    assert_eq!(snapshot.through, 4);
    let cut = journal.replay(0).unwrap();
    assert_eq!(
        cut.gap,
        Some(Gap {
            from: 1,
            to: 2,
            snapshot_to: 4
        })
    );
    assert_eq!(cut.through, 5);
    assert_eq!(journal.replay(3).unwrap().records.len(), 2);
    drop(journal);
    let mut journal = root.open();
    assert_eq!(journal.items(), &[item("b")]);
    assert_eq!(journal.replay(0).unwrap().records, cut.records);
    let cut = journal.checkpoint().unwrap();
    assert_eq!(cut.gap.unwrap().snapshot_to, 6);
    assert_eq!(cut.records.len(), 1);
    drop(journal);
    assert_eq!(root.open().seq, 6);
}

#[test]
fn empty_checkpoint_high_water_survives_restart_and_new_input() {
    let root = Root::new();
    let mut journal = root.open();
    assert_eq!(
        journal.replay(0).unwrap().gap,
        Some(Gap {
            from: 1,
            to: 0,
            snapshot_to: 0
        })
    );
    journal.append(upsert("a", 0)).unwrap();
    journal
        .append(Change::Remove {
            item_id: "a".into(),
        })
        .unwrap();
    let cut = journal.checkpoint().unwrap();
    assert_eq!(
        cut.gap,
        Some(Gap {
            from: 3,
            to: 2,
            snapshot_to: 2
        })
    );
    assert!(cut.records.is_empty());
    assert_eq!(
        fs::read_to_string(root.path()).unwrap().lines().count(),
        1 + usize::from(!root.identity_prefix().is_empty())
    );
    drop(journal);
    let mut journal = root.open();
    assert!(journal.items().is_empty());
    assert_eq!(
        journal.replay(0).unwrap().gap,
        Some(Gap {
            from: 1,
            to: 2,
            snapshot_to: 2
        })
    );
    assert_eq!(journal.append(upsert("new", 0)).unwrap().seq, 3);
    drop(journal);
    assert_eq!(root.open().items(), &[item("new")]);
}

#[test]
fn failed_checkpoint_publication_never_advances_live_state_or_releases_lease() {
    for fail_directory_sync in [false, true] {
        let root = Root::new();
        let mut journal = root.open();
        journal.append(upsert("a", 0)).unwrap();
        let peer = if fail_directory_sync {
            let (file, peer) = socket_file();
            journal.directory = file;
            Some(peer)
        } else {
            let blocked = root.0.join("rename-target");
            fs::create_dir(&blocked).unwrap();
            journal.path = blocked;
            None
        };
        assert!(journal.checkpoint().is_err());
        assert_eq!(journal.seq, 1);
        assert_eq!(journal.items(), &[item("a")]);
        assert!(journal.replay(0).is_err());
        assert!(Journal::open(&root.0, "fixture-agent").is_err());
        drop(peer);
        drop(journal);
        let reopened = root.open();
        assert_eq!(reopened.items(), &[item("a")]);
        assert_eq!(reopened.seq, if fail_directory_sync { 2 } else { 1 });
    }
}

#[test]
fn writer_lease_survives_rename_then_releases_for_handoff() {
    let root = Root::new();
    let mut journal = root.open();
    for checkpoint in [false, true] {
        if checkpoint {
            journal.append(upsert("a", 0)).unwrap();
            journal.checkpoint().unwrap();
        }
        assert_eq!(
            Journal::open(&root.0, "fixture-agent")
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::WouldBlock
        );
    }
    drop(journal);
    let mut successor = root.open();
    assert_eq!(successor.append(upsert("b", 1)).unwrap().seq, 3);
}

#[test]
fn worker_publication_is_bounded_and_replay_cut_does_not_interleave() {
    let root = Root::new();
    let mut journal = root.open();
    let (published, ready) = mpsc::sync_channel(1);
    let (advance, step) = mpsc::sync_channel(1);
    let (finished, completed) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        journal.append(upsert("first", 0)).unwrap();
        published.send(journal.checkpoint().unwrap()).unwrap();
        step.recv_timeout(Duration::from_secs(3)).unwrap();
        journal.append(upsert("second", 1)).unwrap();
        finished.send(journal.checkpoint().unwrap()).unwrap();
    });
    let cut = ready
        .recv_timeout(Duration::from_secs(2))
        .expect("worker must publish a committed cut");
    assert_eq!(cut.through, 2);
    advance.send(()).unwrap();
    let later = completed
        .recv_timeout(Duration::from_secs(2))
        .expect("worker must publish its next cut");
    worker.join().unwrap();
    assert_eq!(later.through, 5);
    assert_eq!(cut.records.len(), 1);
    assert_eq!(cut.records[0]["item"]["item_id"], "first");
    assert_eq!(root.open().items(), &[item("first"), item("second")]);
}

#[test]
fn validation_limits_do_not_spend_sequences_or_poison_the_writer() {
    let root = Root::new();
    let mut journal = root.open();
    assert!(journal.append(upsert("bad-position", 1)).is_err());
    let mut oversized = item("large");
    oversized.body = "x".repeat(LINE_LIMIT as usize);
    assert!(journal
        .append(Change::Upsert {
            item: Box::new(oversized),
            position: 0
        })
        .is_err());
    let mut collision = item("collision");
    collision.metadata.insert("item_id".into(), json!("other"));
    assert!(journal
        .append(Change::Upsert {
            item: Box::new(collision),
            position: 0
        })
        .is_err());
    assert_eq!(
        journal.bytes().unwrap(),
        root.identity_prefix().len() as u64
    );
    assert_eq!(journal.append(upsert("valid", 0)).unwrap().seq, 1);
    for agent in ["", "..", "a/b", "a.b", "a\\b"] {
        assert!(Journal::open(&root.0, agent).is_err());
    }
}

#[test]
fn exhausted_sequences_never_wrap() {
    let root = Root::new();
    drop(root.open());
    root.write_records(format!(
        "{{\"kind\":\"queue_checkpoint\",\"before\":{0},\"high_water\":{0}}}\n",
        u64::MAX
    ));
    let mut journal = root.open();
    assert!(journal.append(upsert("no-wrap", 0)).is_err());
    assert_eq!(journal.seq, u64::MAX);
    assert!(journal.replay(u64::MAX).is_err());
    let bytes = fs::read(root.path()).unwrap();
    assert!(journal.checkpoint().is_err());
    assert_eq!(fs::read(root.path()).unwrap(), bytes);
}

#[test]
fn unrenamed_checkpoint_is_ignored_and_a_later_torn_append_is_repaired() {
    let root = Root::new();
    let mut journal = root.open();
    journal.append(upsert("committed", 0)).unwrap();
    fs::write(
        root.path().with_file_name(".queue-interrupted.next"),
        b"{\"kind\":\"queue_checkpoint\",\"before\":99,\"high_water\":99}\n",
    )
    .unwrap();
    drop(journal);
    let mut journal = root.open();
    assert!(!root
        .path()
        .with_file_name(".queue-interrupted.next")
        .exists());
    assert_eq!(journal.seq, 1);
    assert_eq!(journal.items(), &[item("committed")]);
    journal.checkpoint().unwrap();
    let good = journal.bytes().unwrap();
    journal.file.write_all(b"{\"seq\":3").unwrap();
    drop(journal);
    let mut journal = root.open();
    assert_eq!(journal.bytes().unwrap(), good);
    assert_eq!(journal.seq, 2);
    assert_eq!(journal.append(upsert("next", 1)).unwrap().seq, 3);
}

#[test]
fn oversized_recovery_line_is_refused_without_truncation() {
    let root = Root::new();
    drop(root.open());
    let oversized = vec![b'x'; LINE_LIMIT as usize + 1];
    fs::write(root.path(), &oversized).unwrap();
    assert!(Journal::open(&root.0, "fixture-agent").is_err());
    assert_eq!(fs::read(root.path()).unwrap(), oversized);
}

#[test]
fn writer_lease_excludes_another_process_across_checkpoint_rename() {
    let root = Root::new();
    let mut journal = root.open();
    let probe = || {
        let mut command = crate::process::TracedCommand::new(
            std::env::current_exe().unwrap(),
            "queue-lease-probe",
        );
        command.args([
            "--exact",
            "structured::queue::tests::lease_probe_child",
            "--nocapture",
        ]);
        command.env(
            "QUEUE_LEASE_PROBE",
            root.path().with_file_name(".queue.lock"),
        );
        command
            .output_traced_with_timeout(Duration::from_secs(2))
            .unwrap()
    };
    assert_eq!(probe().status.code(), Some(42));
    journal.append(upsert("a", 0)).unwrap();
    journal.checkpoint().unwrap();
    assert_eq!(probe().status.code(), Some(42));
    drop(journal);
    assert!(probe().status.success());
}

#[test]
fn lease_probe_child() {
    let Some(path) = std::env::var_os("QUEUE_LEASE_PROBE") else {
        return;
    };
    let file = private_fs::open_append_file(Path::new(&path)).unwrap();
    match file.try_lock() {
        Ok(()) => (),
        Err(std::fs::TryLockError::WouldBlock) => std::process::exit(42),
        Err(error) => panic!("unexpected lease failure: {error}"),
    }
}

#[test]
fn mixed_case_identity_round_trips_with_its_original_spelling() {
    let root = Root::new();
    let agent = "aGent-MiXeD_123";
    let mut journal = Journal::open(&root.0, agent).unwrap();
    journal.append(upsert("original", 0)).unwrap();
    let path = journal.path.clone();
    let identity: Value =
        serde_json::from_str(fs::read_to_string(&path).unwrap().lines().next().unwrap()).unwrap();
    assert_eq!(
        identity,
        json!({"kind":"queue_identity", "agent_id":agent, "format_version":FORMAT_VERSION})
    );
    journal.checkpoint().unwrap();
    drop(journal);
    let journal = Journal::open(&root.0, agent).unwrap();
    assert_eq!(journal.path, path);
    assert_eq!(journal.items(), &[item("original")]);
    assert_eq!(journal.seq, 2);
}

#[test]
fn case_distinct_ids_have_distinct_journals_and_leases() {
    let root = Root::new();
    let mut upper = Journal::open(&root.0, "Foo").unwrap();
    let mut lower = Journal::open(&root.0, "foo").unwrap();
    upper.append(upsert("upper", 0)).unwrap();
    lower.append(upsert("lower", 0)).unwrap();
    assert_ne!(upper.path, lower.path);
    assert_eq!(upper.path.parent().unwrap().file_name().unwrap(), "=foo");
    assert_eq!(lower.path.parent().unwrap().file_name().unwrap(), "foo");
    drop((upper, lower));
    assert_eq!(
        Journal::open(&root.0, "Foo").unwrap().items(),
        &[item("upper")]
    );
    assert_eq!(
        Journal::open(&root.0, "foo").unwrap().items(),
        &[item("lower")]
    );
    for invalid in ["=foo", "a=b", "Fo/o", "Fo.o"] {
        assert!(Journal::open(&root.0, invalid).is_err());
    }
}

#[test]
fn mismatched_original_journal_identity_is_refused() {
    let root = Root::new();
    let mut journal = root.open();
    journal.append(upsert("original", 0)).unwrap();
    let bytes = fs::read_to_string(root.path()).unwrap();
    let mut lines = bytes.lines();
    let mut identity: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(identity["kind"], "queue_identity");
    identity["agent_id"] = json!("another-agent");
    let mut changed = encode(&identity).unwrap();
    for line in lines {
        changed.extend_from_slice(line.as_bytes());
        changed.push(b'\n');
    }
    drop(journal);
    fs::write(root.path(), &changed).unwrap();
    assert!(Journal::open(&root.0, "fixture-agent").is_err());
    assert_eq!(fs::read(root.path()).unwrap(), changed);
}

#[test]
fn unknown_enum_items_keep_positions_through_later_updates_and_removals() {
    for remove in [false, true] {
        let root = Root::new();
        drop(root.open());
        let mut a = serde_json::to_value(Record {
            seq: 1,
            change: upsert("a", 0),
        })
        .unwrap();
        a["item"]["state"] = json!("future_state");
        let b = serde_json::to_value(Record {
            seq: 2,
            change: upsert("b", 1),
        })
        .unwrap();
        let mut records = encode(&a).unwrap();
        records.extend(encode(&b).unwrap());
        root.write_records(&records);
        let mut journal = root.open();
        assert_eq!(
            serde_json::to_value(&journal.items()[0]).unwrap(),
            a["item"]
        );
        assert_eq!(journal.items()[1], item("b"));
        assert!(journal.read_only().is_none());
        journal
            .append(if remove {
                Change::Remove {
                    item_id: "a".into(),
                }
            } else {
                upsert("a", 0)
            })
            .unwrap();
        assert_eq!(journal.seq, 3);
        let expected = if remove {
            vec![item("b")]
        } else {
            vec![item("a"), item("b")]
        };
        assert_eq!(journal.items(), expected);
        drop(journal);
        let mut journal = root.open();
        assert_eq!(journal.items(), expected);
        assert_eq!(journal.replay(0).unwrap().records[0], a);
        assert!(
            journal.checkpoint().is_err(),
            "opaque history must remain preserved"
        );
    }
}

#[test]
fn unknown_enum_update_replaces_a_stale_older_item() {
    let root = Root::new();
    let mut journal = root.open();
    journal.append(upsert("a", 0)).unwrap();
    drop(journal);
    let mut a = serde_json::to_value(Record {
        seq: 2,
        change: upsert("a", 0),
    })
    .unwrap();
    a["item"]["state"] = json!({"future_state":{"x":1}});
    a["item"]["body"] = json!("new body, not the stale version");
    let mut file = private_fs::open_append_file(&root.path()).unwrap();
    file.write_all(&encode(&a).unwrap()).unwrap();
    file.sync_all().unwrap();
    let mut journal = root.open();
    assert_eq!(
        serde_json::to_value(&journal.items()[0]).unwrap(),
        a["item"]
    );
    assert_eq!(journal.seq, 2);
    assert!(journal.checkpoint().is_err());
}

#[test]
fn future_record_fields_remain_opaque_and_block_checkpoint() {
    assert_future_data_preserved(&["upsert", "remove"]);
}

#[test]
fn future_enum_values_remain_opaque_and_block_checkpoint() {
    assert_future_data_preserved(&["state", "producer", "delivery", "kind", "priority"]);
}

fn assert_future_data_preserved(extensions: &[&str]) {
    for &extension in extensions {
        let root = Root::new();
        drop(root.open());
        let mut value = serde_json::to_value(Record {
            seq: 1,
            change: upsert("future", 0),
        })
        .unwrap();
        match extension {
            "upsert" => {
                value["future_field"] = json!({"x":1});
            }
            "remove" => {
                value = json!({"seq":1,"kind":"remove","item_id":"future","future_field":[1,2]});
            }
            field => {
                value["item"][field] = json!("future_variant");
            }
        }
        root.write_records(encode(&value).unwrap());
        let mut journal = root.open();
        assert_eq!(journal.replay(0).unwrap().records, [value.clone()]);
        if value["kind"] == "upsert" {
            assert_eq!(
                serde_json::to_value(&journal.items()[0]).unwrap(),
                value["item"]
            );
        }
        assert!(journal.checkpoint().is_err(), "cannot discard {extension}");
        if !matches!(extension, "upsert" | "remove") {
            let typed = serde_json::from_value::<Item>(value["item"].clone()).unwrap();
            let fresh = Root::new();
            let mut appended = fresh.open();
            appended
                .append(Change::Upsert {
                    item: Box::new(typed),
                    position: 0,
                })
                .unwrap();
            assert!(
                appended.checkpoint().is_err(),
                "typed unknown values also block rewriting"
            );
            drop(appended);
            assert_eq!(
                serde_json::to_value(&fresh.open().items()[0]).unwrap(),
                value["item"]
            );
        }
        drop(journal);
        assert_eq!(root.open().replay(0).unwrap().records, [value]);
    }
}

#[test]
fn unverified_identity_is_refused_and_interior_identity_is_read_only() {
    for bytes in [
        b"{\"seq\":1,\"kind\":\"future\"}\n".as_slice(),
        b"{\"kind\":\"queue_identity\",\"agent_id\":\"fixture-agent\"}\n{\"kind\":\"queue_identity\",\"agent_id\":\"fixture-agent\"}\n".as_slice(),
    ] {
        let root=Root::new();
        drop(root.open());
        fs::write(root.path(),bytes).unwrap();
        if bytes.split(|b| *b == b'\n').filter(|line| !line.is_empty()).count() == 1 {
            assert!(Journal::open(&root.0,"fixture-agent").is_err());
        } else { assert!(matches!(root.open().read_only(), Some(ReadOnly::Corrupt(_)))); }
        assert_eq!(fs::read(root.path()).unwrap(),bytes);
    }
}

#[test]
fn torn_first_identity_is_rewritten_without_losing_records() {
    for torn in [
        b"{".as_slice(),
        b"{\"kind\":\"queue_identity\",\"agent_id\":\"fi".as_slice(),
    ] {
        let root = Root::new();
        drop(root.open());
        fs::write(root.path(), torn).unwrap();
        let mut journal = root.open();
        assert!(journal.items().is_empty());
        assert_eq!(journal.replay(0).unwrap().through, 0);
        assert_eq!(
            fs::read(root.path()).unwrap(),
            encode(&journal.identity(journal.version)).unwrap()
        );
        assert_eq!(journal.append(upsert("first", 0)).unwrap().seq, 1);
        drop(journal);
        assert_eq!(root.open().items()[0].item_id, "first");
    }
}

fn assert_corrupt_item_read_only(field: &str, malformed: Value) {
    let root = Root::new();
    drop(root.open());
    let mut record = serde_json::to_value(Record {
        seq: 1,
        change: upsert("bad", 0),
    })
    .unwrap();
    record["item"][field] = malformed;
    let original = root.write_records(encode(&record).unwrap());
    assert!(serde_json::from_value::<Record>(record).is_err());
    let mut journal = root.open();
    assert!(journal.items().is_empty());
    assert!(matches!(journal.read_only(), Some(ReadOnly::Corrupt(_))));
    assert_eq!(journal.replay(0).unwrap().records.len(), 0);
    assert!(journal.checkpoint().is_err());
    assert_eq!(fs::read(root.path()).unwrap(), original);
}

#[test]
fn known_system_tag_with_bad_payload_is_corrupt_not_unknown() {
    assert_corrupt_item_read_only("producer", json!({"system":42}));
}

#[test]
fn known_channel_tag_without_user_is_corrupt_not_unknown() {
    assert_corrupt_item_read_only("producer", json!({"channel":{"binding":"x"}}));
}

#[test]
fn state_without_a_variant_name_is_corrupt_not_unknown() {
    assert_corrupt_item_read_only("state", json!(7));
}

#[test]
fn known_variant_names_never_fall_back_to_unknown() {
    for field in ["state", "kind", "priority", "delivery"] {
        let value = serde_json::to_value(item("known")).unwrap();
        let tag = value[field].as_str().unwrap();
        assert_corrupt_item_read_only(field, json!({tag:42}));
        assert_corrupt_item_read_only(field, json!(7));
    }
    for malformed in [json!("agent"), json!({"human":42}), json!({"fork":false})] {
        assert_corrupt_item_read_only("producer", malformed);
    }
}

#[test]
fn unknown_producer_tag_preserves_its_uninterpreted_payload() {
    let root = Root::new();
    drop(root.open());
    let mut record = serde_json::to_value(Record {
        seq: 1,
        change: upsert("future", 0),
    })
    .unwrap();
    let raw = json!({"future_source":{"arbitrary":42}});
    record["item"]["producer"] = raw.clone();
    root.write_records(encode(&record).unwrap());
    let mut journal = root.open();
    assert_eq!(journal.items()[0].producer, Producer::Unknown(raw));
    assert_eq!(journal.replay(0).unwrap().records, [record]);
    assert!(journal.checkpoint().is_err());
}

mod admission;

// Replace only this test-owned journal handle; its real backing file stays intact.
pub(crate) fn fail_writes(journal: &mut Journal) -> UnixStream {
    let (file, peer) = socket_file();
    journal.file = file;
    peer
}

#[test]
fn newer_stamp_loads_but_refuses_mutations_without_changing_bytes() {
    let root = Root::new();
    drop(root.open());
    let mut identity: Value = serde_json::from_slice(&root.identity_prefix()).unwrap();
    identity["format_version"] = json!(999);
    let mut bytes = encode(&identity).unwrap();
    bytes.extend(b"{\"seq\":1,\"kind\":\"future_queue_fact\"}\n");
    bytes.extend(b"{\"seq\":2,\"kind\":\"upsert\",\"item\":42}\n");
    bytes.extend(b"torn future tail");
    fs::write(root.path(), &bytes).unwrap();
    let mut journal = root.open();
    let append = journal.append(upsert("a", 0)).unwrap_err();
    assert!(append.to_string().contains("queue_journal_newer"));
    let checkpoint = journal.checkpoint().unwrap_err();
    assert!(checkpoint.to_string().contains("queue_journal_newer"));
    assert_eq!(fs::read(root.path()).unwrap(), bytes);
}

#[test]
fn unstamped_v1_gains_a_version_stamp_on_checkpoint() {
    let root = Root::new();
    drop(root.open());
    fs::write(
        root.path(),
        b"{\"kind\":\"queue_identity\",\"agent_id\":\"fixture-agent\"}\n",
    )
    .unwrap();
    let before = fs::read(root.path()).unwrap();
    let mut journal = root.open();
    assert_eq!(journal.format_version(), 1);
    assert_eq!(fs::read(root.path()).unwrap(), before);
    journal.checkpoint().unwrap();
    let header: Value = serde_json::from_slice(&root.identity_prefix()).unwrap();
    assert_eq!(header["format_version"], 2);
}

#[test]
fn same_version_invalid_record_is_read_only_and_preserved() {
    let root = Root::new();
    drop(root.open());
    let bytes = root.write_records(b"{\"seq\":1,\"kind\":\"upsert\",\"item\":42}\n{\"seq\":2,\"kind\":\"future_queue_fact\"}\n");
    let mut journal = root.open();
    assert!(journal.items().is_empty());
    assert_eq!(journal.replay(0).unwrap().records.len(), 0);
    assert!(matches!(journal.read_only(), Some(ReadOnly::Corrupt(_))));
    assert!(journal.checkpoint().is_err());
    assert_eq!(fs::read(root.path()).unwrap(), bytes);
}

#[test]
fn old_format_is_upgraded_before_any_new_append() {
    let root = Root::new();
    let mut journal = root.open();
    journal.append(upsert("a", 0)).unwrap();
    drop(journal);
    let old = fs::read(root.path()).unwrap();
    let mut legacy = encode(&json!({"kind":"queue_identity", "agent_id":"fixture-agent"})).unwrap();
    legacy.extend(old.split_inclusive(|b| *b == b'\n').skip(1).flatten());
    fs::write(root.path(), &legacy).unwrap();
    let mut journal = root.open();
    assert_eq!(journal.format_version(), 1);
    assert!(journal.append(upsert("invalid", 5)).is_err());
    assert_eq!(fs::read(root.path()).unwrap(), legacy);
    assert_eq!(journal.append(upsert("b", 1)).unwrap().seq, 3);
    assert_eq!(journal.format_version(), FORMAT_VERSION);
    drop(journal);
    let journal = root.open();
    assert_eq!(journal.items(), &[item("a"), item("b")]);
    assert_eq!(journal.format_version(), FORMAT_VERSION);
}

#[test]
fn corruption_stops_folding_without_partial_updates_or_repair() {
    let root = Root::new();
    drop(root.open());
    let mut records = encode(&Record {
        seq: 1,
        change: upsert("a", 0),
    })
    .unwrap();
    let mut changed = item("a");
    changed.body = "must not leak from a rejected batch".into();
    records.extend(
        encode(&Record {
            seq: 2,
            change: Change::Batch {
                items: vec![changed, item("missing")],
            },
        })
        .unwrap(),
    );
    records.extend(
        encode(&Record {
            seq: 3,
            change: upsert("a", 5),
        })
        .unwrap(),
    );
    records.extend(b"invalid JSON record\n");
    records.extend(
        encode(&Record {
            seq: 5,
            change: upsert("b", 1),
        })
        .unwrap(),
    );
    let bytes = root.write_records(records);
    let mut journal = root.open();
    assert_eq!(journal.items(), &[item("a")]);
    assert_eq!(journal.replay(0).unwrap().through, 1);
    assert!(matches!(
        journal.read_only(),
        Some(ReadOnly::Corrupt(QueueJournalCorrupt { seq: Some(2), .. }))
    ));
    assert!(journal.checkpoint().is_err());
    assert_eq!(fs::read(root.path()).unwrap(), bytes);
}

#[test]
fn newer_format_records_are_not_subject_to_older_framing_or_repair() {
    let root = Root::new();
    drop(root.open());
    let mut bytes =
        encode(&json!({"kind":"queue_identity", "agent_id":"fixture-agent", "format_version":999}))
            .unwrap();
    bytes.extend(b"{\"kind\":\"queue_checkpoint\",\"new_snapshot\":true}\n");
    bytes.extend(b"{\"kind\":\"future\",\"seq\":20}\n");
    bytes.extend(b"{\"kind\":\"queue_checkpoint\",\"before\":20,\"high_water\":19}\n");
    bytes.extend(b"{\"kind\":\"future_unsequenced\"}\n");
    bytes.extend(vec![b'x'; LINE_LIMIT as usize + 100]);
    bytes.extend(b"\ninvalid JSON\ntorn tail");
    fs::write(root.path(), &bytes).unwrap();
    let temporary = root.path().with_file_name(".queue-future.next");
    fs::write(&temporary, b"future pending replacement").unwrap();
    let mut journal = root.open();
    assert_eq!(
        journal.read_only(),
        Some(ReadOnly::Newer(QueueJournalNewer {
            found: 999,
            supported: FORMAT_VERSION
        }))
    );
    assert_eq!(journal.replay(0).unwrap().through, 20);
    assert!(journal.replay(0).unwrap().gap.is_none());
    assert!(journal.checkpoint().is_err());
    assert_eq!(fs::read(root.path()).unwrap(), bytes);
    assert_eq!(fs::read(temporary).unwrap(), b"future pending replacement");
}

#[test]
fn legacy_upgrade_sequence_exhaustion_refuses_before_any_checkpoint_write() {
    let root = Root::new();
    drop(root.open());
    let mut bytes = encode(&json!({"kind":"queue_identity", "agent_id":"fixture-agent"})).unwrap();
    bytes.extend(
        encode(&json!({"kind":"queue_checkpoint", "before":u64::MAX-2, "high_water":u64::MAX-1}))
            .unwrap(),
    );
    bytes.extend(
        encode(&Record {
            seq: u64::MAX - 1,
            change: upsert("a", 0),
        })
        .unwrap(),
    );
    fs::write(root.path(), &bytes).unwrap();
    let mut journal = root.open();
    assert!(journal.append(upsert("b", 1)).is_err());
    assert_eq!(journal.replay(u64::MAX - 1).unwrap().through, u64::MAX - 1);
    assert_eq!(journal.format_version(), 1);
    assert_eq!(fs::read(root.path()).unwrap(), bytes);
}

#[test]
fn legacy_upgrade_encoded_size_refuses_before_any_checkpoint_write() {
    let root = Root::new();
    drop(root.open());
    let mut bytes = encode(&json!({"kind":"queue_identity", "agent_id":"fixture-agent"})).unwrap();
    bytes.extend(encode(&json!({"kind":"queue_checkpoint", "before":7, "high_water":8})).unwrap());
    bytes.extend(
        encode(&Record {
            seq: 8,
            change: upsert("a", 0),
        })
        .unwrap(),
    );
    fs::write(root.path(), &bytes).unwrap();
    let mut b = item("b");
    b.body.clear();
    let base = encode(&Record {
        seq: 9,
        change: Change::Upsert {
            item: Box::new(b.clone()),
            position: 1,
        },
    })
    .unwrap()
    .len();
    b.body = "x".repeat(LINE_LIMIT as usize - base);
    let mut journal = root.open();
    assert!(journal
        .append(Change::Upsert {
            item: Box::new(b),
            position: 1
        })
        .is_err());
    assert_eq!(journal.replay(8).unwrap().through, 8);
    assert_eq!(journal.format_version(), 1);
    assert_eq!(fs::read(root.path()).unwrap(), bytes);
}

#[test]
fn malformed_known_record_opens_read_only_and_refuses_every_storage_mutation() {
    let root = Root::new();
    let mut journal = root.open();
    journal.append(upsert("a", 0)).unwrap();
    drop(journal);
    let bytes = root.write_records(
        b"{\"seq\":1,\"kind\":\"upsert\",\"item\":42}\n{\"seq\":2,\"kind\":\"future\"}\ntorn tail",
    );
    let mut opened = None;
    let logs = crate::logging::capture_logs(|| {
        opened = Some(root.open());
    });
    assert!(logs.contains("structured.queue.corrupt"));
    let mut journal = opened.unwrap();
    let expected = QueueJournalCorrupt {
        seq: Some(1),
        offset: root.identity_prefix().len() as u64,
        reason: "malformed_known_record",
    };
    assert_eq!(journal.read_only(), Some(ReadOnly::Corrupt(expected)));
    let changes = [
        upsert("b", 0),
        Change::Remove {
            item_id: "a".into(),
        },
        Change::Batch { items: vec![] },
        Change::Fail {
            item_id: "a".into(),
            reason: "invalid_request".into(),
        },
    ];
    for change in changes {
        let error = journal.append(change).unwrap_err();
        assert_eq!(
            error
                .get_ref()
                .unwrap()
                .downcast_ref::<QueueJournalCorrupt>(),
            Some(&expected)
        );
    }
    assert!(journal
        .checkpoint()
        .unwrap_err()
        .to_string()
        .contains("queue_journal_corrupt"));
    assert_eq!(fs::read(root.path()).unwrap(), bytes);
}

#[test]
fn invalid_header_versions_open_read_only_and_preserve_all_bytes() {
    for stamp in ["0", "\"2\"", "null", "-1", "2.5", ""] {
        let root = Root::new();
        drop(root.open());
        let mut bytes = format!(
            "{{\"kind\":\"queue_identity\",\"agent_id\":\"fixture-agent\",\"format_version\":{stamp}}}\n"
        ).into_bytes();
        bytes.extend(
            encode(&Record {
                seq: 1,
                change: upsert("later", 0),
            })
            .unwrap(),
        );
        bytes.extend(b"torn tail");
        fs::write(root.path(), &bytes).unwrap();
        let mut journal = root.open();
        let reason = match journal.read_only() {
            Some(ReadOnly::Corrupt(reason)) => reason,
            other => panic!("expected corrupt header, got {other:?}: {stamp}"),
        };
        assert_eq!(reason.offset, 0);
        assert_eq!(reason.seq, None);
        assert_eq!(
            reason.reason,
            if stamp.is_empty() {
                "invalid_json"
            } else {
                "invalid_format_version"
            }
        );
        assert!(journal.items().is_empty());
        assert_all_storage_mutations_refuse(&mut journal, reason);
        assert_eq!(fs::read(root.path()).unwrap(), bytes);
    }
}

fn assert_all_storage_mutations_refuse(journal: &mut Journal, reason: QueueJournalCorrupt) {
    for change in [
        upsert("new", 0),
        Change::Remove {
            item_id: "a".into(),
        },
        Change::Batch { items: vec![] },
        Change::Fail {
            item_id: "a".into(),
            reason: "invalid_request".into(),
        },
    ] {
        let error = journal.append(change).unwrap_err();
        assert_eq!(
            error
                .get_ref()
                .unwrap()
                .downcast_ref::<QueueJournalCorrupt>(),
            Some(&reason)
        );
    }
    let error = journal.checkpoint().unwrap_err();
    assert_eq!(
        error
            .get_ref()
            .unwrap()
            .downcast_ref::<QueueJournalCorrupt>(),
        Some(&reason)
    );
}

#[test]
fn legacy_unknown_data_open_read_only_before_upgrade_is_attempted() {
    for extension in ["kind", "field", "enum"] {
        let root = Root::new();
        drop(root.open());
        let mut bytes =
            encode(&json!({"kind":"queue_identity","agent_id":"fixture-agent"})).unwrap();
        bytes.extend(unknown_record(extension));
        fs::write(root.path(), &bytes).unwrap();
        let mut journal = root.open();
        assert_eq!(journal.format_version(), 1);
        let reason = match journal.read_only() {
            Some(ReadOnly::Corrupt(reason)) => reason,
            other => panic!("expected unknown-data refusal, got {other:?}"),
        };
        assert_eq!(reason.reason, "unknown_data_prevents_checkpoint");
        assert_all_storage_mutations_refuse(&mut journal, reason);
        assert_eq!(fs::read(root.path()).unwrap(), bytes);
    }
}

fn unknown_record(extension: &str) -> Vec<u8> {
    let mut record = serde_json::to_value(Record {
        seq: 1,
        change: upsert("a", 0),
    })
    .unwrap();
    match extension {
        "kind" => record = json!({"kind":"future","seq":1}),
        "field" => record["future"] = json!(true),
        "enum" => record["item"]["state"] = json!("future"),
        _ => panic!("unexpected fixture extension"),
    }
    encode(&record).unwrap()
}

#[test]
fn required_checkpoint_latches_unknown_data_read_only_before_any_more_writes() {
    for extension in ["kind", "field", "enum"] {
        let root = Root::new();
        drop(root.open());
        root.write_records(unknown_record(extension));
        let mut journal = root.open();
        assert!(journal.read_only().is_none());
        journal
            .append(upsert("known", journal.items().len()))
            .unwrap();
        let bytes = fs::read(root.path()).unwrap();
        let error = journal.checkpoint().unwrap_err();
        let reason = *error
            .get_ref()
            .unwrap()
            .downcast_ref::<QueueJournalCorrupt>()
            .expect("checkpoint refusal must be typed");
        assert_eq!(reason.reason, "unknown_data_prevents_checkpoint");
        assert_eq!(journal.read_only(), Some(ReadOnly::Corrupt(reason)));
        assert_all_storage_mutations_refuse(&mut journal, reason);
        assert_eq!(fs::read(root.path()).unwrap(), bytes);
    }
}

#[test]
fn first_invalid_json_is_the_reported_corruption_point() {
    assert_first_corruption(b"garbage\n", "invalid_json", None);
}

#[test]
fn first_missing_kind_is_the_reported_corruption_point() {
    assert_first_corruption(b"{\"seq\":1}\n", "missing_kind", Some(1));
}

#[test]
fn first_malformed_upsert_is_the_reported_corruption_point() {
    assert_first_corruption(
        b"{\"seq\":1,\"kind\":\"upsert\"}\n",
        "malformed_known_record",
        Some(1),
    );
}

fn assert_first_corruption(first: &[u8], reason: &'static str, seq: Option<u64>) {
    let root = Root::new();
    drop(root.open());
    let offset = root.identity_prefix().len() as u64;
    let mut records = first.to_vec();
    records.extend(b"{\"seq\":2,\"kind\":\"upsert\",\"item\":42}\ntorn tail");
    let bytes = root.write_records(records);
    let mut journal = root.open();
    let expected = QueueJournalCorrupt {
        seq,
        offset,
        reason,
    };
    assert_eq!(journal.read_only(), Some(ReadOnly::Corrupt(expected)));
    assert!(journal.items().is_empty());
    assert_eq!(journal.replay(0).unwrap().through, 0);
    assert_all_storage_mutations_refuse(&mut journal, expected);
    assert_eq!(fs::read(root.path()).unwrap(), bytes);
}
