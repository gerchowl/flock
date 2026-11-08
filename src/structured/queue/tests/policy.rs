use super::*;
use std::num::NonZeroU64;

fn policy(bytes: u64) -> RewritePolicy {
    RewritePolicy::new(NonZeroU64::new(bytes).unwrap())
}
fn open(root: &Root, bytes: u64) -> Journal {
    Journal::open_with_policy(&root.0, "fixture-agent", policy(bytes)).unwrap()
}

#[test]
fn checkpoint_policy_uses_strict_configured_byte_threshold_and_returns_owned_cut() {
    let root = Root::new();
    let mut j = root.open();
    j.append(upsert("a", 0)).unwrap();
    let size = j.bytes().unwrap();
    drop(j);
    for threshold in [size, size + 1] {
        let mut j = open(&root, threshold);
        assert!(j.checkpoint_if_required().unwrap().is_none());
        assert_eq!(j.seq, 1);
    }
    let mut j = open(&root, size - 1);
    let cut = j.checkpoint_if_required().unwrap().unwrap();
    assert_eq!(
        cut.gap,
        Some(Gap {
            from: 2,
            to: 1,
            snapshot_to: 2
        })
    );
    assert_eq!(cut.through, 2);
    assert_eq!(cut.records.len(), 1);
    assert_eq!(cut.records[0]["item"]["item_id"], "a");
    j.append(upsert("b", 1)).unwrap();
    assert_eq!(cut.records.len(), 1, "publication owns an immutable cut");
    assert_eq!(cut.through, 2);
}

#[test]
fn checkpoint_policy_runs_before_append_and_includes_snapshot_in_replay() {
    let root = Root::new();
    let mut j = root.open();
    j.append(upsert("a", 0)).unwrap();
    let size = j.bytes().unwrap();
    drop(j);
    let mut j = open(&root, size - 1);
    let record = j.append(upsert("b", 1)).unwrap();
    assert_eq!(record.seq, 3);
    let cut = j.replay(1).unwrap();
    assert_eq!(
        cut.gap,
        Some(Gap {
            from: 2,
            to: 1,
            snapshot_to: 2
        })
    );
    assert_eq!(cut.through, 3);
    assert_eq!(cut.records.len(), 2);
}

#[test]
fn checkpoint_policy_empty_snapshot_keeps_high_water_across_reopen() {
    let root = Root::new();
    let mut j = root.open();
    j.append(upsert("a", 0)).unwrap();
    j.append(Change::Remove {
        item_id: "a".into(),
    })
    .unwrap();
    let size = j.bytes().unwrap();
    drop(j);
    let mut j = open(&root, size - 1);
    let cut = j.checkpoint_if_required().unwrap().unwrap();
    assert_eq!(
        cut.gap,
        Some(Gap {
            from: 3,
            to: 2,
            snapshot_to: 2
        })
    );
    assert!(cut.records.is_empty());
    drop(j);
    let mut j = open(&root, size - 1);
    assert_eq!(j.append(upsert("b", 0)).unwrap().seq, 3);
}

fn unknown(root: &Root) -> Vec<u8> {
    drop(root.open());
    root.write_records(b"{\"seq\":1,\"kind\":\"future\"}\n")
}
fn assert_refusal(j: &mut Journal, bytes: &[u8], root: &Root) {
    let reason = j.read_only().unwrap();
    assert!(matches!(
        reason,
        ReadOnly::Corrupt(QueueJournalCorrupt {
            reason: "unknown_data_prevents_checkpoint",
            ..
        })
    ));
    for error in [
        j.append(upsert("a", 0)).unwrap_err(),
        j.checkpoint_if_required().unwrap_err(),
        j.checkpoint().unwrap_err(),
    ] {
        assert_eq!(
            error
                .get_ref()
                .unwrap()
                .downcast_ref::<QueueJournalCorrupt>(),
            match &reason {
                ReadOnly::Corrupt(r) => Some(r),
                _ => None,
            }
        );
    }
    assert_eq!(fs::read(root.path()).unwrap(), bytes);
}

