use super::*;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

#[derive(Clone)]
struct Disk(Arc<AtomicU64>);
impl DiskSpace for Disk {
    fn available(&self, _: &Path) -> std::io::Result<u64> {
        Ok(self.0.load(Ordering::Relaxed))
    }
}
struct Fixture {
    path: PathBuf,
    disk: Disk,
}
impl Fixture {
    fn new() -> Self {
        let key = MessageKey::mint("node.example".into(), 0).unwrap();
        let path = std::env::temp_dir().join(format!("flock-custody-{}", key.message_id));
        fs::create_dir(&path).unwrap();
        Self {
            path: path.join("mesh-mail.sqlite"),
            disk: Disk(Arc::new(AtomicU64::new(u64::MAX))),
        }
    }
    fn open(&self, wall: i64) -> Store<Disk> {
        self.limited(wall, Limits::default())
    }
    fn limited(&self, wall: i64, limits: Limits) -> Store<Disk> {
        Store::open_with(&self.path, wall, limits, self.disk.clone()).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.path.parent().unwrap());
    }
}
fn envelope() -> Envelope {
    let key = MessageKey::mint("origin.example".into(), 1000).unwrap();
    Envelope {
        kind: Kind::Message,
        origin_key: Vec::new(),
        signature: Vec::new(),
        return_binding: ReturnBinding {
            request: key.clone(),
            recipient_node: "receiver.example".into(),
            collection_token: vec![42; 32],
            collection_peers: vec!["peer.example".into()],
        },
        key,
        sender: "sender".into(),
        target_agent: "recipient".into(),
        target_session: "session".into(),
        correlation_id: "thread".into(),
        in_reply_to: None,
        request_key: None,
        intent: "\"needs_reply\"".into(),
        body: b"answer me".to_vec(),
    }
}

#[test]
fn refused_custody_is_terminal_and_never_selected_for_retry() {
    let fixture = Fixture::new();
    let mut store = fixture.open(0);
    let mail = envelope();
    store
        .accept(&mail, CUSTODY_TTL_MS, Admission::Custody, 0)
        .unwrap();
    store.refuse(&mail.key, "forward_limit", 1).unwrap();
    for now in [2, 60_000, 300_000] {
        assert!(matches!(
            store.schedule_retry(&mail.key, 60_000, now),
            Err(Error::InvalidState)
        ));
        assert!(store.retry_ready_limit(now, 10).unwrap().is_empty());
        let status = store
            .status(&mail.key.origin_node, &mail.correlation_id, now)
            .unwrap()
            .unwrap();
        assert_eq!(status.state, "refused");
        assert_eq!(status.detail.as_deref(), Some("forward_limit"));
    }
    drop(store);
    assert!(fixture
        .open(300_001)
        .retry_ready_limit(300_001, 10)
        .unwrap()
        .is_empty());
}

#[test]
fn collection_is_origin_token_and_request_scoped_and_ack_is_idempotent() {
    let f = Fixture::new();
    let mut store = f.open(0);
    let request = envelope();
    store
        .accept(&request, CUSTODY_TTL_MS, Admission::Inbox, 0)
        .unwrap();
    let mut reply = envelope();
    reply.request_key = Some(request.key.clone());
    store
        .accept(&reply, CUSTODY_TTL_MS, Admission::Custody, 0)
        .unwrap();
    let unrelated = envelope();
    store
        .accept(&unrelated, CUSTODY_TTL_MS, Admission::Custody, 0)
        .unwrap();
    let mut query = crate::mesh::collect::AnswerCollect {
        request: request.key.clone(),
        token: request.return_binding.collection_token.clone(),
        ack: vec![],
    };
    assert!(store.collect_answers("other.example", &query, 1).is_err());
    query.token[0] ^= 1;
    assert!(store
        .collect_answers(&request.key.origin_node, &query, 1)
        .is_err());
    query.token[0] ^= 1;
    assert_eq!(
        store
            .collect_answers(&request.key.origin_node, &query, 1)
            .unwrap()[0]
            .envelope,
        reply
    );
    query.ack = vec![reply.key.clone(), unrelated.key.clone()];
    assert!(store
        .collect_answers(&request.key.origin_node, &query, 2)
        .is_err());
    assert_eq!(
        store.get(&reply.key).unwrap().unwrap().state,
        "custody",
        "invalid batch ack is atomic"
    );
    query.ack = vec![reply.key.clone()];
    assert!(store
        .collect_answers(&request.key.origin_node, &query, 2)
        .unwrap()
        .is_empty());
    assert!(store
        .collect_answers(&request.key.origin_node, &query, 3)
        .unwrap()
        .is_empty());
    assert_eq!(store.get(&unrelated.key).unwrap().unwrap().state, "custody");
}

#[test]
fn collection_migration_preserves_existing_writer_generation_and_custody() {
    let f = Fixture::new();
    let request = envelope();
    let mut store = f.open(0);
    store
        .accept(&request, CUSTODY_TTL_MS, Admission::Custody, 0)
        .unwrap();
    assert_eq!(store.handoff_generation().unwrap(), 1);
    store
        .connection
        .execute_batch(
            "DROP INDEX open_collections; DROP INDEX held_recipient;
        DROP INDEX collection_token;
        ALTER TABLE envelopes DROP COLUMN collection_token;
        DROP INDEX request_answers;
        DROP TABLE collect_acks;
        ALTER TABLE envelopes DROP COLUMN request_origin;
        ALTER TABLE envelopes DROP COLUMN request_id;
        ALTER TABLE envelopes DROP COLUMN recipient_node;
        ALTER TABLE envelopes DROP COLUMN reply_expected;
        ALTER TABLE envelopes DROP COLUMN collect_done;
        ALTER TABLE envelopes DROP COLUMN collect_failures;
        ALTER TABLE envelopes DROP COLUMN collect_error;
        DROP INDEX collect_ready;
        ALTER TABLE envelopes DROP COLUMN collect_at;
        ALTER TABLE envelopes DROP COLUMN collect_attempts;
        ALTER TABLE envelopes DROP COLUMN remote_state;
        ALTER TABLE envelopes DROP COLUMN receipt_sent;
        DROP TABLE delivery_attempts;
        DROP TABLE status_signer;
        DROP INDEX status_correlation;
        PRAGMA user_version=7;",
        )
        .unwrap();
    drop(store);
    let mut store = f.open(0);
    assert_eq!(store.handoff_generation().unwrap(), 2);
    assert_eq!(store.get(&request.key).unwrap().unwrap().envelope, request);
    store.finish(&request.key, Outcome::Delivered, 0).unwrap();
    assert_eq!(
        store
            .collect_ready(&request.key.origin_node, 5_000, 1, &[])
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn collection_deadlines_back_off_survive_restart_and_freeze_without_idle_commits() {
    let f = Fixture::new();
    let mut store = f.open(0);
    let request = envelope();
    store
        .accept(&request, CUSTODY_TTL_MS, Admission::Custody, 0)
        .unwrap();
    store.finish(&request.key, Outcome::Delivered, 0).unwrap();
    assert_eq!(
        store
            .collect_ready(&request.key.origin_node, 5_000, 1, &[])
            .unwrap()
            .len(),
        1
    );
    let due = |s: &Store<Disk>| {
        s.connection
            .query_row("SELECT collect_at FROM envelopes", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
    };
    let first = due(&store);
    assert!((10_000..=11_000).contains(&first));
    let before = store.connection.total_changes();
    assert!(store
        .collect_ready(&request.key.origin_node, first - 1, 1, &[])
        .unwrap()
        .is_empty());
    assert_eq!(
        store.connection.total_changes(),
        before,
        "idle scans must not commit"
    );
    store.set_paused(true, 5_010).unwrap();
    drop(store);
    let mut store = f.open(100_000);
    assert!(store
        .collect_ready(&request.key.origin_node, 100_000, 1, &[])
        .unwrap()
        .is_empty());
    assert_eq!(due(&store), first);
    store.set_paused(false, 100_000).unwrap();
    assert_eq!(
        store
            .collect_ready(&request.key.origin_node, 100_000 + first - 5_000, 1, &[])
            .unwrap()
            .len(),
        1
    );
    let second = due(&store);
    assert!((60_000..=61_000).contains(&(second - first - 10)));
    assert_eq!(
        store
            .collect_ready(&request.key.origin_node, 100_000 + second - 5_000, 1, &[])
            .unwrap()
            .len(),
        1
    );
    assert!((299_000..=300_000).contains(&(due(&store) - second - 10)));
}

#[test]
fn durable_acceptance_and_atomic_import_survive_reopen() {
    let f = Fixture::new();
    let e = envelope();
    {
        let mut s = f.open(1000);
        assert_eq!(
            s.connection
                .query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "wal"
        );
        assert_eq!(
            s.connection
                .query_row("PRAGMA synchronous", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            s.accept(&e, CUSTODY_TTL_MS, Admission::Inbox, 1000)
                .unwrap(),
            Accepted::New
        );
    }
    let mut s = f.open(2000);
    let r = s.get(&e.key).unwrap().unwrap();
    assert_eq!(s.inbox_keys().unwrap(), vec![e.key.clone()]);
    assert_eq!(r.envelope, e);
    assert!(r.delivered);
    assert_eq!(r.remaining_ms, DAY_MS - 1000);
    assert_eq!(
        s.accept(&e, CUSTODY_TTL_MS, Admission::Inbox, 2000)
            .unwrap(),
        Accepted::Duplicate
    );
    assert_eq!(
        s.connection
            .query_row("SELECT count(*) FROM inbox_imports", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn failed_inbox_insert_rolls_back_envelope_and_dedup() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let e = envelope();
    s.connection.execute_batch("CREATE TRIGGER fail_import BEFORE INSERT ON inbox_imports BEGIN SELECT RAISE(ABORT,'injected crash boundary'); END;").unwrap();
    assert!(s.accept(&e, 1000, Admission::Inbox, 0).is_err());
    drop(s);
    let mut s = f.open(0);
    assert!(s.get(&e.key).unwrap().is_none());
    s.connection
        .execute_batch("DROP TRIGGER fail_import")
        .unwrap();
    assert_eq!(
        s.accept(&e, 1000, Admission::Inbox, 0).unwrap(),
        Accepted::New
    );
}

#[test]
fn duplicate_cannot_extend_ttl_or_change_immutable_content() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let mut e = envelope();
    s.accept(&e, 1000, Admission::Custody, 0).unwrap();
    assert_eq!(
        s.accept(&e, 5000, Admission::Custody, 500).unwrap(),
        Accepted::Duplicate
    );
    // Duplicate admission is read-only. Read TTL at the caller's wall time,
    // rather than assuming it committed a new persistent clock anchor.
    assert_eq!(
        s.collection_record(&e.key, 500)
            .unwrap()
            .unwrap()
            .remaining_ms,
        500
    );
    e.body.push(1);
    assert!(matches!(
        s.accept(&e, 1000, Admission::Custody, 500),
        Err(Error::ConflictingKey)
    ));
    e.body.pop();
    e.return_binding.collection_token[0] = 0;
    assert!(matches!(
        s.accept(&e, 1000, Admission::Custody, 500),
        Err(Error::ConflictingKey)
    ));
}

#[test]
fn correlation_is_not_dedup_key_and_origins_are_scoped() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let a = envelope();
    let mut b = envelope();
    s.accept(&a, 1000, Admission::Custody, 0).unwrap();
    s.accept(&b, 1000, Admission::Custody, 0).unwrap();
    assert_eq!(
        s.conversation(&a.key.origin_node, "thread").unwrap().len(),
        2
    );
    b.key.origin_node = "other.example".into();
    s.accept(&b, 1000, Admission::Custody, 0).unwrap();
    assert_eq!(
        s.conversation(&b.key.origin_node, "thread").unwrap().len(),
        1
    );
}

#[test]
fn import_has_separate_day_budget_and_preserves_delivery_on_expiry() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let e = envelope();
    s.accept(&e, CUSTODY_TTL_MS, Admission::Custody, 0).unwrap();
    let imported = CUSTODY_TTL_MS - 1;
    s.import(&e.key, imported).unwrap();
    assert_eq!(s.import(&e.key, imported + 1).unwrap(), Accepted::Duplicate);
    s.maintain(imported + DAY_MS - 1).unwrap();
    assert_eq!(s.get(&e.key).unwrap().unwrap().state, "inbox");
    s.maintain(imported + DAY_MS).unwrap();
    let r = s.get(&e.key).unwrap().unwrap();
    assert!(r.delivered);
    assert_eq!(r.state, "inbox_expired");
    assert!(r.envelope.body.is_empty());
}

#[test]
fn terminal_receipts_preserve_dedup_after_body_deletion() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let e = envelope();
    s.accept(&e, CUSTODY_TTL_MS, Admission::Custody, 0).unwrap();
    s.finish(&e.key, Outcome::Transferred, 0).unwrap();
    assert!(s.get(&e.key).unwrap().unwrap().envelope.body.is_empty());
    assert_eq!(
        s.accept(&e, CUSTODY_TTL_MS, Admission::Custody, 1).unwrap(),
        Accepted::Duplicate
    );
    assert_eq!(s.maintain(CUSTODY_TTL_MS).unwrap(), 0);
    assert_eq!(s.maintain(CUSTODY_TTL_MS + DAY_MS).unwrap(), 1);
    assert!(s.get(&e.key).unwrap().is_none());
}

#[test]
fn final_outcomes_live_seven_days_after_local_termination() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let e = envelope();
    s.accept(&e, 1000, Admission::Custody, 0).unwrap();
    s.maintain(1000).unwrap();
    assert_eq!(s.get(&e.key).unwrap().unwrap().state, "expired");
    assert_eq!(s.maintain(1000 + CUSTODY_TTL_MS - 1).unwrap(), 0);
    assert_eq!(s.maintain(1000 + CUSTODY_TTL_MS).unwrap(), 1);
}

#[test]
fn active_quota_refuses_without_evicting_and_allows_idempotent_retry() {
    let f = Fixture::new();
    let mut s = f.limited(
        0,
        Limits {
            active_envelopes: 1,
            ..Limits::default()
        },
    );
    let e = envelope();
    s.accept(&e, 1000, Admission::Custody, 0).unwrap();
    assert_eq!(
        s.accept(&e, 1000, Admission::Custody, 0).unwrap(),
        Accepted::Duplicate
    );
    assert!(matches!(
        s.accept(&envelope(), 1000, Admission::Inbox, 0),
        Err(Error::MailStoreFull)
    ));
    assert_eq!(s.get(&e.key).unwrap().unwrap().envelope, e);
    s.finish(&e.key, Outcome::Delivered, 0).unwrap();
    s.accept(&envelope(), 1000, Admission::Custody, 0).unwrap();
}

#[test]
fn body_quota_preserves_metadata_reserve_and_terminal_capacity() {
    let f = Fixture::new();
    let mut s = f.limited(
        0,
        Limits {
            logical_bytes: 10_000,
            metadata_reserve: 4000,
            ..Limits::default()
        },
    );
    let mut e = envelope();
    e.body = vec![0; 6001];
    assert!(matches!(
        s.accept(&e, 1000, Admission::Custody, 0),
        Err(Error::MailStoreFull)
    ));
    e.body.pop();
    s.accept(&e, 1000, Admission::Custody, 0).unwrap();
    s.finish(&e.key, Outcome::Transferred, 0).unwrap();
    assert_eq!(s.get(&e.key).unwrap().unwrap().state, "transferred");
}

#[test]
fn metadata_counts_toward_total_quota() {
    let f = Fixture::new();
    let mut s = f.limited(
        0,
        Limits {
            logical_bytes: 2000,
            metadata_reserve: 1000,
            ..Limits::default()
        },
    );
    let mut e = envelope();
    e.correlation_id = "x".repeat(2000);
    assert!(matches!(
        s.accept(&e, 1000, Admission::Custody, 0),
        Err(Error::MailStoreFull)
    ));
    assert!(s.get(&e.key).unwrap().is_none());
}

#[test]
fn disk_reserve_refuses_writes_without_acknowledgment() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let e = envelope();
    f.disk.0.store(0, Ordering::Relaxed);
    assert!(matches!(
        s.accept(&e, 1000, Admission::Custody, 0),
        Err(Error::MailStoreFull)
    ));
    assert!(s.get(&e.key).unwrap().is_none());
    assert!(Store::open_with(&f.path, 0, Limits::default(), f.disk.clone()).is_ok());
}

