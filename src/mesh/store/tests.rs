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
        intent: "needs_reply".into(),
        body: b"answer me".to_vec(),
    }
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
    assert_eq!(s.get(&e.key).unwrap().unwrap().remaining_ms, 500);
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
            "DROP TABLE identity_pins;
         CREATE TABLE identity_pins (
             source TEXT NOT NULL, peer TEXT NOT NULL, node_id TEXT NOT NULL,
             public_key BLOB NOT NULL, PRIMARY KEY(source,peer),
             UNIQUE(source,node_id), UNIQUE(source,public_key));
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
