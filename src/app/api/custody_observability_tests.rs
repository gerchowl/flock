use super::*;
use crate::api::error_code::ErrorCode;
use std::time::{Duration, Instant};

async fn correction(n: u32) -> (AppQueue, Deliver, rusqlite::Connection) {
    let mut r = setup(n).await;
    let d = mail(&r);
    let wall = crate::app::notifications::now_ms() as i64;
    with_store(|s| {
        let session = &r
            .app
            .state
            .structured_agents
            .get(&r.case.id)
            .unwrap()
            .0
            .session_id;
        s.accept_bound_delivery(&d, (&r.case.id, session), false, wall)
            .map_err(|e| e.to_string())
    })
    .unwrap();
    let (ticket, ack) = r.app.admit_queue_custody(&d.envelope).unwrap();
    r.until(|_| ticket.committed.try_recv().is_ok_and(|x| x.is_ok()))
        .await;
    drop(ack);
    let db = rusqlite::Connection::open(with_store(|s| Ok(s.path().to_owned())).unwrap()).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_delivery_save BEFORE UPDATE OF state ON envelopes
        WHEN NEW.state='delivered' BEGIN SELECT RAISE(FAIL,'injected delivery save failure'); END;",
    )
    .unwrap();
    r.app
        .retry_queue_custody(d.envelope.clone(), ErrorCode::InputWriteUnknown);
    with_store(|s| {
        s.maintain(wall + crate::mesh::store::DAY_MS)
            .map_err(|e| e.to_string())
    })
    .unwrap();
    r.app.finish_structured_messages(true, Instant::now());
    assert!(r.app.structured_sends.retries[&d.envelope.key].custody_correction);
    (r, d, db)
}
fn delivered(r: &AppQueue, d: &Deliver) {
    let row = with_store(|s| s.get(&d.envelope.key).map_err(|e| e.to_string()))
        .unwrap()
        .unwrap();
    assert_eq!(row.state, "delivered");
    assert!(row.envelope.body.is_empty());
    assert!(!r.app.structured_sends.retries.contains_key(&d.envelope.key));
}

#[test]
fn correction_owner_loss_cannot_erase_proven_commitment() {
    run(async {
        let _store = crate::mesh::runtime_store::TestStore::new();
        let (mut r, d, db) = correction(2561).await;
        r.stop().await;
        r.app.structured_workers = crate::structured::workers::Workers::new(r.notify.clone());
        assert_eq!(
            r.app
                .structured_workers
                .queue_custody_committed(&r.case.id, &d.envelope.key),
            Err(crate::structured::queue::custody::Unknown::OwnerUnavailable)
        );
        let deadline = r.app.structured_sends.retries[&d.envelope.key].at;
        r.app.finish_structured_messages(false, deadline);
        assert!(
            r.app.structured_sends.retries.contains_key(&d.envelope.key),
            "owner loss inside retention must not drop proven correction"
        );
        assert_eq!(r.app.structured_sends.recovering.len(), 1);
        db.execute_batch("DROP TRIGGER fail_delivery_save").unwrap();
        let deadline = r.app.structured_sends.retries[&d.envelope.key].at;
        r.app.finish_structured_messages(false, deadline);
        delivered(&r, &d);
        assert!(r.app.structured_sends.recovering.is_empty());
    });
}

#[test]
fn correction_replacement_owner_wake_bypasses_long_backoff() {
    run(async {
        let _store = crate::mesh::runtime_store::TestStore::new();
        let (mut r, d, db) = correction(2562).await;
        r.stop().await;
        let future = Instant::now() + Duration::from_secs(300);
        let retry = r
            .app
            .structured_sends
            .retries
            .get_mut(&d.envelope.key)
            .unwrap();
        retry.at = future;
        retry.wake_at = future;
        let attempts = r.app.structured_sends.custody_attempts;
        r.restart(|_| db.execute_batch("DROP TRIGGER fail_delivery_save").unwrap())
            .await;
        r.app.finish_structured_messages(true, Instant::now());
        delivered(&r, &d);
        assert_eq!(
            r.app.structured_sends.custody_attempts, attempts,
            "correction never re-admits"
        );
        assert_eq!(r.inputs().len(), 1);
        r.stop().await;
    });
}

#[test]
fn real_worker_load_measures_changed_wakes_and_bounded_recovery() {
    run(async {
        let _store = crate::mesh::runtime_store::TestStore::new();
        let mut r = setup(2563).await;
        let mut pending = mail(&r);
        pending.envelope.target_agent = "agent_absent_example".into();
        crate::mesh::sign::seal(&mut pending.envelope, &NodeIdentity::fixture([41; 32]));
        with_store(|s| {
            s.accept_bound_delivery(
                &pending,
                ("agent_absent_example", "absent-session"),
                false,
                crate::app::notifications::now_ms() as i64,
            )
            .map_err(|e| e.to_string())
        })
        .unwrap();
        let scheduled = Instant::now();
        for _ in 0..3 {
            r.app
                .retry_queue_custody(pending.envelope.clone(), ErrorCode::AgentNotFound);
        }
        let begin = Instant::now();
        let retry = r
            .app
            .structured_sends
            .retries
            .get_mut(&pending.envelope.key)
            .unwrap();
        assert!(retry.at >= scheduled + Duration::from_secs(240));
        retry.wake_at = begin;
        let counts = r.app.structured_sends.custody_worker_wakes;
        for n in 0..64 {
            let mut d = mail(&r);
            d.envelope.correlation_id = format!("load-{n}");
            let mut body: Value = serde_json::from_slice(&d.envelope.body).unwrap();
            body["message"]["correlation_id"] = json!(d.envelope.correlation_id);
            d.envelope.body = serde_json::to_vec(&body).unwrap();
            crate::mesh::sign::seal(&mut d.envelope, &NodeIdentity::fixture([41; 32]));
            let rx = request(&mut r, d);
            assert!(answer(&mut r, &rx).await["result"]["item_id"].is_string());
            r.until(|r| r.inputs().len() > n).await;
            let id = r.inputs()[n]["input"]["uuid"].clone();
            r.control(json!({"op":"lifecycle","state":"started","uuid":id}));
            r.until(|r| r.facts().len() > n * 2).await;
            r.control(json!({"op":"result"}));
            r.until(|r| r.facts().len() >= (n + 1) * 2).await;
        }
        let elapsed = begin.elapsed();
        let after = r.app.structured_sends.custody_worker_wakes;
        let attempts = r
            .app
            .structured_sends
            .custody_attempt_keys
            .get(&pending.envelope.key)
            .copied()
            .unwrap_or(0);
        let measurement = format!(
            "CUSTODY_LOAD turns=64 polls={} changed={} retries={} elapsed_ms={}",
            after.0 - counts.0,
            after.1 - counts.1,
            attempts,
            elapsed.as_millis()
        );
        println!("{measurement}"); // guardrails-ok(no-debug-leftovers): workload measurements are the test output
        assert!(
            after.1 > counts.1,
            "measure actual worker progress notifications"
        );
        assert!(
            attempts > 0,
            "the workload must exercise a changed wake retry"
        );
        assert!(
            attempts as u128 <= 1 + elapsed.as_millis() / 4000,
            "actual changed wakes must still obey the four-second retry floor"
        );
        assert!(r
            .app
            .structured_sends
            .retries
            .contains_key(&pending.envelope.key));
        r.stop().await;
    });
}