#[test]
fn rollback_and_forward_jump_remain_conservative_across_restart() {
    let f = Fixture::new();
    let mut s = f.open(1000);
    let e = envelope();
    s.accept(&e, 1000, Admission::Custody, 1000).unwrap();
    s.maintain(1500).unwrap();
    drop(s);
    let mut s = f.open(100);
    assert_eq!(s.get(&e.key).unwrap().unwrap().remaining_ms, 500);
    s.maintain(3000).unwrap();
    drop(s);
    let mut s = f.open(1100);
    assert_eq!(s.get(&e.key).unwrap().unwrap().state, "expired");
    assert_eq!(
        s.accept(&e, 1000, Admission::Custody, 1100).unwrap(),
        Accepted::Duplicate
    );
}

#[test]
fn pause_freezes_all_mutations_ttl_retry_and_gc_across_restart() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let e = envelope();
    s.accept(&e, CUSTODY_TTL_MS, Admission::Custody, 0).unwrap();
    s.schedule_retry(&e.key, 60_000, 1000).unwrap();
    s.set_paused(true, 2000).unwrap();
    let before = s.get(&e.key).unwrap().unwrap();
    drop(s);
    let mut s = f.open(20 * CUSTODY_TTL_MS);
    assert_eq!(s.get(&e.key).unwrap().unwrap(), before);
    assert!(matches!(
        s.accept(&envelope(), 1000, Admission::Custody, 20 * CUSTODY_TTL_MS),
        Err(Error::Paused)
    ));
    assert!(matches!(
        s.import(&e.key, 20 * CUSTODY_TTL_MS),
        Err(Error::Paused)
    ));
    assert!(matches!(
        s.finish(&e.key, Outcome::Delivered, 20 * CUSTODY_TTL_MS),
        Err(Error::Paused)
    ));
    assert!(matches!(
        s.schedule_retry(&e.key, 60_000, 20 * CUSTODY_TTL_MS),
        Err(Error::Paused)
    ));
    assert!(matches!(
        s.maintain(20 * CUSTODY_TTL_MS),
        Err(Error::Paused)
    ));
    s.set_paused(false, 1000).unwrap();
    s.maintain(2000).unwrap();
    assert_eq!(
        s.get(&e.key).unwrap().unwrap().remaining_ms,
        before.remaining_ms - 1000
    );
    assert_eq!(
        s.get(&e.key).unwrap().unwrap().retry_at_ms,
        before.retry_at_ms
    );
}

#[test]
fn pause_preserves_terminal_retention() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let e = envelope();
    s.accept(&e, 1, Admission::Custody, 0).unwrap();
    s.maintain(1).unwrap();
    s.set_paused(true, 2).unwrap();
    drop(s);
    let mut s = f.open(20 * CUSTODY_TTL_MS);
    s.set_paused(false, 20 * CUSTODY_TTL_MS).unwrap();
    assert_eq!(s.maintain(20 * CUSTODY_TTL_MS).unwrap(), 0);
    assert_eq!(s.maintain(21 * CUSTODY_TTL_MS).unwrap(), 1);
}

#[test]
fn invalid_budgets_and_ids_are_refused() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let mut e = envelope();
    for ttl in [0, -1, CUSTODY_TTL_MS + 1] {
        assert!(matches!(
            s.accept(&e, ttl, Admission::Custody, 0),
            Err(Error::InvalidEnvelope)
        ));
    }
    e.key.message_id = "caller-label".into();
    assert!(matches!(
        s.accept(&e, 1, Admission::Custody, 0),
        Err(Error::InvalidEnvelope)
    ));
}

#[test]
fn minted_ulids_encode_time_and_have_independent_entropy() {
    let a = MessageKey::mint("origin.example".into(), 0).unwrap();
    let b = MessageKey::mint("origin.example".into(), 0).unwrap();
    assert!(a.is_valid());
    assert!(b.is_valid());
    assert_ne!(a, b);
    assert!(a.message_id.starts_with("0000000000"));
}

#[test]
fn unread_and_read_are_distinct_and_terminal_outcome_cannot_be_rewritten() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let e = envelope();
    s.accept(&e, 1000, Admission::Inbox, 0).unwrap();
    s.finish(&e.key, Outcome::Read, 1).unwrap();
    assert!(s.get(&e.key).unwrap().unwrap().delivered);
    assert!(matches!(
        s.finish(&e.key, Outcome::RecipientGone, 2),
        Err(Error::InvalidState)
    ));
    s.finish(&e.key, Outcome::Read, 3).unwrap();
}

#[test]
fn checkpoint_bounds_wal_and_corruption_never_falls_back_to_memory() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let mut e = envelope();
    e.body = vec![0; 128 * 1024];
    s.accept(&e, 1000, Admission::Custody, 0).unwrap();
    s.checkpoint().unwrap();
    let occupied_bytes = fs::metadata(&f.path).unwrap().len();
    assert_eq!(
        s.connection
            .query_row("PRAGMA auto_vacuum", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        fs::metadata(f.path.with_file_name("mesh-mail.sqlite-wal"))
            .unwrap()
            .len(),
        0
    );
    s.maintain(1000).unwrap();
    s.maintain(1000 + CUSTODY_TTL_MS).unwrap();
    assert!(fs::metadata(&f.path).unwrap().len() < occupied_bytes);
    drop(s);
    fs::write(&f.path, b"not a sqlite database").unwrap();
    assert!(Store::open_with(&f.path, 0, Limits::default(), f.disk.clone()).is_err());
}

#[test]
fn retry_schedule_recovers_after_restart_without_charging_pause() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let e = envelope();
    s.accept(&e, CUSTODY_TTL_MS, Admission::Custody, 0).unwrap();
    s.schedule_retry(&e.key, 60_000, 0).unwrap();
    s.set_paused(true, 1000).unwrap();
    drop(s);
    let mut s = f.open(1_000_000);
    assert!(matches!(s.retry_ready(1_000_000), Err(Error::Paused)));
    s.set_paused(false, 1_000_000).unwrap();
    assert!(s.retry_ready(1_058_999).unwrap().is_empty());
    assert_eq!(s.retry_ready(1_059_000).unwrap(), vec![e.key]);
}

#[test]
fn expired_custody_cannot_be_imported_or_marked_delivered() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let e = envelope();
    s.accept(&e, 1000, Admission::Custody, 0).unwrap();
    assert!(matches!(s.import(&e.key, 1000), Err(Error::InvalidState)));
    assert!(matches!(
        s.finish(&e.key, Outcome::Delivered, 1000),
        Err(Error::InvalidState)
    ));
    s.maintain(1000).unwrap();
    assert_eq!(s.get(&e.key).unwrap().unwrap().state, "expired");
}

#[test]
fn collection_tokens_are_independently_minted() {
    let key = envelope().key;
    let a = ReturnBinding::mint(key.clone(), "receiver.example".into(), vec![]).unwrap();
    let b = ReturnBinding::mint(key, "receiver.example".into(), vec![]).unwrap();
    assert_eq!(a.collection_token.len(), 32);
    assert_ne!(a.collection_token, b.collection_token);
}

#[test]
fn reserve_never_blocks_reclamation_pause_reopen_or_existing_body_import() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let expired = envelope();
    let imported = envelope();
    let finished = envelope();
    for e in [&expired, &imported, &finished] {
        s.accept(e, 1000, Admission::Custody, 0).unwrap();
    }
    f.disk.0.store(0, Ordering::Relaxed);
    s.set_paused(true, 0).unwrap();
    drop(s);
    let mut s = f.open(100);
    assert!(s.clock().unwrap().paused);
    s.set_paused(false, 100).unwrap();
    s.import(&imported.key, 100).unwrap();
    s.finish(&finished.key, Outcome::Delivered, 100).unwrap();
    s.checkpoint().unwrap();
    s.maintain(1100).unwrap();
    assert_eq!(s.get(&expired.key).unwrap().unwrap().state, "expired");
    s.maintain(100 + DAY_MS).unwrap();
    assert_eq!(s.maintain(100 + DAY_MS + CUSTODY_TTL_MS).unwrap(), 3);
    drop(s);
    let s = f.open(100 + DAY_MS + CUSTODY_TTL_MS);
    assert!(s.get(&expired.key).unwrap().is_none());
}

fn usage(s: &Store<Disk>) -> (i64, i64, i64) {
    s.connection
        .query_row("SELECT total_bytes,body_bytes,active FROM usage", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap()
}

#[test]
fn unversioned_store_migrates_once_and_counters_track_rollback_release_and_gc() {
    let f = Fixture::new();
    let e = envelope();
    {
        let c = Connection::open(&f.path).unwrap();
        c.execute_batch(schema::BASE).unwrap();
        c.execute(
            "INSERT INTO identity_pins VALUES(?1,?2,?3)",
            params!["peer.example", "node.example", vec![7u8; 32]],
        )
        .unwrap();
        let mut metadata = e.clone();
        metadata.body.clear();
        c.execute("INSERT INTO envelopes VALUES(?1,?2,?3,?4,X'',?5,'custody',1000,NULL,1000,NULL,0,0,100)",
            params![e.key.origin_node,e.key.message_id,e.correlation_id,serde_json::to_string(&metadata).unwrap(),e.body]).unwrap();
    }
    let mut s = f.open(0);
    assert_eq!(
        s.get_pin("peer.example").unwrap(),
        Some(IdentityPin {
            node_id: "node.example".into(),
            public_key: vec![7; 32],
        })
    );
    assert!(s
        .get_pin_from(PinSource::Inbound, "peer.example")
        .unwrap()
        .is_none());
    assert_eq!(
        usage(&s),
        (100 + e.body.len() as i64, e.body.len() as i64, 1)
    );
    assert_eq!(
        s.connection
            .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        schema::VERSION
    );
    assert_eq!(
        s.by_collection_token(&e.return_binding.collection_token)
            .unwrap(),
        vec![e.key.clone()]
    );
    s.finish(&e.key, Outcome::Delivered, 0).unwrap();
    assert_eq!(usage(&s), (100, 0, 0));
    let before = usage(&s);
    s.connection.execute_batch("CREATE TRIGGER fail_import BEFORE INSERT ON inbox_imports BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(s.accept(&envelope(), 1000, Admission::Inbox, 0).is_err());
    assert_eq!(usage(&s), before);
    drop(s);
    let mut s = f.open(0);
    assert_eq!(usage(&s), before);
    s.maintain(CUSTODY_TTL_MS).unwrap();
    assert_eq!(usage(&s), (0, 0, 0));
}

#[test]
fn newer_schema_is_refused_without_changing_version() {
    let f = Fixture::new();
    let s = f.open(0);
    s.connection
        .pragma_update(None, "user_version", schema::VERSION + 1)
        .unwrap();
    drop(s);
    let result = Store::open_with(&f.path, 0, Limits::default(), f.disk.clone());
    assert!(
        matches!(result, Err(Error::NewerSchema { found, supported }) if found == schema::VERSION + 1 && supported == schema::VERSION)
    );
    let c = Connection::open(&f.path).unwrap();
    assert_eq!(
        c.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        schema::VERSION + 1
    );
}

#[test]
fn gc_processes_multiple_batches_and_counters_remain_exact() {
    let f = Fixture::new();
    let mut s = f.open(0);
    // Seed more than two batches through real admission, then verify every
    // transition and cascade using the durable aggregate counters.
    for _ in 0..1001 {
        s.accept(&envelope(), 1, Admission::Inbox, 0).unwrap();
    }
    assert_eq!(usage(&s).2, 1001);
    s.maintain(DAY_MS).unwrap();
    assert_eq!(usage(&s).1, 0);
    assert_eq!(usage(&s).2, 0);
    assert_eq!(s.maintain(DAY_MS + CUSTODY_TTL_MS).unwrap(), 1001);
    assert_eq!(usage(&s), (0, 0, 0));
    assert_eq!(
        s.connection
            .query_row("SELECT count(*) FROM inbox_imports", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn retry_claims_exclude_other_workers_and_survive_reopen_until_expiry() {
    let f = Fixture::new();
    let mut a = f.open(0);
    let e = envelope();
    a.accept(&e, CUSTODY_TTL_MS, Admission::Custody, 0).unwrap();
    let mut b = f.open(0);
    assert_eq!(a.retry_ready(0).unwrap(), vec![e.key.clone()]);
    assert!(b.retry_ready(0).unwrap().is_empty());
    drop(a);
    drop(b);
    let mut c = f.open(59_999);
    assert!(c.retry_ready(59_999).unwrap().is_empty());
    assert_eq!(c.retry_ready(60_000).unwrap(), vec![e.key]);
}

#[test]
fn request_and_token_lookups_are_scoped_and_survive_restart() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let request = envelope();
    let mut reply = envelope();
    reply.request_key = Some(request.key.clone());
    reply.return_binding = request.return_binding.clone();
    let mut unrelated = envelope();
    unrelated.return_binding.collection_token = vec![7; 32];
    for e in [&request, &reply, &unrelated] {
        s.accept(e, 1000, Admission::Custody, 0).unwrap();
    }
    drop(s);
    let s = f.open(0);
    assert_eq!(
        s.by_request_key(&request.key).unwrap(),
        vec![reply.key.clone()]
    );
    assert!(s.by_request_key(&unrelated.key).unwrap().is_empty());
    let keys = s
        .by_collection_token(&request.return_binding.collection_token)
        .unwrap();
    assert_eq!(keys.len(), 2);
    assert!(keys.contains(&request.key));
    assert!(keys.contains(&reply.key));
    assert!(s.by_collection_token(&[0; 32]).unwrap().is_empty());
}

#[test]
fn pins_require_explicit_reset_and_persist() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let pin = IdentityPin {
        node_id: "node.example".into(),
        public_key: vec![1; 32],
    };
    s.put_pin("peer.example", &pin).unwrap();
    s.put_pin("peer.example", &pin).unwrap();
    s.put_pin("other.example", &pin).unwrap();
    assert_eq!(s.get_pin("other.example").unwrap(), Some(pin.clone()));
    let replacement = IdentityPin {
        node_id: "new.example".into(),
        public_key: vec![2; 32],
    };
    assert!(matches!(
        s.put_pin("peer.example", &replacement),
        Err(Error::IdentityPinConflict)
    ));
    drop(s);
    let mut s = f.open(0);
    assert_eq!(s.get_pin("peer.example").unwrap(), Some(pin));
    assert!(s.reset_pin("peer.example").unwrap());
    s.put_pin("peer.example", &replacement).unwrap();
    assert_eq!(s.get_pin("peer.example").unwrap(), Some(replacement));
}

#[test]
fn system_disk_queries_the_store_directory_and_reports_bad_paths() {
    fn assert_send<T: Send>() {}
    assert_send::<Store>();
    let f = Fixture::new();
    assert!(SystemDisk.available(f.path.parent().unwrap()).is_ok());
    assert!(SystemDisk.available(&f.path.join("missing")).is_err());
}

#[test]
fn concurrent_workers_claim_a_message_only_once() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let e = envelope();
    s.accept(&e, CUSTODY_TTL_MS, Admission::Custody, 0).unwrap();
    drop(s);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let path = f.path.clone();
            let disk = f.disk.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut s = Store::open_with(&path, 0, Limits::default(), disk).unwrap();
                barrier.wait();
                s.retry_ready(0).unwrap()
            })
        })
        .collect();
    let keys: Vec<_> = workers
        .into_iter()
        .flat_map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(keys, vec![e.key]);
}