#[test]
fn checkpoint_policy_unknown_data_rederives_readonly_on_reopen_without_tail_repair() {
    for tail in [b"".as_slice(), b"{torn"] {
        let root = Root::new();
        let mut bytes = unknown(&root);
        bytes.extend(tail);
        fs::write(root.path(), &bytes).unwrap();
        for _ in 0..2 {
            let mut j = open(&root, bytes.len() as u64 - 1);
            assert_refusal(&mut j, &bytes, &root);
        }
    }
}

#[test]
fn checkpoint_policy_unknown_data_latches_when_append_crosses_threshold() {
    let root = Root::new();
    let bytes = unknown(&root);
    let mut j = open(&root, bytes.len() as u64);
    assert!(j.read_only().is_none());
    assert!(j.checkpoint_if_required().unwrap().is_none());
    j.append(upsert("a", 0)).unwrap();
    let bytes = fs::read(root.path()).unwrap();
    assert!(j.append(upsert("b", 1)).is_err());
    assert_refusal(&mut j, &bytes, &root);
}

#[test]
fn checkpoint_policy_keeps_newer_and_corrupt_journal_bytes_unchanged() {
    for (newer, records) in [
        (false, b"{\"seq\":1,\"kind\":\"upsert\"}\n".as_slice()),
        (true, b"{\"seq\":1,\"kind\":\"future\"}\n".as_slice()),
    ] {
        let root = Root::new();
        drop(root.open());
        let bytes = if newer {
            let header = json!({"kind":"queue_identity","agent_id":"fixture-agent","format_version":FORMAT_VERSION+1});
            let mut bytes = encode(&header).unwrap();
            bytes.extend(records);
            bytes.extend(b"{torn");
            fs::write(root.path(), &bytes).unwrap();
            bytes
        } else {
            root.write_records(records)
        };
        let mut j = open(&root, 1);
        assert!(j.read_only().is_some());
        assert!(j.checkpoint_if_required().is_err());
        assert!(j.append(upsert("a", 0)).is_err());
        assert_eq!(fs::read(root.path()).unwrap(), bytes);
    }
}

#[test]
fn queue_rewrite_config_defaults_roundtrips_and_rejects_zero() {
    use crate::config::model::{SessionsConfig, DEFAULT_QUEUE_REWRITE_BYTES};
    let default: crate::config::Config = toml::from_str("").unwrap();
    assert_eq!(
        default.sessions.queue_rewrite_bytes.get(),
        DEFAULT_QUEUE_REWRITE_BYTES
    );
    let custom: crate::config::Config =
        toml::from_str("[sessions]\nqueue_rewrite_bytes = 4096\n").unwrap();
    assert_eq!(custom.sessions.queue_rewrite_bytes.get(), 4096);
    let encoded = toml::to_string(&custom.sessions).unwrap();
    assert_eq!(
        toml::from_str::<SessionsConfig>(&encoded)
            .unwrap()
            .queue_rewrite_bytes
            .get(),
        4096
    );
    for value in ["0", "-1", "1.5", "'4096'"] {
        assert!(toml::from_str::<crate::config::Config>(&format!(
            "[sessions]\nqueue_rewrite_bytes = {value}\n"
        ))
        .is_err());
    }
}

#[test]
fn checkpoint_policy_preserves_supported_torn_tail_repair() {
    let root = Root::new();
    let mut j = root.open();
    j.append(upsert("a", 0)).unwrap();
    let size = j.bytes().unwrap();
    drop(j);
    let mut bytes = fs::read(root.path()).unwrap();
    bytes.extend(b"{torn");
    fs::write(root.path(), &bytes).unwrap();
    let mut j = open(&root, size - 1);
    assert!(j.read_only().is_none());
    assert_eq!(j.bytes().unwrap(), size);
    assert_eq!(j.append(upsert("b", 1)).unwrap().seq, 3);
}

#[test]
fn checkpoint_policy_does_not_rewrite_or_spend_sequence_for_invalid_append() {
    let root = Root::new();
    let mut j = root.open();
    j.append(upsert("a", 0)).unwrap();
    let size = j.bytes().unwrap();
    drop(j);
    let bytes = fs::read(root.path()).unwrap();
    let mut j = open(&root, size - 1);
    assert!(j.append(upsert("b", 5)).is_err());
    assert_eq!(fs::read(root.path()).unwrap(), bytes);
    assert_eq!(j.seq, 1);
    assert_eq!(j.append(upsert("b", 1)).unwrap().seq, 3);
}
