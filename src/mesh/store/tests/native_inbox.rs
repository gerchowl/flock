use super::*;
fn admit(s: &mut Store<Disk>) -> Envelope {
    let e = envelope();
    s.accept_local(&e, CUSTODY_TTL_MS, "recipient", "session", 0)
        .unwrap();
    s.settle_queue_admission(&e.key, None, 0).unwrap();
    e
}
#[test]
fn native_inbox_mixed_batch_consumes_once_without_changing_custody() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let old = admit(&mut s);
    let live = admit(&mut s);
    s.native_inbox_delivered(&old.key, 100, 1).unwrap();
    s.native_inbox_delivered(&live.key, 100, 51).unwrap();
    for recipient in [("other", "session"), ("recipient", "other-session")] {
        assert!(s
            .native_inbox(Some(recipient), true, 101)
            .unwrap()
            .0
            .is_empty());
    }
    let (read, expired) = s
        .native_inbox(Some(("recipient", "session")), true, 101)
        .unwrap();
    assert_eq!(read, vec![live.clone()]);
    assert_eq!(expired, vec![old.clone()]);
    drop(s);
    let mut s = f.open(101);
    assert_eq!(
        s.native_inbox(Some(("recipient", "session")), true, 101)
            .unwrap(),
        (vec![], vec![])
    );
    for e in [old, live] {
        let retained = s.get(&e.key).unwrap().unwrap();
        assert_eq!(retained.state, "delivered");
        assert_eq!(retained.envelope, e);
    }
}
#[test]
fn native_inbox_window_starts_at_delivery_and_never_renews() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let e = admit(&mut s);
    s.connection
        .execute_batch("DROP TABLE native_inbox_reads; PRAGMA user_version=12;")
        .unwrap();
    drop(s);
    let mut s = f.open(49);
    let recipient = Some(("recipient", "session"));
    assert!(s.native_inbox(recipient, false, 49).unwrap().0.is_empty());
    s.native_inbox_delivered(&e.key, 100, 50).unwrap();
    s.native_inbox_delivered(&e.key, 1000, 149).unwrap();
    drop(s);
    let mut s = f.open(149);
    assert_eq!(
        s.native_inbox(recipient, false, 149).unwrap().0,
        vec![e.clone()]
    );
    assert_eq!(
        s.native_inbox(recipient, true, 150).unwrap(),
        (vec![], vec![e])
    );
    assert!(s.native_inbox(recipient, false, 151).unwrap().1.is_empty());
}
#[test]
fn native_inbox_failed_batch_ack_rolls_back_and_skips_corrupt_entries() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let a = admit(&mut s);
    let b = admit(&mut s);
    let corrupt = admit(&mut s);
    let invalid_deadline = admit(&mut s);
    for e in [&a, &b, &corrupt, &invalid_deadline] {
        s.native_inbox_delivered(&e.key, 100, 1).unwrap();
    }
    s.connection
        .execute(
            "UPDATE envelopes SET metadata='{' WHERE id=?1",
            [&corrupt.key.message_id],
        )
        .unwrap();
    s.connection
        .execute(
            "UPDATE native_inbox_reads SET until_ms='invalid' WHERE id=?1",
            [&invalid_deadline.key.message_id],
        )
        .unwrap();
    s.connection
        .execute_batch(
            "CREATE TRIGGER refuse_read BEFORE UPDATE ON native_inbox_reads
        WHEN NEW.state='read' AND (SELECT COUNT(*) FROM native_inbox_reads WHERE state='read')=1
        BEGIN SELECT RAISE(ABORT,'injected ack failure'); END;",
        )
        .unwrap();
    let recipient = Some(("recipient", "session"));
    let _ = s.native_inbox(recipient, false, 2).unwrap();
    assert!(s.native_inbox(recipient, true, 2).is_err());
    s.connection
        .execute_batch("DROP TRIGGER refuse_read;")
        .unwrap();
    s.connection
        .execute(
            "UPDATE envelopes SET metadata=X'ff' WHERE id=?1",
            [&corrupt.key.message_id],
        )
        .unwrap();
    drop(s);
    let mut s = f.open(2);
    assert_eq!(s.native_inbox(recipient, true, 2).unwrap().0, vec![a, b]);
    let raw: Vec<u8> = s
        .connection
        .query_row(
            "SELECT metadata FROM envelopes WHERE id=?1",
            [&corrupt.key.message_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(raw, vec![255]);
    assert!(s.native_inbox(recipient, true, 3).unwrap().0.is_empty());
}

#[test]
fn native_inbox_idle_sweep_is_read_only() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let observer = Connection::open(&f.path).unwrap();
    let version: i64 = observer
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .unwrap();
    let changes = s.connection.total_changes();
    for now in [1, 1000, 6000] {
        assert_eq!(s.native_inbox(None, false, now).unwrap(), (vec![], vec![]));
    }
    assert_eq!(
        s.connection.total_changes(),
        changes,
        "idle sweep wrote the store"
    );
    let after: i64 = observer
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(after, version, "idle sweep committed");
}

#[test]
fn native_inbox_live_peek_and_unmatched_consume_are_read_only() {
    let f = Fixture::new();
    let mut s = f.open(0);
    let e = admit(&mut s);
    s.native_inbox_delivered(&e.key, 100, 0).unwrap();
    let changes = s.connection.total_changes();
    assert!(s.native_inbox(None, false, 1).unwrap().1.is_empty());
    assert_eq!(
        s.native_inbox(Some(("recipient", "session")), false, 2)
            .unwrap()
            .0,
        vec![e.clone()]
    );
    assert!(s
        .native_inbox(Some(("other", "session")), true, 3)
        .unwrap()
        .0
        .is_empty());
    assert_eq!(
        s.connection.total_changes(),
        changes,
        "query-only inbox work wrote the store"
    );
    assert_eq!(
        s.native_inbox(Some(("recipient", "session")), true, 100)
            .unwrap(),
        (vec![], vec![e])
    );
    assert!(s.native_inbox(None, false, 101).unwrap().1.is_empty());
}