#[test]
fn pin_directions_reset_independently_and_identity_cannot_gain_an_alias() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let configured = IdentityPin {
        node_id: "outbound.example".into(),
        public_key: vec![1; 32],
    };
    let inbound = IdentityPin {
        node_id: "inbound.example".into(),
        public_key: vec![2; 32],
    };
    s.put_pin("peer.example", &configured).unwrap();
    s.put_pin_from(PinSource::Inbound, "peer.example", &inbound)
        .unwrap();
    assert_eq!(
        s.conflicting_pin_name(PinSource::Inbound, "other.example", &inbound)
            .unwrap(),
        Some("peer.example".into())
    );
    s.put_pin("other.example", &inbound).unwrap();
    assert_eq!(
        s.pin_name_from(PinSource::Configured, &inbound).unwrap(),
        Some("other.example".into())
    );
    assert!(matches!(
        s.put_pin_from(PinSource::Inbound, "other.example", &inbound),
        Err(Error::IdentityPinConflict)
    ));
    let reused_key = IdentityPin {
        node_id: "forged.example".into(),
        public_key: inbound.public_key.clone(),
    };
    assert!(matches!(
        s.put_pin_from(PinSource::Inbound, "other.example", &reused_key),
        Err(Error::IdentityPinConflict)
    ));
    s.reset_pin("peer.example").unwrap();
    assert_eq!(
        s.get_pin_from(PinSource::Inbound, "peer.example").unwrap(),
        Some(inbound.clone())
    );
    s.put_pin("peer.example", &configured).unwrap();
    s.reset_pin_from(PinSource::Inbound, "peer.example")
        .unwrap();
    assert_eq!(s.get_pin("peer.example").unwrap(), Some(configured));
    assert_eq!(
        s.get_pin_from(PinSource::Inbound, "peer.example").unwrap(),
        None
    );
}

#[test]
fn configured_alias_and_inbound_name_enroll_in_either_order_and_survive_reopen() {
    for inbound_first in [false, true] {
        let f = Fixture::new();
        let mut s = f.open(0);
        let pin = IdentityPin {
            node_id: "node.example".into(),
            public_key: vec![9; 32],
        };
        let entries = if inbound_first {
            [
                (PinSource::Inbound, "remote.example"),
                (PinSource::Configured, "alias.example"),
            ]
        } else {
            [
                (PinSource::Configured, "alias.example"),
                (PinSource::Inbound, "remote.example"),
            ]
        };
        for (source, name) in entries {
            assert!(s
                .conflicting_pin_name(source, name, &pin)
                .unwrap()
                .is_none());
            s.put_pin_from(source, name, &pin).unwrap();
        }
        drop(s);
        let mut s = f.open(0);
        assert_eq!(s.get_pin("alias.example").unwrap(), Some(pin.clone()));
        assert_eq!(
            s.get_pin_from(PinSource::Inbound, "remote.example")
                .unwrap(),
            Some(pin.clone())
        );
        assert_eq!(
            s.conflicting_pin_name(PinSource::Inbound, "impostor.example", &pin)
                .unwrap(),
            Some("remote.example".into())
        );
        assert!(matches!(
            s.put_pin_from(PinSource::Inbound, "impostor.example", &pin),
            Err(Error::IdentityPinConflict)
        ));
    }
}

#[test]
fn version_three_pin_migration_preserves_both_directions_and_allows_local_aliases() {
    let f = Fixture::new();
    let s = f.open(0);
    s.connection
        .execute_batch(
            "DROP TABLE writer_generation;
         DROP TABLE identity_pins;
         CREATE TABLE identity_pins (
             source TEXT NOT NULL, peer TEXT NOT NULL, node_id TEXT NOT NULL,
             public_key BLOB NOT NULL, PRIMARY KEY(source,peer),
             UNIQUE(source,node_id), UNIQUE(source,public_key));
         DROP INDEX open_collections; DROP INDEX held_recipient;
        DROP INDEX collection_token;
        ALTER TABLE envelopes DROP COLUMN collection_token;
        DROP INDEX request_answers;
        DROP TABLE collect_acks;
        ALTER TABLE envelopes DROP COLUMN request_origin;
        ALTER TABLE envelopes DROP COLUMN request_id;
        ALTER TABLE envelopes DROP COLUMN recipient_node;
        ALTER TABLE envelopes DROP COLUMN reply_expected;
        ALTER TABLE envelopes DROP COLUMN collect_done;
        ALTER TABLE envelopes DROP COLUMN collect_failures;
        ALTER TABLE envelopes DROP COLUMN collect_error;
        DROP INDEX collect_ready;
         ALTER TABLE envelopes DROP COLUMN collect_at;
         ALTER TABLE envelopes DROP COLUMN collect_attempts;
         ALTER TABLE envelopes DROP COLUMN remote_state;
         ALTER TABLE envelopes DROP COLUMN receipt_sent;
         DROP TABLE delivery_attempts;
         DROP TABLE status_signer;
         DROP INDEX status_correlation;
         PRAGMA user_version=3;",
        )
        .unwrap();
    let pin = IdentityPin {
        node_id: "node.example".into(),
        public_key: vec![5; 32],
    };
    for (source, name) in [
        ("configured", "alias.example"),
        ("inbound", "remote.example"),
    ] {
        s.connection
            .execute(
                "INSERT INTO identity_pins VALUES(?1,?2,?3,?4)",
                params![source, name, pin.node_id, pin.public_key],
            )
            .unwrap();
    }
    drop(s);
    let mut s = f.open(0);
    assert_eq!(s.get_pin("alias.example").unwrap(), Some(pin.clone()));
    assert_eq!(
        s.get_pin_from(PinSource::Inbound, "remote.example")
            .unwrap(),
        Some(pin.clone())
    );
    s.put_pin("second-alias.example", &pin).unwrap();
    assert_eq!(
        s.get_pin("second-alias.example").unwrap(),
        Some(pin.clone())
    );
    assert!(matches!(
        s.put_pin_from(PinSource::Inbound, "impostor.example", &pin),
        Err(Error::IdentityPinConflict)
    ));
}

#[test]
fn first_contact_both_pins_roll_back_on_inbound_conflict() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let original = IdentityPin {
        node_id: "original.example".into(),
        public_key: vec![1; 32],
    };
    let replacement = IdentityPin {
        node_id: "replacement.example".into(),
        public_key: vec![2; 32],
    };
    s.put_pin_from(PinSource::Inbound, "peer.example", &original)
        .unwrap();
    assert!(matches!(
        s.put_inbound_configured_pin("peer.example", &replacement),
        Err(Error::IdentityPinConflict)
    ));
    assert_eq!(s.get_pin("peer.example").unwrap(), None);
    assert_eq!(
        s.get_pin_from(PinSource::Inbound, "peer.example").unwrap(),
        Some(original)
    );
}

#[test]
fn concurrent_first_contact_and_outbound_pins_match_or_refuse() {
    for matching in [false, true] {
        let f = Fixture::new();
        let mut inbound = f.open(0);
        let mut outbound = f.open(0);
        let first = IdentityPin {
            node_id: "first.example".into(),
            public_key: vec![1; 32],
        };
        let second = if matching {
            first.clone()
        } else {
            IdentityPin {
                node_id: "second.example".into(),
                public_key: vec![2; 32],
            }
        };
        let barrier = std::sync::Barrier::new(2);
        let (incoming, outgoing) = std::thread::scope(|scope| {
            let a = scope.spawn(|| {
                barrier.wait();
                inbound.put_inbound_configured_pin("peer.example", &first)
            });
            let b = scope.spawn(|| {
                barrier.wait();
                outbound.put_pin("peer.example", &second)
            });
            (a.join().unwrap(), b.join().unwrap())
        });
        if matching {
            incoming.unwrap();
            outgoing.unwrap();
        } else {
            assert_ne!(incoming.is_ok(), outgoing.is_ok());
            let error = incoming.err().or(outgoing.err()).unwrap();
            assert!(matches!(error, Error::IdentityPinConflict));
        }
        let s = f.open(1);
        let configured = s.get_pin("peer.example").unwrap().unwrap();
        if let Some(claimed) = s.get_pin_from(PinSource::Inbound, "peer.example").unwrap() {
            assert_eq!(claimed, configured);
        } else {
            assert!(!matching);
            assert_eq!(configured, second);
        }
    }
}

#[test]
fn pin_origin_survives_reconnect_restart_and_reset() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let pin = IdentityPin {
        node_id: "node.example".into(),
        public_key: vec![7; 32],
    };
    s.put_inbound_configured_pin("peer.example", &pin).unwrap();
    s.put_pin("peer.example", &pin).unwrap();
    drop(s);
    let mut s = f.open(0);
    for source in [PinSource::Configured, PinSource::Inbound] {
        assert_eq!(
            s.pin_origin(source, "peer.example").unwrap(),
            Some(PinOrigin::InboundFirstContact)
        );
    }
    s.reset_pin("peer.example").unwrap();
    assert_eq!(
        s.pin_origin(PinSource::Configured, "peer.example").unwrap(),
        None
    );
    s.put_pin("peer.example", &pin).unwrap();
    s.put_inbound_configured_pin("peer.example", &pin).unwrap();
    assert_eq!(
        s.pin_origin(PinSource::Configured, "peer.example").unwrap(),
        Some(PinOrigin::Dialed)
    );
    assert_eq!(
        s.pin_origin(PinSource::Inbound, "peer.example").unwrap(),
        Some(PinOrigin::InboundFirstContact)
    );
}

#[test]
fn version_four_pin_origin_is_unknown_after_migration() {
    let f = Fixture::new();
    let s = f.open(0);
    s.connection
        .execute_batch(
            "DROP TABLE writer_generation;
        ALTER TABLE identity_pins DROP COLUMN origin;
        INSERT INTO identity_pins VALUES ('configured','peer.example','node.example',zeroblob(32));
        DROP INDEX open_collections; DROP INDEX held_recipient;
        DROP INDEX collection_token;
        ALTER TABLE envelopes DROP COLUMN collection_token;
        DROP INDEX request_answers;
        DROP TABLE collect_acks;
        ALTER TABLE envelopes DROP COLUMN request_origin;
        ALTER TABLE envelopes DROP COLUMN request_id;
        ALTER TABLE envelopes DROP COLUMN recipient_node;
        ALTER TABLE envelopes DROP COLUMN reply_expected;
        ALTER TABLE envelopes DROP COLUMN collect_done;
        ALTER TABLE envelopes DROP COLUMN collect_failures;
        ALTER TABLE envelopes DROP COLUMN collect_error;
        DROP INDEX collect_ready;
         ALTER TABLE envelopes DROP COLUMN collect_at;
         ALTER TABLE envelopes DROP COLUMN collect_attempts;
         ALTER TABLE envelopes DROP COLUMN remote_state;
         ALTER TABLE envelopes DROP COLUMN receipt_sent;
         DROP TABLE delivery_attempts;
         DROP TABLE status_signer;
         DROP INDEX status_correlation;
         PRAGMA user_version=4;",
        )
        .unwrap();
    drop(s);
    let mut s = f.open(0);
    let pin = s.get_pin("peer.example").unwrap().unwrap();
    s.put_pin("peer.example", &pin).unwrap();
    assert_eq!(
        s.pin_origin(PinSource::Configured, "peer.example").unwrap(),
        Some(PinOrigin::Unknown)
    );
}

