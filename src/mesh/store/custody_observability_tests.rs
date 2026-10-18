use super::*;

fn late(store: &mut Store<Disk>) -> Envelope {
    let request = envelope();
    store.set_local_node(&request.key.origin_node);
    store
        .accept_local(&request, CUSTODY_TTL_MS, "native", "session", 0)
        .unwrap();
    store
        .queue_commitment_unknown(
            &request.key,
            Some(crate::structured::queue::custody::Unknown::WriteUncertain),
            0,
        )
        .unwrap();
    store.maintain(DAY_MS).unwrap();
    store
        .settle_queue_admission(&request.key, None, DAY_MS + 1)
        .unwrap();
    request
}

#[test]
fn final_reply_keeps_delivered_after_expiry_detail_and_ack_debt() {
    let f = Fixture::new();
    let mut store = f.open(0);
    let request = late(&mut store);
    let mut reply = envelope();
    reply.key.origin_node = request.return_binding.recipient_node.clone();
    reply.return_binding.request = reply.key.clone();
    reply.request_key = Some(request.key.clone());
    reply.correlation_id = "final-answer".into();
    reply.body = br#"{"message":{"body":"answer"}}"#.to_vec();
    store
        .accept_collected(&reply, CUSTODY_TTL_MS, DAY_MS + 2)
        .unwrap();
    drop(store);
    let mut store = f.open(DAY_MS + 3);
    let status = store
        .status(
            &request.key.origin_node,
            &request.correlation_id,
            DAY_MS + 3,
        )
        .unwrap()
        .unwrap();
    assert_eq!(status.state, "delivered");
    assert_eq!(
        status.detail,
        Some(format!(
            "delivered_after_expiry:{}",
            request.key.lifecycle_id()
        ))
    );
    assert_eq!(status.reply.unwrap().body, "answer");
    assert_eq!(
        store.collection_acks(&request.key).unwrap(),
        vec![reply.key]
    );
    assert_eq!(
        store
            .collect_ready(
                &request.key.origin_node,
                DAY_MS + 5_001,
                1,
                &[],
                std::slice::from_ref(&request.return_binding.recipient_node)
            )
            .unwrap()
            .len(),
        1,
        "retaining an informational detail must not block acknowledgement collection"
    );
}

#[test]
fn late_delivery_detail_allows_fast_collection_without_extending_deadlines() {
    let f = Fixture::new();
    let mut store = f.open(0);
    let request = late(&mut store);
    let peers = [request.return_binding.recipient_node.clone()];
    for n in [5_001, 11_001] {
        assert_eq!(
            store
                .collect_ready(&request.key.origin_node, DAY_MS + n, 1, &[], &peers)
                .unwrap()
                .len(),
            1
        );
    }
    store
        .collect_fast(
            &request.key.origin_node,
            std::slice::from_ref(&request.correlation_id),
            &[],
            DAY_MS + 11_002,
        )
        .unwrap();
    assert_eq!(
        store
            .collect_ready(&request.key.origin_node, DAY_MS + 16_002, 1, &[], &peers)
            .unwrap()
            .len(),
        1,
        "a live waiter must shorten the backed-off informational row too"
    );
    assert_eq!(
        store
            .collection_record(&request.key, CUSTODY_TTL_MS)
            .unwrap()
            .unwrap()
            .remaining_ms,
        0
    );
}

#[test]
fn proof_save_failure_crossing_expiry_retains_the_correction_window() {
    let f = Fixture::new();
    let mut store = f.open(0);
    let request = envelope();
    store.set_local_node(&request.key.origin_node);
    store
        .accept_local(&request, CUSTODY_TTL_MS, "native", "session", 0)
        .unwrap();
    store
        .queue_commitment_unknown(
            &request.key,
            Some(crate::structured::queue::custody::Unknown::AdmissionPending),
            0,
        )
        .unwrap();
    assert_eq!(
        store
            .queue_commitment_unknown(
                &request.key,
                Some(crate::structured::queue::custody::Unknown::SettlementPending),
                DAY_MS
            )
            .unwrap(),
        Some(CUSTODY_TTL_MS as u64),
        "expiry during the first failed proven save must return the existing correction window"
    );
    assert_eq!(
        store.get(&request.key).unwrap().unwrap().state,
        "inbox_expired"
    );
    store
        .settle_queue_admission(&request.key, None, DAY_MS + 1)
        .unwrap();
    assert_eq!(store.get(&request.key).unwrap().unwrap().state, "delivered");
}