#[test]
fn idle_retry_and_unchanged_pause_do_not_write() {
    let fixture = Fixture::new();
    let mut store = fixture.open(1000);
    let changes = store.connection.total_changes();
    for now in 1000..1100 {
        store.set_paused(false, now).unwrap();
        assert!(store.retry_ready_limit(now, 4).unwrap().is_empty());
    }
    assert_eq!(store.connection.total_changes(), changes);
    let message = envelope();
    store
        .accept(&message, CUSTODY_TTL_MS, Admission::Custody, 1100)
        .unwrap();
    store.schedule_retry(&message.key, 60_000, 1100).unwrap();
    let changes = store.connection.total_changes();
    assert!(store.retry_ready(1200).unwrap().is_empty());
    assert_eq!(store.connection.total_changes(), changes);
    assert_eq!(store.retry_ready(61_100).unwrap(), vec![message.key]);
}

#[test]
fn batch_read_expires_overdue_rows_and_reads_live_rows() {
    let fixture = Fixture::new();
    let mut store = fixture.open(1000);
    let expired = envelope();
    let live = envelope();
    store
        .accept(&expired, CUSTODY_TTL_MS, Admission::Inbox, 1000)
        .unwrap();
    store
        .accept(&live, CUSTODY_TTL_MS, Admission::Inbox, 2000)
        .unwrap();
    store
        .read_inbox(&[expired.key.clone(), live.key.clone()], 1000 + DAY_MS)
        .unwrap();
    assert_eq!(
        store.get(&expired.key).unwrap().unwrap().state,
        "inbox_expired"
    );
    let record = store.get(&live.key).unwrap().unwrap();
    assert_eq!(record.state, "read");
    assert_eq!(record.remaining_ms, 1000);
    assert_eq!(record.mailbox_ttl_ms, DAY_MS);
}

#[test]
fn dedupe_is_scoped_to_authenticated_origin() {
    let fixture = Fixture::new();
    let mut store = fixture.open(1000);
    let first = envelope();
    let mut second = first.clone();
    second.key.origin_node = "another.example".into();
    second.return_binding.request = second.key.clone();
    assert_eq!(
        store
            .accept(&first, DAY_MS, Admission::Inbox, 1000)
            .unwrap(),
        Accepted::New
    );
    assert_eq!(
        store
            .accept(&second, DAY_MS, Admission::Inbox, 1000)
            .unwrap(),
        Accepted::New
    );
}

#[test]
fn origin_policy_prefers_configured_alias_to_inbound_name() {
    let fixture = Fixture::new();
    let mut store = fixture.open(1000);
    let pin = IdentityPin {
        node_id: "origin.example".into(),
        public_key: vec![1; 32],
    };
    store
        .put_pin_from(PinSource::Inbound, "inbound.example", &pin)
        .unwrap();
    assert_eq!(
        store.origin_name(&pin.node_id).unwrap().as_deref(),
        Some("inbound.example")
    );
    store.put_pin("configured.example", &pin).unwrap();
    assert_eq!(
        store.origin_name(&pin.node_id).unwrap().as_deref(),
        Some("configured.example")
    );
}

#[test]
fn collection_only_polls_delivered_open_local_questions_and_retains_ack_debt() {
    let f = Fixture::new();
    let mut store = f.open(0);
    let request = envelope();
    let mut notice = envelope();
    notice.intent = "\"fyi\"".into();
    let mut foreign = envelope();
    foreign.key.origin_node = "foreign.example".into();
    foreign.return_binding.request = foreign.key.clone();
    for item in [&request, &notice, &foreign] {
        store
            .accept(item, CUSTODY_TTL_MS, Admission::Custody, 0)
            .unwrap();
    }
    assert!(store
        .collect_ready(&request.key.origin_node, 0, 16, &[])
        .unwrap()
        .is_empty());
    for item in [&request, &notice, &foreign] {
        store.finish(&item.key, Outcome::Delivered, 0).unwrap();
    }
    assert!(store
        .collect_ready(&request.key.origin_node, 4_999, 16, &[])
        .unwrap()
        .is_empty());
    // A notice is polled only for its read receipt, and a terminal one ends
    // that. The question's own polling is what the rest of this test pins.
    store.import_receipt(&notice.key, "read").unwrap();
    let claimed = store
        .collect_ready(&request.key.origin_node, 5_000, 16, &[])
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].envelope.key, request.key);
    let mut answer = envelope();
    answer.key.origin_node = request.return_binding.recipient_node.clone();
    answer.return_binding.request = answer.key.clone();
    answer.request_key = Some(request.key.clone());
    answer.correlation_id = "question:deferred".into();
    store
        .accept_collected(&answer, CUSTODY_TTL_MS, 5_000)
        .unwrap();
    store
        .collection_acked(&request.key, &[answer.key.clone()])
        .unwrap();
    assert!(
        !store
            .collect_ready(&request.key.origin_node, 20_000, 1, &[])
            .unwrap()
            .is_empty(),
        "a deferral keeps the conversation open"
    );
    answer.key.message_id = MessageKey::mint(answer.key.origin_node.clone(), 20_000)
        .unwrap()
        .message_id;
    answer.return_binding.request = answer.key.clone();
    answer.correlation_id = "final".into();
    store
        .accept_collected(&answer, CUSTODY_TTL_MS, 20_000)
        .unwrap();
    drop(store);
    let mut store = f.open(90_000);
    assert_eq!(
        store.collection_acks(&request.key).unwrap(),
        vec![answer.key.clone()]
    );
    assert_eq!(
        store
            .collect_ready(&request.key.origin_node, 90_000, 1, &[])
            .unwrap()
            .len(),
        1,
        "lost acknowledgements remain collectable after restart"
    );
    store.collection_acked(&request.key, &[answer.key]).unwrap();
    store
        .collection_failed(&request.key, "invalid reply binding", true)
        .unwrap();
    assert!(
        store
            .collection_error(&request.key.origin_node, &request.correlation_id)
            .unwrap()
            .is_none(),
        "a later bad answer cannot override a final answer"
    );
    assert!(
        !store
            .collect_ready(&request.key.origin_node, 400_000, 16, &[])
            .unwrap()
            .is_empty(),
        "a final answer still needs a terminal read receipt"
    );
    store.import_receipt(&request.key, "read").unwrap();
    assert!(
        store
            .collect_ready(&request.key.origin_node, 500_000, 16, &[])
            .unwrap()
            .is_empty(),
        "the final answer and read receipt stop polling"
    );
}

#[test]
fn collection_empty_polls_are_read_only_and_bad_answers_are_quarantined_individually() {
    let f = Fixture::new();
    let mut store = f.open(0);
    let request = envelope();
    store
        .accept(&request, CUSTODY_TTL_MS, Admission::Inbox, 0)
        .unwrap();
    let query = crate::mesh::collect::AnswerCollect {
        request: request.key.clone(),
        token: request.return_binding.collection_token.clone(),
        ack: vec![],
    };
    let changes = store.connection.total_changes();
    assert!(store
        .collect_answers(&request.key.origin_node, &query, 10_000)
        .unwrap()
        .is_empty());
    assert_eq!(store.connection.total_changes(), changes);
    let mut bad = envelope();
    bad.request_key = Some(request.key.clone());
    let mut good = envelope();
    good.request_key = Some(request.key.clone());
    for item in [&bad, &good] {
        store
            .accept(item, CUSTODY_TTL_MS, Admission::Custody, 10_000)
            .unwrap();
    }
    store
        .connection
        .execute(
            "UPDATE envelopes SET metadata='broken' WHERE id=?1",
            [&bad.key.message_id],
        )
        .unwrap();
    let answers = store
        .collect_answers(&request.key.origin_node, &query, 20_000)
        .unwrap();
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].envelope.key, good.key);
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT state FROM envelopes WHERE id=?1",
                [&bad.key.message_id],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "quarantined"
    );
    assert_eq!(
        answers[0].remaining_ms,
        CUSTODY_TTL_MS - 10_000,
        "TTL advances without a clock commit"
    );
    let plan:String = store.connection.query_row(
        "EXPLAIN QUERY PLAN SELECT origin,id FROM envelopes WHERE request_origin=?1 AND request_id=?2",
        params![request.key.origin_node,request.key.message_id],|r|r.get(3)).unwrap();
    assert!(plan.contains("request_answers"), "{plan}");
}

#[test]
fn collection_quarantines_bad_requests_and_bounds_each_peer_and_import_failures() {
    let f = Fixture::new();
    let mut store = f.open(0);
    let bad = envelope();
    let mut good = envelope();
    good.return_binding.recipient_node = "other.example".into();
    let duplicate_peer = envelope();
    for item in [&bad, &good, &duplicate_peer] {
        store
            .accept(item, CUSTODY_TTL_MS, Admission::Custody, 0)
            .unwrap();
        store.finish(&item.key, Outcome::Delivered, 0).unwrap();
    }
    store
        .connection
        .execute(
            "UPDATE envelopes SET collect_at=0 WHERE id=?1",
            [&bad.key.message_id],
        )
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE envelopes SET metadata='broken' WHERE id=?1",
            [&bad.key.message_id],
        )
        .unwrap();
    let claimed = store
        .collect_ready(&good.key.origin_node, 5_000, 16, &[])
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].envelope.key, good.key);
    assert!(store
        .collect_ready(
            &good.key.origin_node,
            6_000,
            16,
            &["receiver.example".into(), "other.example".into()]
        )
        .unwrap()
        .is_empty());
    let before = store.connection.total_changes();
    for _ in 0..5 {
        store
            .collection_failed(&good.key, "mailbox_full", false)
            .unwrap();
    }
    assert_eq!(
        store.connection.total_changes(),
        before,
        "backpressure must not spend the import-failure budget"
    );
    for attempt in 1..=3 {
        store
            .collection_failed(&good.key, "undecodable answer", false)
            .unwrap();
        assert_eq!(
            store
                .collection_error(&good.key.origin_node, &good.correlation_id)
                .unwrap()
                .is_some(),
            attempt == 3
        );
    }
    drop(store);
    let mut store = f.open(500_000);
    let selected = store
        .collect_ready(&good.key.origin_node, 500_000, 16, &[])
        .unwrap();
    assert!(selected.iter().all(|r| r.envelope.key != good.key));
    store
        .collection_failed(&duplicate_peer.key, "invalid reply binding", true)
        .unwrap();
    assert!(store
        .collect_ready(&good.key.origin_node, 900_000, 16, &[])
        .unwrap()
        .is_empty());
}

#[test]
fn waiting_or_reenrollment_restores_fast_polling_without_reopening_final_answers() {
    let f = Fixture::new();
    let mut store = f.open(0);
    let request = envelope();
    store
        .accept(&request, CUSTODY_TTL_MS, Admission::Custody, 0)
        .unwrap();
    store.finish(&request.key, Outcome::Delivered, 0).unwrap();
    store
        .connection
        .execute("UPDATE envelopes SET collect_at=300000", [])
        .unwrap();
    store
        .collect_fast(
            &request.key.origin_node,
            std::slice::from_ref(&request.correlation_id),
            &[],
            1_000,
        )
        .unwrap();
    assert_eq!(
        store
            .collect_ready(&request.key.origin_node, 6_000, 1, &[])
            .unwrap()
            .len(),
        1
    );
    store
        .connection
        .execute("UPDATE envelopes SET collect_at=300000", [])
        .unwrap();
    store
        .collect_fast(
            &request.key.origin_node,
            &[],
            std::slice::from_ref(&request.return_binding.recipient_node),
            7_000,
        )
        .unwrap();
    assert_eq!(
        store
            .collect_ready(&request.key.origin_node, 12_000, 1, &[])
            .unwrap()
            .len(),
        1
    );
    store.set_paused(true, 12_000).unwrap();
    let changes = store.connection.total_changes();
    store
        .collect_fast(
            &request.key.origin_node,
            &[request.correlation_id],
            &[],
            500_000,
        )
        .unwrap();
    assert!(store
        .collect_ready(&request.key.origin_node, 500_000, 1, &[])
        .unwrap()
        .is_empty());
    assert_eq!(store.connection.total_changes(), changes);
}

#[test]
fn held_answers_do_not_lease_or_starve_outbox_and_idle_ticks_do_not_commit() {
    let f = Fixture::new();
    let mut store = f.open(0);
    let request = envelope();
    for _ in 0..130 {
        let mut answer = envelope();
        answer.request_key = Some(request.key.clone());
        store
            .accept(&answer, CUSTODY_TTL_MS, Admission::Held, 0)
            .unwrap();
    }
    assert_eq!(
        store
            .connection
            .query_row("SELECT active FROM usage", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        130
    );
    let before = store.connection.total_changes();
    for now in (0..600_000).step_by(1_000) {
        assert!(store.retry_ready_limit(now, 1).unwrap().is_empty());
        assert_eq!(
            store
                .activate_held(&request.key.origin_node, &["absent.example".into()], now)
                .unwrap(),
            0
        );
        assert_eq!(store.maintain_if_due(now).unwrap(), 0);
    }
    assert_eq!(
        store.connection.total_changes(),
        before,
        "idle held rows must not write leases or clock ticks"
    );
    let outbox = envelope();
    store
        .accept(&outbox, CUSTODY_TTL_MS, Admission::Custody, 600_000)
        .unwrap();
    assert_eq!(
        store.retry_ready_limit(600_000, 1).unwrap(),
        vec![outbox.key]
    );
    assert_eq!(
        store
            .activate_held(
                &request.key.origin_node,
                &[request.return_binding.recipient_node],
                600_000
            )
            .unwrap(),
        130
    );
    assert_eq!(store.retry_ready_limit(600_000, 500).unwrap().len(), 130);
}

#[test]
fn held_answers_remain_collectable_after_a_final_answer_and_expire_normally() {
    let f = Fixture::new();
    let mut store = f.open(0);
    let request = envelope();
    store
        .accept(&request, CUSTODY_TTL_MS, Admission::Inbox, 0)
        .unwrap();
    let mut query = crate::mesh::collect::AnswerCollect {
        request: request.key.clone(),
        token: request.return_binding.collection_token.clone(),
        ack: vec![],
    };
    let mut answer = envelope();
    answer.request_key = Some(request.key.clone());
    store
        .accept(&answer, CUSTODY_TTL_MS, Admission::Held, 0)
        .unwrap();
    assert_eq!(
        store
            .collect_answers(&request.key.origin_node, &query, 1)
            .unwrap()
            .len(),
        1
    );
    query.ack.push(answer.key.clone());
    assert!(store
        .collect_answers(&request.key.origin_node, &query, 2)
        .unwrap()
        .is_empty());
    store
        .connection
        .execute(
            "UPDATE envelopes SET collect_done=1 WHERE origin=?1 AND id=?2",
            params![request.key.origin_node, request.key.message_id],
        )
        .unwrap();
    let mut later = envelope();
    later.request_key = Some(request.key.clone());
    store
        .accept(&later, CUSTODY_TTL_MS, Admission::Held, 3)
        .unwrap();
    query.ack.clear();
    let collected = store
        .collect_answers(&request.key.origin_node, &query, 4)
        .unwrap();
    assert_eq!(collected.len(), 1);
    assert_eq!(collected[0].envelope.key, later.key);
    store.maintain(CUSTODY_TTL_MS + 3).unwrap();
    assert_eq!(store.get(&later.key).unwrap().unwrap().state, "expired");
    assert_eq!(
        store
            .connection
            .query_row("SELECT active FROM usage", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn schema_nine_held_answers_migrate_out_of_the_retry_queue() {
    let f = Fixture::new();
    let mut store = f.open(0);
    let request = envelope();
    let mut answer = envelope();
    answer.request_key = Some(request.key.clone());
    store
        .accept(&answer, CUSTODY_TTL_MS, Admission::Custody, 0)
        .unwrap();
    store
        .connection
        .execute_batch("DROP INDEX held_recipient; ALTER TABLE envelopes DROP COLUMN remote_state; ALTER TABLE envelopes DROP COLUMN receipt_sent; DROP TABLE delivery_attempts; DROP TABLE status_signer; DROP INDEX status_correlation; PRAGMA user_version=9;")
        .unwrap();
    drop(store);
    let mut store = f.open(1);
    assert_eq!(store.get(&answer.key).unwrap().unwrap().state, "held");
    assert!(store.retry_ready_limit(1, 500).unwrap().is_empty());
    assert_eq!(
        store
            .connection
            .query_row("SELECT active FROM usage", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn collection_backoff_batch_rolls_back_together() {
    let f = Fixture::new();
    let mut store = f.open(0);
    let first = envelope();
    let mut second = envelope();
    second.return_binding.recipient_node = "second.example".into();
    for item in [&first, &second] {
        store
            .accept(item, CUSTODY_TTL_MS, Admission::Custody, 0)
            .unwrap();
        store.finish(&item.key, Outcome::Delivered, 0).unwrap();
    }
    store
        .connection
        .execute(
            "UPDATE envelopes SET collect_at=0 WHERE id=?1",
            [&first.key.message_id],
        )
        .unwrap();
    store
        .connection
        .execute_batch(
            "CREATE TRIGGER fail_second_backoff BEFORE UPDATE OF collect_attempts ON envelopes
        WHEN NEW.recipient_node='second.example' AND NEW.collect_attempts>0
        BEGIN SELECT RAISE(ABORT,'second backoff fails'); END;",
        )
        .unwrap();
    assert!(store
        .collect_ready(&first.key.origin_node, 5_000, 16, &[])
        .is_err());
    let first_backoff: (i64, i64) = store
        .connection
        .query_row(
            "SELECT collect_at,collect_attempts FROM envelopes WHERE id=?1",
            [&first.key.message_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        first_backoff,
        (0, 0),
        "the first update must roll back with the failed second update"
    );
}

#[test]
fn spoke_custody_collection_scopes_outbox_and_validates_entire_ack_batch() {
    use crate::mesh::collect::{OutboundAck, OutboundCollect};
    let fixture = Fixture::new();
    let mut store = fixture.open(0);
    let mail = envelope();
    let mut other = envelope();
    other.return_binding.recipient_node = "other.example".into();
    store
        .accept(&mail, CUSTODY_TTL_MS, Admission::Held, 0)
        .unwrap();
    store
        .accept(&other, CUSTODY_TTL_MS, Admission::Held, 0)
        .unwrap();
    let poll = OutboundCollect::default();
    let batch = store
        .collect_outbound(
            Offer::Step1RequestsOnly,
            "origin.example",
            "receiver.example",
            &poll,
            1,
        )
        .unwrap();
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].envelope, mail);
    drop(store);
    let mut store = fixture.open(2);
    assert_eq!(
        store
            .collect_outbound(
                Offer::Step1RequestsOnly,
                "origin.example",
                "receiver.example",
                &poll,
                5002
            )
            .unwrap()[0]
            .envelope,
        mail
    );
    let good = OutboundAck {
        key: mail.key.clone(),
        token: mail.return_binding.collection_token.clone(),
        refusal: None,
    };
    let bad = OutboundAck {
        key: other.key.clone(),
        token: other.return_binding.collection_token.clone(),
        refusal: None,
    };
    assert!(store
        .collect_outbound(
            Offer::Step1RequestsOnly,
            "origin.example",
            "receiver.example",
            &OutboundCollect {
                receipts: Vec::new(),
                ack: vec![good.clone(), bad]
            },
            3
        )
        .is_err());
    assert_eq!(store.get(&mail.key).unwrap().unwrap().state, "held");
    let mut forged = good.clone();
    forged.token[0] ^= 1;
    assert!(store
        .collect_outbound(
            Offer::Step1RequestsOnly,
            "origin.example",
            "receiver.example",
            &OutboundCollect {
                ack: vec![forged],
                receipts: Vec::new()
            },
            3
        )
        .is_err());
    let ack = OutboundCollect {
        ack: vec![good],
        receipts: Vec::new(),
    };
    assert!(store
        .collect_outbound(
            Offer::Step1RequestsOnly,
            "origin.example",
            "receiver.example",
            &ack,
            4
        )
        .unwrap()
        .is_empty());
    assert!(store
        .collect_outbound(
            Offer::Step1RequestsOnly,
            "origin.example",
            "receiver.example",
            &ack,
            5
        )
        .unwrap()
        .is_empty());
    assert_eq!(store.get(&mail.key).unwrap().unwrap().state, "delivered");
    assert_eq!(store.get(&other.key).unwrap().unwrap().state, "held");
}

#[test]
fn spoke_custody_duplicates_do_not_write_and_conflicts_still_fail() {
    let fixture = Fixture::new();
    let mut store = fixture.open(0);
    let mut mail = envelope();
    store
        .accept(&mail, CUSTODY_TTL_MS, Admission::Inbox, 0)
        .unwrap();
    let observer = rusqlite::Connection::open(&fixture.path).unwrap();
    let version = || {
        observer
            .query_row("PRAGMA data_version", [], |r| r.get::<_, i64>(0))
            .unwrap()
    };
    let before = version();
    let changes = store.connection.total_changes();
    for now in 1..20 {
        assert_eq!(
            store
                .accept(&mail, CUSTODY_TTL_MS - now, Admission::Inbox, now)
                .unwrap(),
            Accepted::Duplicate
        );
    }
    mail.body.push(0);
    assert!(matches!(
        store.accept(&mail, CUSTODY_TTL_MS, Admission::Inbox, 20),
        Err(Error::ConflictingKey)
    ));
    assert_eq!(changes, store.connection.total_changes());
    assert_eq!(before, version());
}

#[test]
fn spoke_custody_backoff_survives_restart_and_cannot_starve_later_rows() {
    use crate::mesh::collect::{OutboundCollect, BATCH_CAP};
    let fixture = Fixture::new();
    let mut store = fixture.open(0);
    for _ in 0..(BATCH_CAP + 5) {
        store
            .accept(&envelope(), CUSTODY_TTL_MS, Admission::Held, 0)
            .unwrap();
    }
    let first = store
        .collect_outbound(
            Offer::Step1RequestsOnly,
            "origin.example",
            "receiver.example",
            &OutboundCollect::default(),
            1,
        )
        .unwrap();
    assert_eq!(first.len(), BATCH_CAP);
    drop(store);
    let mut store = fixture.open(2);
    let rest = store
        .collect_outbound(
            Offer::Step1RequestsOnly,
            "origin.example",
            "receiver.example",
            &OutboundCollect::default(),
            2,
        )
        .unwrap();
    assert_eq!(rest.len(), 5);
    assert!(rest
        .iter()
        .all(|r| !first.iter().any(|f| f.envelope.key == r.envelope.key)));
    assert!(!store
        .has_outbound("origin.example", "receiver.example", 3)
        .unwrap());
    assert!(!store
        .has_outbound("origin.example", "other.example", 5002)
        .unwrap());
    assert!(store
        .has_outbound("origin.example", "receiver.example", 5002)
        .unwrap());
}

#[test]
fn status_reads_are_pure_and_survive_restart_handoff_and_retention() {
    let fixture = Fixture::new();
    let request = envelope();
    let mut store = fixture.open(0);
    store
        .accept(&request, CUSTODY_TTL_MS, Admission::Custody, 0)
        .unwrap();
    let changes = store.connection.total_changes();
    let queued = store
        .status("origin.example", "thread", 1)
        .unwrap()
        .unwrap();
    assert_eq!(queued.state, "queued");
    assert_eq!(store.connection.total_changes(), changes);
    store.finish(&request.key, Outcome::Delivered, 2).unwrap();
    store.import_receipt(&request.key, "read").unwrap();
    let generation = store.handoff_generation().unwrap();
    drop(store);
    Store::check_generation(&fixture.path, generation).unwrap();
    let mut store = fixture.open(3);
    assert_eq!(
        store
            .referenced_status("thread", &queued.reference, 3)
            .unwrap()
            .state,
        "read"
    );
    let changes = store.connection.total_changes();
    assert_eq!(
        store
            .status("origin.example", "thread", CUSTODY_TTL_MS + 3)
            .unwrap()
            .unwrap()
            .state,
        "outcome_retention_elapsed"
    );
    assert_eq!(store.connection.total_changes(), changes);
    store.maintain(CUSTODY_TTL_MS + 2 * DAY_MS).unwrap();
    assert!(store
        .status("origin.example", "thread", CUSTODY_TTL_MS + 2 * DAY_MS)
        .unwrap()
        .is_none());
    assert_eq!(
        store
            .referenced_status("thread", &queued.reference, CUSTODY_TTL_MS + 2 * DAY_MS)
            .unwrap()
            .state,
        "outcome_retention_elapsed"
    );
    let mut forged = queued.reference;
    forged.key.message_id = MessageKey::mint("origin.example".into(), 0)
        .unwrap()
        .message_id;
    assert!(store.referenced_status("thread", &forged, 0).is_err());
}

#[test]
fn status_reports_held_collected_custody_and_expiry_without_commits() {
    let fixture = Fixture::new();
    let mut store = fixture.open(0);
    let request = envelope();
    store
        .accept(&request, CUSTODY_TTL_MS, Admission::Custody, 0)
        .unwrap();
    store.finish(&request.key, Outcome::Transferred, 1).unwrap();
    assert_eq!(
        store
            .status("origin.example", "thread", 1)
            .unwrap()
            .unwrap()
            .state,
        "custody"
    );
    let mut answer = envelope();
    answer.correlation_id = "answer".into();
    answer.request_key = Some(request.key.clone());
    answer.in_reply_to = Some("thread".into());
    store
        .accept(&answer, CUSTODY_TTL_MS, Admission::Held, 2)
        .unwrap();
    assert_eq!(
        store
            .status("origin.example", "answer", 2)
            .unwrap()
            .unwrap()
            .state,
        "held"
    );
    store.finish(&answer.key, Outcome::Delivered, 3).unwrap();
    assert_eq!(
        store
            .status("origin.example", "answer", 3)
            .unwrap()
            .unwrap()
            .state,
        "collected"
    );
    let mut expired = envelope();
    expired.correlation_id = "expired".into();
    store.accept(&expired, 10, Admission::Custody, 3).unwrap();
    let changes = store.connection.total_changes();
    assert_eq!(
        store
            .status("origin.example", "expired", 14)
            .unwrap()
            .unwrap()
            .state,
        "expired"
    );
    assert_eq!(store.connection.total_changes(), changes);
}

#[test]
fn status_selects_real_imported_reply_after_deferral_without_audit_events() {
    let fixture = Fixture::new();
    let mut store = fixture.open(0);
    let request = envelope();
    store
        .accept(&request, CUSTODY_TTL_MS, Admission::Custody, 0)
        .unwrap();
    for correlation in ["thread:deferred", "real-answer"] {
        let mut answer = envelope();
        answer.correlation_id = correlation.into();
        answer.request_key = Some(request.key.clone());
        answer.in_reply_to = Some("thread".into());
        answer.body =
            serde_json::to_vec(&serde_json::json!({"message":{"body":correlation}})).unwrap();
        store
            .accept(&answer, CUSTODY_TTL_MS, Admission::Inbox, 1)
            .unwrap();
    }
    let changes = store.connection.total_changes();
    let status = store
        .status("origin.example", "thread", 2)
        .unwrap()
        .unwrap();
    assert_eq!(status.reply.unwrap().body, "real-answer");
    assert_eq!(store.connection.total_changes(), changes);
}

#[test]
fn receipt_collection_is_repeatable_until_ack_and_read_advances_it() {
    let fixture = Fixture::new();
    let mut store = fixture.open(0);
    let request = envelope();
    store
        .accept(&request, CUSTODY_TTL_MS, Admission::Inbox, 0)
        .unwrap();
    let receipts = store.pending_receipts(&request.key.origin_node, 1).unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].state, "delivered");
    assert_eq!(
        store.pending_receipts(&request.key.origin_node, 1).unwrap(),
        receipts
    );
    store.receipts_sent(&receipts).unwrap();
    assert!(store
        .pending_receipts(&request.key.origin_node, 1)
        .unwrap()
        .is_empty());
    store
        .read_inbox(std::slice::from_ref(&request.key), 2)
        .unwrap();
    let receipts = store.pending_receipts(&request.key.origin_node, 2).unwrap();
    assert_eq!(receipts[0].state, "read");
    drop(store);
    assert_eq!(
        fixture
            .open(3)
            .pending_receipts(&request.key.origin_node, 3)
            .unwrap(),
        receipts
    );
}

#[test]
fn a_delivered_notice_is_polled_until_a_terminal_read_receipt() {
    let f = Fixture::new();
    let mut store = f.open(0);
    let mut notice = envelope();
    notice.intent = "\"fyi\"".into();
    store
        .accept(&notice, CUSTODY_TTL_MS, Admission::Custody, 0)
        .unwrap();
    store.finish(&notice.key, Outcome::Delivered, 0).unwrap();
    let claimed = store
        .collect_ready(&notice.key.origin_node, 5_000, 16, &[])
        .unwrap();
    assert_eq!(claimed.len(), 1, "an unconfirmed notice awaits its receipt");
    store.import_receipt(&notice.key, "delivered").unwrap();
    assert_eq!(
        store
            .status("origin.example", "thread", 5_001)
            .unwrap()
            .unwrap()
            .state,
        "delivered"
    );
    assert_eq!(
        store
            .collect_ready(&notice.key.origin_node, 400_000, 16, &[])
            .unwrap()
            .len(),
        1,
        "a delivered receipt is not terminal"
    );
    store.import_receipt(&notice.key, "read").unwrap();
    assert_eq!(
        store
            .status("origin.example", "thread", 400_001)
            .unwrap()
            .unwrap()
            .state,
        "read"
    );
    assert!(store
        .collect_ready(&notice.key.origin_node, 900_000, 16, &[])
        .unwrap()
        .is_empty());
    store.import_receipt(&notice.key, "delivered").unwrap();
    assert_eq!(
        store
            .status("origin.example", "thread", 900_001)
            .unwrap()
            .unwrap()
            .state,
        "read",
        "a late receipt never moves a message backwards"
    );
}

#[test]
fn outbound_receipts_are_judged_per_record_and_never_fail_the_batch() {
    use crate::mesh::collect::{OutboundCollect, Receipt};
    let fixture = Fixture::new();
    let mut store = fixture.open(0);
    let mail = envelope();
    store
        .accept(&mail, CUSTODY_TTL_MS, Admission::Custody, 0)
        .unwrap();
    store.finish(&mail.key, Outcome::Delivered, 1).unwrap();
    let good = Receipt {
        key: mail.key.clone(),
        token: mail.return_binding.collection_token.clone(),
        state: "read".into(),
    };
    let mut pruned = good.clone();
    pruned.key = MessageKey::mint("origin.example".into(), 2).unwrap();
    let mut forged = good.clone();
    forged.token[0] ^= 1;
    forged.state = "expired".into();
    let query = OutboundCollect {
        receipts: vec![pruned, forged, good],
        ack: Vec::new(),
    };
    store
        .collect_outbound(
            Offer::Step1RequestsOnly,
            "origin.example",
            "receiver.example",
            &query,
            3,
        )
        .expect("a pruned or forged receipt drops only itself");
    assert_eq!(
        store
            .status("origin.example", "thread", 4)
            .unwrap()
            .unwrap()
            .state,
        "read"
    );
}

#[test]
fn every_receipt_state_is_markable_so_none_hogs_the_window() {
    let fixture = Fixture::new();
    let mut store = fixture.open(0);
    let stale = envelope();
    store
        .accept(&stale, CUSTODY_TTL_MS, Admission::Inbox, 0)
        .unwrap();
    // Past its inbox deadline but not yet swept: status says expired while
    // the row is still `inbox`. That receipt must be markable too.
    let late = DAY_MS + 1;
    let receipts = store
        .pending_receipts(&stale.key.origin_node, late)
        .unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].state, "expired");
    store.receipts_sent(&receipts).unwrap();
    assert!(store
        .pending_receipts(&stale.key.origin_node, late)
        .unwrap()
        .is_empty());
    // A full window of fresh rows behind it is not starved.
    let mut fresh = Vec::new();
    for _ in 0..crate::mesh::collect::BATCH_CAP + 1 {
        let mut mail = envelope();
        mail.key = MessageKey::mint("origin.example".into(), late as u64).unwrap();
        mail.return_binding.request = mail.key.clone();
        store
            .accept(&mail, CUSTODY_TTL_MS, Admission::Inbox, late)
            .unwrap();
        fresh.push(mail.key);
    }
    let window = store
        .pending_receipts(&stale.key.origin_node, late)
        .unwrap();
    assert_eq!(window.len(), crate::mesh::collect::BATCH_CAP);
    assert!(window.iter().all(|r| r.key != stale.key));
    store.receipts_sent(&window).unwrap();
    assert_eq!(
        store
            .pending_receipts(&stale.key.origin_node, late)
            .unwrap()
            .len(),
        1,
        "the row past the first window is offered next"
    );
}

#[test]
fn a_receipt_for_a_still_held_row_survives_its_lost_ack() {
    use crate::mesh::collect::{OutboundAck, OutboundCollect, Receipt};
    let fixture = Fixture::new();
    let mut store = fixture.open(0);
    let mail = envelope();
    store
        .accept(&mail, CUSTODY_TTL_MS, Admission::Held, 0)
        .unwrap();
    let token = mail.return_binding.collection_token.clone();
    let receipt = Receipt {
        key: mail.key.clone(),
        token: token.clone(),
        state: "read".into(),
    };
    store
        .collect_outbound(
            Offer::Step1RequestsOnly,
            "origin.example",
            "receiver.example",
            &OutboundCollect {
                receipts: vec![receipt],
                ack: Vec::new(),
            },
            1,
        )
        .unwrap();
    let ack = OutboundAck {
        key: mail.key.clone(),
        token,
        refusal: None,
    };
    store
        .collect_outbound(
            Offer::Step1RequestsOnly,
            "origin.example",
            "receiver.example",
            &OutboundCollect {
                receipts: Vec::new(),
                ack: vec![ack],
            },
            2,
        )
        .unwrap();
    assert_eq!(
        store
            .status("origin.example", "thread", 3)
            .unwrap()
            .unwrap()
            .state,
        "read",
        "the receipt that outran its ack is kept"
    );
}

#[test]
fn status_by_correlation_is_scoped_to_the_asking_origin() {
    let fixture = Fixture::new();
    let mut store = fixture.open(0);
    let own = envelope();
    store
        .accept(&own, CUSTODY_TTL_MS, Admission::Custody, 0)
        .unwrap();
    let mut foreign = envelope();
    foreign.key = MessageKey::mint("foreign.example".into(), 1).unwrap();
    foreign.return_binding.request = foreign.key.clone();
    store
        .accept(&foreign, CUSTODY_TTL_MS, Admission::Inbox, 1)
        .unwrap();
    let status = store
        .status("origin.example", "thread", 2)
        .unwrap()
        .unwrap();
    assert_eq!(status.reference.key, own.key);
    assert_eq!(status.state, "queued");
    assert!(store
        .status("elsewhere.example", "thread", 2)
        .unwrap()
        .is_none());
}

#[test]
fn a_delivery_attempt_write_stays_keyed_with_a_full_registry() {
    let fixture = Fixture::new();
    let mut store = fixture.open(0);
    let mail = envelope();
    store
        .accept(&mail, CUSTODY_TTL_MS, Admission::Inbox, 0)
        .unwrap();
    let cap = crate::app::mailboxes::MAX_SEEN;
    let attempt = |id: usize, state: &str, finished: bool| crate::api::schema::DeliveryAttempt {
        attempt_id: format!("attempt:{id:020}"),
        pane: "fixture-pane".into(),
        correlation_ids: vec!["thread".into()],
        wake: true,
        state: state.into(),
        reason: None,
        queued_at_ms: id as u64,
        typed_at_ms: None,
        submit_sent_at_ms: None,
        finished_at_ms: finished.then_some(id as u64),
        retried: false,
    };
    // Seed a full registry directly: the oldest is pending, the next is
    // unconfirmed while its message still sits in the inbox, the rest done.
    let tx = store.connection.transaction().unwrap();
    for id in 0..cap {
        let row = match id {
            0 => attempt(id, "typed", false),
            1 => attempt(id, "unconfirmed", true),
            _ => attempt(id, "accepted", true),
        };
        tx.execute(
            "INSERT INTO delivery_attempts VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                row.attempt_id,
                serde_json::to_string(&row).unwrap(),
                row.queued_at_ms as i64,
                row.finished_at_ms.is_some(),
                row.state,
                serde_json::to_string(&row.correlation_ids).unwrap()
            ],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    let started = std::time::Instant::now();
    assert!(store
        .record_attempt(&attempt(cap, "queued", false))
        .unwrap());
    let elapsed = started.elapsed();
    let ids: Vec<String> = store
        .connection
        .prepare("SELECT id FROM delivery_attempts ORDER BY id LIMIT 3")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(
        ids,
        [0, 1, 3].map(|id| format!("attempt:{id:020}")),
        "only the oldest evictable row goes; pending and protected stay"
    );
    let retained: i64 = store
        .connection
        .query_row("SELECT count(*) FROM delivery_attempts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(retained as usize, cap);
    // An update to an existing attempt is one keyed upsert, no eviction.
    let before = store.connection.total_changes();
    assert!(store.record_attempt(&attempt(cap, "typed", false)).unwrap());
    assert_eq!(store.connection.total_changes() - before, 1);
    // Generous for a debug build on a loaded runner, far below what parsing
    // and rewriting 4096 rows per write cost before this was keyed.
    assert!(
        elapsed < std::time::Duration::from_millis(250),
        "one attempt write took {elapsed:?}"
    );
}

#[test]
fn spoke_outbound_leases_are_atomic_and_idle_polls_do_not_commit() {
    use crate::mesh::collect::OutboundCollect;
    let fixture = Fixture::new();
    let mut store = fixture.open(0);
    let first = envelope();
    let second = envelope();
    for mail in [&first, &second] {
        store
            .accept(mail, CUSTODY_TTL_MS, Admission::Held, 0)
            .unwrap();
    }
    store
        .connection
        .execute_batch(
            "CREATE TRIGGER fail_second_lease BEFORE UPDATE OF collect_attempts ON envelopes
         WHEN (SELECT SUM(collect_attempts) FROM envelopes)>0
         BEGIN SELECT RAISE(ABORT,'second lease fails'); END;",
        )
        .unwrap();
    let poll = OutboundCollect::default();
    assert!(store
        .collect_outbound(
            Offer::Step1RequestsOnly,
            "origin.example",
            "receiver.example",
            &poll,
            1
        )
        .is_err());
    let attempts: i64 = store
        .connection
        .query_row("SELECT SUM(collect_attempts) FROM envelopes", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(attempts, 0, "the first lease must roll back too");
    store
        .connection
        .execute_batch("DROP TRIGGER fail_second_lease")
        .unwrap();
    assert_eq!(
        store
            .collect_outbound(
                Offer::Step1RequestsOnly,
                "origin.example",
                "receiver.example",
                &poll,
                1
            )
            .unwrap()
            .len(),
        2
    );
    let observer = Connection::open(&fixture.path).unwrap();
    let version = || {
        observer
            .query_row("PRAGMA data_version", [], |r| r.get::<_, i64>(0))
            .unwrap()
    };
    let before = version();
    assert!(store
        .collect_outbound(
            Offer::Step1RequestsOnly,
            "origin.example",
            "receiver.example",
            &poll,
            2
        )
        .unwrap()
        .is_empty());
    assert_eq!(version(), before);
}

#[test]
fn spoke_outbound_missing_ack_does_not_reject_live_ack() {
    use crate::mesh::collect::{OutboundAck, OutboundCollect};
    let fixture = Fixture::new();
    let mut store = fixture.open(0);
    let expired = envelope();
    let live = envelope();
    for mail in [&expired, &live] {
        store
            .accept(mail, CUSTODY_TTL_MS, Admission::Held, 0)
            .unwrap();
    }
    store
        .collect_outbound(
            Offer::Step1RequestsOnly,
            "origin.example",
            "receiver.example",
            &OutboundCollect::default(),
            1,
        )
        .unwrap();
    store
        .connection
        .execute(
            "DELETE FROM envelopes WHERE id=?1",
            [&expired.key.message_id],
        )
        .unwrap();
    let query = OutboundCollect {
        ack: [&expired, &live]
            .into_iter()
            .map(|mail| OutboundAck {
                key: mail.key.clone(),
                token: mail.return_binding.collection_token.clone(),
                refusal: None,
            })
            .collect(),
        receipts: Vec::new(),
    };
    assert!(store
        .collect_outbound(
            Offer::Step1RequestsOnly,
            "origin.example",
            "receiver.example",
            &query,
            2
        )
        .unwrap()
        .is_empty());
    assert_eq!(store.get(&live.key).unwrap().unwrap().state, "delivered");
    assert!(store
        .collect_outbound(
            Offer::Step1RequestsOnly,
            "origin.example",
            "receiver.example",
            &query,
            3
        )
        .unwrap()
        .is_empty());
}

#[test]
fn v11_db_with_in_place_edited_columns_migrates_idempotently() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let e = envelope();
    s.accept(&e, 1000, Admission::Held, 0).unwrap();
    s.connection
        .execute_batch("PRAGMA user_version=11")
        .unwrap();
    drop(s);
    let mut s = f.open(1);
    assert_eq!(
        s.get(&e.key).unwrap().unwrap().next_hop,
        e.return_binding.recipient_node
    );
    assert_eq!(
        s.accept(&e, 999, Admission::Held, 1).unwrap(),
        Accepted::Duplicate
    );
    drop(s);
    assert_eq!(f.open(2).get(&e.key).unwrap().unwrap().remaining_ms, 998);
}

#[test]
fn fresh_and_upgraded_schemas_are_identical() {
    fn schema(s: &Store<Disk>) -> Vec<(String, String)> {
        s.connection
            .prepare("SELECT name,sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY name")
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?
                        .split_whitespace()
                        .collect::<String>()
                        .replace('"', ""),
                ))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }
    let fresh = Fixture::new();
    let old = Fixture::new();
    Connection::open(&old.path)
        .unwrap()
        .execute_batch(schema::BASE)
        .unwrap();
    assert_eq!(schema(&fresh.open(0)), schema(&old.open(0)));
}

#[test]
fn failed_migration_reports_path_and_hint() {
    let f = Fixture::new();
    Connection::open(&f.path)
        .unwrap()
        .execute_batch("CREATE TABLE envelopes(origin TEXT); PRAGMA user_version=11")
        .unwrap();
    let error = Store::open_with(&f.path, 0, Limits::default(), f.disk.clone())
        .err()
        .unwrap()
        .to_string();
    assert!(
        error.contains(&format!(
            "mail_store_unavailable: migrating {} (schema v11 -> v12) failed:",
            f.path.display()
        )),
        "{error}"
    );
    assert!(error.contains("missing column envelopes.id"), "{error}");
    assert!(
        error.ends_with("move the file aside to start an empty store (custody in it will be lost)")
    );
    assert_eq!(
        Connection::open(&f.path)
            .unwrap()
            .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        11
    );
}

#[test]
fn fingerprint_ignores_signature() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let mut e = envelope();
    s.accept(&e, 1000, Admission::Custody, 0).unwrap();
    e.signature = vec![7; 64];
    assert_eq!(
        s.accept(&e, 1000, Admission::Custody, 1).unwrap(),
        Accepted::Duplicate
    );
    e.kind = Kind::Receipt;
    assert!(matches!(
        s.accept(&e, 1000, Admission::Custody, 1),
        Err(Error::ConflictingKey)
    ));
}

#[test]
fn push_ready_selects_only_pushable_next_hops_and_leases() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let a = envelope();
    let b = envelope();
    for e in [&a, &b] {
        s.accept(e, CUSTODY_TTL_MS, Admission::Custody, 0).unwrap();
    }
    s.set_next_hop(&a.key, "push.example").unwrap();
    s.set_next_hop(&b.key, "pull.example").unwrap();
    let nodes = vec!["push.example".into()];
    assert_eq!(s.push_ready(0, 10, &nodes).unwrap(), vec![a.key.clone()]);
    assert!(s.push_ready(1, 10, &nodes).unwrap().is_empty());
    drop(s);
    assert_eq!(
        f.open(60_000).push_ready(60_000, 10, &nodes).unwrap(),
        vec![a.key]
    );
}

#[test]
fn unrouted_rows_are_never_leased() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let e = envelope();
    s.accept(&e, 1000, Admission::Custody, 0).unwrap();
    s.set_next_hop(&e.key, "").unwrap();
    let before = s.connection.total_changes();
    assert_eq!(s.unrouted(10).unwrap(), vec![e.key]);
    assert!(s.push_ready(1, 10, &[String::new()]).unwrap().is_empty());
    assert_eq!(s.connection.total_changes(), before);
}

#[test]
fn collect_outbound_offers_forwarded_answers_and_receipts_to_next_hop_only() {
    use crate::mesh::collect::OutboundCollect;
    let f = Fixture::new();
    let mut s = f.open(0);
    let mut answer = envelope();
    answer.request_key = Some(envelope().key);
    let mut receipt = envelope();
    receipt.kind = Kind::Receipt;
    for e in [&answer, &receipt] {
        s.accept(e, 1000, Admission::Held, 0).unwrap();
        s.set_next_hop(&e.key, "neighbor.example").unwrap();
    }
    let query = OutboundCollect::default();
    assert!(s
        .collect_outbound(Offer::All, "local.example", "receiver.example", &query, 0)
        .unwrap()
        .is_empty());
    let offered = s
        .collect_outbound(Offer::All, "local.example", "neighbor.example", &query, 0)
        .unwrap();
    assert_eq!(offered.len(), 2);
    assert!(offered.iter().any(|d| d.envelope == answer));
    assert!(offered.iter().any(|d| d.envelope == receipt));
}

#[test]
fn ack_from_wrong_neighbor_is_invalid() {
    use crate::mesh::collect::{OutboundAck, OutboundCollect};
    let f = Fixture::new();
    let mut s = f.open(0);
    let e = envelope();
    s.accept(&e, 1000, Admission::Held, 0).unwrap();
    s.set_next_hop(&e.key, "neighbor.example").unwrap();
    let query = OutboundCollect {
        ack: vec![OutboundAck {
            key: e.key.clone(),
            token: e.return_binding.collection_token.clone(),
            refusal: None,
        }],
        receipts: Vec::new(),
    };
    assert!(matches!(
        s.collect_outbound(Offer::All, "local.example", "receiver.example", &query, 0),
        Err(Error::InvalidEnvelope)
    ));
    s.collect_outbound(Offer::All, "local.example", "neighbor.example", &query, 0)
        .unwrap();
    assert_eq!(s.get(&e.key).unwrap().unwrap().state, "delivered");
}

#[test]
fn tombstone_finishes_pending_rows_and_refuses_new_inbox_import() {
    let f = Fixture::new();
    let mut s = f.open(0);
    s.set_local_node("receiver.example");
    let mut keys = Vec::new();
    for admission in [Admission::Inbox, Admission::Custody, Admission::Held] {
        let e = envelope();
        s.accept(&e, 1000, admission, 0).unwrap();
        keys.push(e.key);
    }
    let mut remote = envelope();
    remote.return_binding.recipient_node = "other.example".into();
    s.accept(&remote, 1000, Admission::Custody, 0).unwrap();
    let mut other_session = envelope();
    other_session.target_session = "new-session".into();
    s.accept(&other_session, 1000, Admission::Inbox, 0).unwrap();
    let affected = s.tombstone("recipient", "session", "killed", 1).unwrap();
    assert_eq!(affected.len(), 3);
    for key in keys {
        assert!(affected.contains(&key));
        let record = s.get(&key).unwrap().unwrap();
        assert_eq!(record.state, "recipient_gone");
        assert!(record.envelope.body.is_empty());
    }
    assert_eq!(s.get(&remote.key).unwrap().unwrap().state, "custody");
    assert_eq!(s.get(&other_session.key).unwrap().unwrap().state, "inbox");
    assert!(matches!(
        s.accept(&envelope(), 1000, Admission::Inbox, 2),
        Err(Error::RecipientGone)
    ));
}

#[test]
fn tombstone_and_owner_hints_survive_reopen() {
    let f = Fixture::new();
    let mut s = f.open(0);
    s.tombstone("recipient", "session", "closed", 0).unwrap();
    s.note_owner("recipient", "receiver.example", "peer.example", 0)
        .unwrap();
    s.set_paused(true, 1).unwrap();
    drop(s);
    let mut s = f.open(20 * DAY_MS);
    assert!(s
        .is_tombstoned("recipient", "session", 20 * DAY_MS)
        .unwrap());
    assert_eq!(
        s.owner("recipient").unwrap().unwrap().node_id,
        "receiver.example"
    );
    s.set_paused(false, 20 * DAY_MS).unwrap();
    assert!(!s
        .is_tombstoned("recipient", "session", 27 * DAY_MS)
        .unwrap());
}

#[test]
fn note_owner_writes_only_on_change() {
    let f = Fixture::new();
    let mut s = f.open(0);
    assert!(s
        .note_owner("recipient", "receiver.example", "peer.example", 0)
        .unwrap());
    let before = s.connection.total_changes();
    for wall in 1..100 {
        assert!(!s
            .note_owner("recipient", "receiver.example", "peer.example", wall)
            .unwrap());
    }
    assert_eq!(s.connection.total_changes(), before);
    assert!(s
        .note_owner("recipient", "other.example", "peer.example", 100)
        .unwrap());
    assert!(s
        .note_owner("recipient", "other.example", "renamed.example", 101)
        .unwrap());
}

#[test]
fn gc_keeps_quarantined_rows() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let e = envelope();
    s.accept(&e, 1000, Admission::Custody, 0).unwrap();
    s.finish(&e.key, Outcome::Delivered, 1).unwrap();
    s.quarantine(&e.key).unwrap();
    assert_eq!(s.maintain(20 * DAY_MS).unwrap(), 0);
    assert_eq!(s.quarantined_count().unwrap(), 1);
    let before = s.connection.total_changes();
    s.maintain_if_due(21 * DAY_MS).unwrap();
    assert_eq!(s.connection.total_changes(), before);
}

#[test]
fn pending_receipts_limit_applies_after_sql_filter() {
    let f = Fixture::new();
    let mut s = f.open(0);
    for _ in 0..40 {
        s.accept(&envelope(), 1000, Admission::Inbox, 0).unwrap();
    }
    let first = s.pending_receipts("origin.example", 0).unwrap();
    assert_eq!(first.len(), 16);
    s.receipts_sent(&first).unwrap();
    let second = s.pending_receipts("origin.example", 0).unwrap();
    assert_eq!(second.len(), 16);
    assert!(second
        .iter()
        .all(|r| !first.iter().any(|old| old.key == r.key)));
    s.receipts_sent(&second).unwrap();
    assert_eq!(s.pending_receipts("origin.example", 0).unwrap().len(), 8);
}

#[test]
fn open_tightens_loose_modes() {
    let f = Fixture::new();
    let s = f.open(0);
    let parent = f.path.parent().unwrap();
    let wal = f.path.with_extension("sqlite-wal");
    let shm = f.path.with_extension("sqlite-shm");
    for path in [&f.path, &wal, &shm] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o666)).unwrap();
    }
    fs::set_permissions(parent, fs::Permissions::from_mode(0o777)).unwrap();
    let _second = f.open(0);
    for path in [&f.path, &wal, &shm] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert_eq!(
        fs::metadata(parent).unwrap().permissions().mode() & 0o777,
        0o700
    );
    drop(s);
}

#[test]
fn unknown_error_is_transient() {
    for error in [
        Error::InvalidEnvelope,
        Error::ConflictingKey,
        Error::InvalidSignature,
        Error::OriginMismatch,
        Error::RecipientGone,
        Error::LoopDetected,
        Error::HopBudgetExhausted,
    ] {
        assert!(error.is_permanent());
    }
    for error in [
        Error::InvalidState,
        Error::NotFound,
        Error::Paused,
        Error::MailStoreFull,
        Error::Io(std::io::Error::other("unknown future failure")),
    ] {
        assert!(!error.is_permanent());
    }
}

#[test]
fn idle_step2_queries_make_no_commits() {
    use crate::mesh::collect::OutboundCollect;
    let f = Fixture::new();
    let mut s = f.open(0);
    s.checkpoint().unwrap();
    let before = s.connection.total_changes();
    let wal = f.path.with_extension("sqlite-wal");
    let size = fs::metadata(&wal).unwrap().len();
    for now in 0..100 {
        assert!(s
            .push_ready(now, 16, &["receiver.example".into()])
            .unwrap()
            .is_empty());
        assert!(s.unrouted(16).unwrap().is_empty());
        assert!(s
            .collect_outbound(
                Offer::All,
                "origin.example",
                "receiver.example",
                &OutboundCollect::default(),
                now
            )
            .unwrap()
            .is_empty());
    }
    assert_eq!(s.connection.total_changes(), before);
    assert_eq!(fs::metadata(wal).unwrap().len(), size);
}

#[test]
fn forwarding_verifies_origin_and_duplicate_preserves_transport_state() {
    let f = Fixture::new();
    let mut s = f.open(0);
    s.set_local_node("local.example");
    let e = crate::mesh::sign::tests::signed();
    let visited = vec![e.key.origin_node.clone()];
    assert_eq!(
        s.accept_forward(
            &e,
            1000,
            7,
            &visited,
            "neighbor.example",
            Admission::Custody,
            0
        )
        .unwrap(),
        Accepted::New
    );
    assert_eq!(
        s.accept_forward(&e, 2000, 6, &[], "other.example", Admission::Custody, 10)
            .unwrap(),
        Accepted::Duplicate
    );
    let record = s.get(&e.key).unwrap().unwrap();
    assert_eq!(record.remaining_ms, 1000);
    assert_eq!(record.hops_left, 7);
    assert_eq!(record.visited, visited);
    assert_eq!(record.next_hop, "neighbor.example");
    let mut forged = e.clone();
    forged.body.push(1);
    assert!(matches!(
        s.accept_forward(&forged, 1000, 7, &[], "", Admission::Custody, 0),
        Err(Error::InvalidSignature)
    ));
    assert!(matches!(
        s.accept_forward(&e, 1000, 0, &[], "", Admission::Custody, 0),
        Err(Error::HopBudgetExhausted)
    ));
    assert!(matches!(
        s.accept_forward(
            &e,
            1000,
            7,
            &["local.example".into()],
            "",
            Admission::Custody,
            0
        ),
        Err(Error::LoopDetected)
    ));
}

#[test]
fn routing_quarantines_bad_rows_individually() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let broken = envelope();
    let good = envelope();
    for e in [&broken, &good] {
        s.accept(e, 1000, Admission::Custody, 0).unwrap();
    }
    s.connection
        .execute(
            "UPDATE envelopes SET visited='bad' WHERE id=?1",
            [&broken.key.message_id],
        )
        .unwrap();
    assert_eq!(
        s.push_ready(0, 16, &["receiver.example".into()]).unwrap(),
        vec![good.key]
    );
    assert_eq!(s.quarantined_count().unwrap(), 1);
    let broken = envelope();
    s.accept(&broken, 1000, Admission::Custody, 0).unwrap();
    s.set_next_hop(&broken.key, "").unwrap();
    s.connection
        .execute(
            "UPDATE envelopes SET metadata='bad' WHERE id=?1",
            [&broken.key.message_id],
        )
        .unwrap();
    assert!(s.unrouted(16).unwrap().is_empty());
    assert_eq!(s.quarantined_count().unwrap(), 2);
}

#[test]
fn inbox_record_reads_the_persisted_ttl() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let e = envelope();
    s.accept(&e, 1000, Admission::Inbox, 0).unwrap();
    s.connection
        .execute(
            "UPDATE envelopes SET mailbox_ttl_ms=1234 WHERE id=?1",
            [&e.key.message_id],
        )
        .unwrap();
    drop(s);
    assert_eq!(f.open(0).get(&e.key).unwrap().unwrap().mailbox_ttl_ms, 1234);
}

#[test]
fn all_dev_versions_tolerate_columns_already_added_in_place() {
    for version in 1..=11 {
        let f = Fixture::new();
        let mut s = f.open(0);
        let e = envelope();
        s.accept(&e, 1000, Admission::Custody, 0).unwrap();
        s.handoff_generation().unwrap();
        s.connection
            .pragma_update(None, "user_version", version)
            .unwrap();
        drop(s);
        let mut s = f.open(1);
        assert_eq!(s.handoff_generation().unwrap(), 2, "version {version}");
        assert_eq!(
            s.accept(&e, 999, Admission::Custody, 1).unwrap(),
            Accepted::Duplicate,
            "version {version}"
        );
        assert_eq!(
            s.get(&e.key).unwrap().unwrap().remaining_ms,
            999,
            "version {version}"
        );
    }
}

#[test]
fn tombstone_and_pending_outcomes_roll_back_together() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let e = envelope();
    s.accept(&e, 1000, Admission::Inbox, 0).unwrap();
    s.connection.execute_batch("CREATE TRIGGER fail_removal BEFORE UPDATE OF state ON envelopes BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(s.tombstone("recipient", "session", "killed", 1).is_err());
    assert!(!s.is_tombstoned("recipient", "session", 1).unwrap());
    assert_eq!(s.get(&e.key).unwrap().unwrap().state, "inbox");
    assert_eq!(s.clock().unwrap().elapsed_ms, 0);
}

#[test]
fn outbound_offer_mode_preserves_step1_requests_and_leaves_other_kinds_unleased() {
    use crate::mesh::collect::OutboundCollect;
    let f = Fixture::new();
    let mut s = f.open(0);
    let local = envelope();
    let mut answer = envelope();
    answer.request_key = Some(local.key.clone());
    let mut forwarded = envelope();
    forwarded.key.origin_node = "forwarded.example".into();
    let mut receipt = envelope();
    receipt.key.origin_node = "receipt.example".into();
    receipt.kind = Kind::Receipt;
    for e in [&local, &answer, &forwarded, &receipt] {
        s.accept(e, 1000, Admission::Held, 0).unwrap();
    }
    // Step 1 uses the original recipient binding, independently of new routing state.
    s.set_next_hop(&local.key, "other.example").unwrap();
    let query = OutboundCollect::default();
    let offers = s
        .collect_outbound(
            Offer::Step1RequestsOnly,
            "origin.example",
            "receiver.example",
            &query,
            0,
        )
        .unwrap();
    assert_eq!(offers.len(), 1);
    assert_eq!(offers[0].envelope, local);
    for e in [&answer, &forwarded, &receipt] {
        assert_eq!(s.get(&e.key).unwrap().unwrap().retry_at_ms, 0);
    }
    let offers = s
        .collect_outbound(Offer::All, "origin.example", "receiver.example", &query, 0)
        .unwrap();
    assert_eq!(offers.len(), 3);
    for e in [&answer, &forwarded, &receipt] {
        assert!(offers.iter().any(|delivery| delivery.envelope == *e));
    }
}

#[test]
fn step1_ack_cannot_finish_answers_or_forwarded_requests() {
    use crate::mesh::collect::{OutboundAck, OutboundCollect};
    let f = Fixture::new();
    let mut s = f.open(0);
    let mut answer = envelope();
    answer.request_key = Some(envelope().key);
    let mut forwarded = envelope();
    forwarded.key.origin_node = "forwarded.example".into();
    for e in [&answer, &forwarded] {
        s.accept(e, 1000, Admission::Held, 0).unwrap();
        let query = OutboundCollect {
            ack: vec![OutboundAck {
                key: e.key.clone(),
                token: e.return_binding.collection_token.clone(),
                refusal: None,
            }],
            receipts: Vec::new(),
        };
        assert!(matches!(
            s.collect_outbound(
                Offer::Step1RequestsOnly,
                "origin.example",
                "receiver.example",
                &query,
                0
            ),
            Err(Error::InvalidEnvelope)
        ));
        assert_eq!(s.get(&e.key).unwrap().unwrap().state, "held");
        s.collect_outbound(Offer::All, "origin.example", "receiver.example", &query, 0)
            .unwrap();
        assert_eq!(s.get(&e.key).unwrap().unwrap().state, "delivered");
    }
}

fn strip_step2_schema(connection: &Connection) {
    connection
        .execute_batch(
            "DROP INDEX push_ready; DROP INDEX unrouted;
         ALTER TABLE envelopes DROP COLUMN next_hop;
         ALTER TABLE envelopes DROP COLUMN hops_left;
         ALTER TABLE envelopes DROP COLUMN visited;
         ALTER TABLE envelopes DROP COLUMN kind;
         ALTER TABLE envelopes DROP COLUMN mailbox_ttl_ms;
         DROP TABLE agent_owners; DROP TABLE agent_tombstones;
         PRAGMA user_version=11;",
        )
        .unwrap();
}

#[test]
fn genuine_v11_backfills_live_mail_and_second_migration_is_read_only() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let rows: Vec<_> = [Admission::Held, Admission::Custody, Admission::Inbox]
        .into_iter()
        .map(|admission| {
            let mail = envelope();
            s.accept(&mail, 1000, admission, 0).unwrap();
            (mail, admission)
        })
        .collect();
    strip_step2_schema(&s.connection);
    drop(s);
    let mut s = f.open(0);
    for (mail, admission) in rows {
        let row = s.get(&mail.key).unwrap().unwrap();
        assert_eq!(row.envelope, mail);
        assert_eq!(row.mailbox_ttl_ms, DAY_MS);
        assert_eq!(
            row.remaining_ms,
            if admission == Admission::Inbox {
                DAY_MS
            } else {
                1000
            }
        );
        assert_eq!(
            row.next_hop,
            if admission == Admission::Inbox {
                ""
            } else {
                &mail.return_binding.recipient_node
            }
        );
        assert_eq!(
            row.state,
            match admission {
                Admission::Held => "held",
                Admission::Custody => "custody",
                Admission::Inbox => "inbox",
            }
        );
    }
    let changes = s.connection.total_changes();
    let wal = fs::read(f.path.with_extension("sqlite-wal")).unwrap();
    schema::migrate(&mut s.connection, f.path.parent().unwrap(), 0).unwrap();
    assert_eq!(s.connection.total_changes(), changes);
    assert_eq!(fs::read(f.path.with_extension("sqlite-wal")).unwrap(), wal);
}

#[test]
fn stamped_stores_repair_missing_columns_in_each_table() {
    for version in [11, 12] {
        let f = Fixture::new();
        let mut s = f.open(0);
        let mail = envelope();
        s.accept(&mail, 1000, Admission::Held, 0).unwrap();
        if version == 11 {
            strip_step2_schema(&s.connection);
        }
        s.connection
            .execute_batch(
                "ALTER TABLE envelopes DROP COLUMN receipt_sent;
             ALTER TABLE identity_pins DROP COLUMN origin;
             INSERT INTO identity_pins VALUES('configured','peer.example','node.example',X'42');",
            )
            .unwrap();
        drop(s);
        let s = f.open(0);
        assert_eq!(s.get(&mail.key).unwrap().unwrap().envelope, mail);
        let receipt: Option<String> = s
            .connection
            .query_row("SELECT receipt_sent FROM envelopes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(receipt, None);
        let origin: String = s
            .connection
            .query_row("SELECT origin FROM identity_pins", [], |r| r.get(0))
            .unwrap();
        assert_eq!(origin, "unknown");
    }
}

#[test]
fn busy_migration_and_transient_open_errors_never_advise_discarding_mail() {
    let f = Fixture::new();
    let s = f.open(0);
    strip_step2_schema(&s.connection);
    s.connection.execute_batch("BEGIN IMMEDIATE").unwrap();
    let error = Store::open_with(&f.path, 0, Limits::default(), f.disk.clone())
        .err()
        .unwrap();
    assert!(
        matches!(error, Error::Sql(rusqlite::Error::SqliteFailure(ref code, _)) if code.code == rusqlite::ErrorCode::DatabaseBusy)
    );
    assert_eq!(error.code(), "mail_store_unavailable");
    assert!(!error.is_permanent());
    assert!(!error.to_string().contains("move the file aside"));
    s.connection.execute_batch("ROLLBACK").unwrap();
    let missing_parent = f.path.join("not-a-directory.sqlite");
    let error = Store::open_with(&missing_parent, 0, Limits::default(), f.disk.clone())
        .err()
        .unwrap();
    assert_eq!(error.code(), "mail_store_unavailable");
    assert!(!error.is_permanent());
    assert!(!error.to_string().contains("move the file aside"));
}

#[test]
fn permission_tightening_is_best_effort_and_skips_current_directory() {
    let f = Fixture::new();
    tighten_mode(&f.path, 0o600);
    assert_eq!(mode_directory(Path::new("mail.sqlite")), None);
    assert_eq!(mode_directory(Path::new("./mail.sqlite")), None);
    assert_eq!(mode_directory(&f.path), f.path.parent());
    f.open(0);
}

#[test]
fn conflicting_key_keeps_its_public_error_code() {
    assert_eq!(Error::ConflictingKey.code(), "message_key_conflict");
    assert!(Error::ConflictingKey.is_permanent());
}

#[test]
fn quarantine_discards_bodies_and_releases_quota_including_older_quarantines() {
    for older in [false, true] {
        let f = Fixture::new();
        let limits = Limits {
            logical_bytes: 10_000,
            metadata_reserve: 4000,
            ..Limits::default()
        };
        let mut s = f.limited(0, limits);
        let mut mail = envelope();
        mail.body = vec![42; 6000];
        s.accept(&mail, 1000, Admission::Custody, 0).unwrap();
        let mut next = envelope();
        next.body = mail.body.clone();
        assert!(matches!(
            s.accept(&next, 1000, Admission::Custody, 0),
            Err(Error::MailStoreFull)
        ));
        if older {
            s.connection
                .execute("UPDATE envelopes SET state='quarantined'", [])
                .unwrap();
            drop(s);
            s = f.limited(0, limits);
        } else {
            s.quarantine(&mail.key).unwrap();
        }
        let row = s.get(&mail.key).unwrap().unwrap();
        assert_eq!(row.state, "quarantined");
        assert!(row.envelope.body.is_empty());
        assert_eq!(row.envelope.target_agent, mail.target_agent);
        s.accept(&next, 1000, Admission::Custody, 0).unwrap();
        assert_eq!(s.quarantined_count().unwrap(), 1);
    }
}

#[test]
fn migration_archives_full_raw_quarantined_rows_before_clearing() {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let f = Fixture::new();
    let mut s = f.open(0);
    let mail = envelope();
    s.accept(&mail, 1000, Admission::Held, 0).unwrap();
    let body = [0xff, 0, 0x80];
    let metadata = [0xfe, 0, b'{'];
    s.connection
        .execute(
            "UPDATE envelopes SET state='quarantined',body=?1,metadata=?2",
            params![body.as_slice(), metadata.as_slice()],
        )
        .unwrap();
    strip_step2_schema(&s.connection);
    drop(s);
    let s = f.open(10);
    let archived = f.path.parent().unwrap().join("mesh-quarantine.jsonl");
    let line = fs::read_to_string(&archived).unwrap();
    let record: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(
        record["key"]["message_id"]["base64"],
        STANDARD.encode(mail.key.message_id.as_bytes())
    );
    assert_eq!(record["state"]["base64"], STANDARD.encode(b"quarantined"));
    assert_eq!(record["reason"], "existing quarantine during migration");
    assert_eq!(record["quarantined_at"], 10);
    assert_eq!(record["body"]["base64"], STANDARD.encode(body));
    assert_eq!(record["envelope"]["base64"], STANDARD.encode(metadata));
    assert_eq!(record["envelope"]["sqlite_type"], "blob");
    let columns: i64 = s
        .connection
        .query_row(
            "SELECT count(*) FROM pragma_table_info('envelopes')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        record["raw_row"].as_object().unwrap().len(),
        columns as usize + 1
    );
    assert_eq!(record["raw_row"]["custody_deadline"]["value"], 1000);
    let kept: Vec<u8> = s
        .connection
        .query_row("SELECT body FROM envelopes", [], |r| r.get(0))
        .unwrap();
    assert!(kept.is_empty());
    assert_eq!(
        fs::metadata(&archived).unwrap().permissions().mode() & 0o777,
        0o600
    );
    drop(s);
    f.open(11);
    assert_eq!(fs::read_to_string(archived).unwrap(), line);
}

#[test]
fn failed_quarantine_backup_preserves_body_and_state_until_retry() {
    for migration in [false, true] {
        let f = Fixture::new();
        let mut s = f.open(0);
        let mail = envelope();
        s.accept(&mail, 1000, Admission::Held, 0).unwrap();
        let blocked = f.path.parent().unwrap().join("mesh-quarantine.jsonl");
        fs::create_dir(&blocked).unwrap();
        if migration {
            s.connection
                .execute("UPDATE envelopes SET state='quarantined'", [])
                .unwrap();
            strip_step2_schema(&s.connection);
            drop(s);
            s = f.open(0);
        } else {
            s.quarantine(&mail.key).unwrap();
        }
        let row = s.get(&mail.key).unwrap().unwrap();
        assert_eq!(row.envelope, mail);
        assert_eq!(row.state, if migration { "quarantined" } else { "held" });
        fs::remove_dir(&blocked).unwrap();
        s.quarantine(&mail.key).unwrap();
        assert!(s.get(&mail.key).unwrap().unwrap().envelope.body.is_empty());
        assert_eq!(fs::read_to_string(blocked).unwrap().lines().count(), 1);
    }
}

#[test]
fn failed_database_quarantine_update_leaves_recoverable_archive() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let mail = envelope();
    s.accept(&mail, 1000, Admission::Custody, 0).unwrap();
    s.connection.execute_batch("CREATE TRIGGER reject_clear BEFORE UPDATE OF body ON envelopes BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(s.quarantine(&mail.key).is_err());
    assert_eq!(s.get(&mail.key).unwrap().unwrap().envelope, mail);
    let archive =
        fs::read_to_string(f.path.parent().unwrap().join("mesh-quarantine.jsonl")).unwrap();
    assert_eq!(archive.lines().count(), 1);
}

#[test]
fn quarantine_sidecar_rotation_keeps_two_bounded_private_files() {
    let f = Fixture::new();
    let directory = f.path.parent().unwrap();
    let active = directory.join("mesh-quarantine.jsonl");
    let previous = directory.join("mesh-quarantine.jsonl.1");
    let lines = [b"{\"n\":1}\n", b"{\"n\":2}\n", b"{\"n\":3}\n"];
    for line in lines {
        quarantine::append(directory, line, line.len() as u64).unwrap();
        fs::set_permissions(&active, fs::Permissions::from_mode(0o666)).unwrap();
    }
    // Opening the active file tightens it before it can become a rotated file.
    quarantine::append(directory, b"{\"n\":4}\n", lines[0].len() as u64).unwrap();
    assert_eq!(fs::read_to_string(&active).unwrap(), "{\"n\":4}\n");
    assert_eq!(fs::read_to_string(&previous).unwrap(), "{\"n\":3}\n");
    assert_eq!(fs::read_dir(directory).unwrap().count(), 2);
    for path in [&active, &previous] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert!(quarantine::append(directory, b"oversized record\n", 8).is_err());
    assert_eq!(fs::read_to_string(active).unwrap(), "{\"n\":4}\n");
}
