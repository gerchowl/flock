use super::*;
use crate::api::error_code::ErrorCode;
use crate::structured::queue::admission::{Admission, Gate, Principal, MAX_BATCH};

fn id(n: u64) -> String {
    format!("00000000-0000-4000-8000-{n:012x}")
}
fn input(n: u64) -> Item {
    item(&id(n))
}
fn queue(root: &Root) -> Admission {
    Admission::new(root.open(), MAX_BATCH).unwrap()
}
fn send(q: &mut Admission, n: u64, producer: Producer) {
    let mut item = input(n);
    item.kind = Kind::Message;
    let principal = if producer == Producer::Human {
        Principal::Operator
    } else {
        Principal::Origin(producer)
    };
    q.enqueue(item, principal, Gate::Send).unwrap();
}
fn complete(q: &mut Admission) -> Vec<Item> {
    let batch = q.batch(100).unwrap().unwrap();
    let original = batch.items.clone();
    q.replace(
        batch
            .items
            .into_iter()
            .map(|mut i| {
                i.state = State::Done;
                i
            })
            .collect(),
    )
    .unwrap();
    original
}

#[test]
fn gate_and_authenticated_origin_are_durable_before_acceptance() {
    let root = Root::new();
    let mut q = queue(&root);
    let decisions = [
        Gate::Send,
        Gate::Hold,
        Gate::Stash("later".into()),
        Gate::Drop,
    ];
    for (n, gate) in decisions.into_iter().enumerate() {
        let mut item = input(n as u64);
        item.producer = Producer::System("forged".into());
        q.enqueue(item, Principal::Operator, gate).unwrap();
    }
    assert_eq!(
        q.journal()
            .items()
            .iter()
            .map(|i| i.state.clone())
            .collect::<Vec<_>>(),
        [State::Queued, State::Held, State::Stashed, State::Dropped]
    );
    assert_eq!(q.journal().items()[2].metadata["stash"], "later");
    assert!(q
        .journal()
        .items()
        .iter()
        .all(|i| i.producer == Producer::Human));
    assert_eq!(q.journal().replay(0).unwrap().through, 4);
    assert_eq!(q.batch(100).unwrap().unwrap().items.len(), 1);
    let before = fs::read(root.path()).unwrap();
    assert!(q
        .enqueue(input(0), Principal::Operator, Gate::Hold)
        .is_err());
    assert_eq!(
        fs::read(root.path()).unwrap(),
        before,
        "the gate cannot run twice"
    );
    drop(q);
    let reopened = queue(&root);
    assert_eq!(reopened.journal().replay(0).unwrap().through, 4);
    assert_eq!(
        reopened.journal().items()[2].metadata["gate"],
        json!({"decision":"stash","stash":"later"})
    );
}

#[test]
fn fifo_keeps_every_producer_boundary_and_consecutive_humans() {
    let root = Root::new();
    let mut q = queue(&root);
    let origins = [
        Producer::Human,
        Producer::Human,
        Producer::Agent("sender".into()),
        Producer::Human,
        Producer::Human,
        Producer::Channel {
            binding: "chat".into(),
            user: "user".into(),
        },
        Producer::System("wake".into()),
        Producer::Fork("child".into()),
    ];
    for (n, producer) in origins.iter().enumerate() {
        send(&mut q, n as u64, producer.clone());
    }
    for expected in [vec![0, 1], vec![2], vec![3, 4], vec![5], vec![6], vec![7]] {
        let through = q.journal().replay(0).unwrap().through;
        let batch = q.batch(100).unwrap().unwrap();
        assert_eq!(
            q.journal().replay(0).unwrap().through,
            through,
            "formation is pure"
        );
        assert_eq!(
            batch
                .items
                .iter()
                .map(|i| i.item_id.clone())
                .collect::<Vec<_>>(),
            expected.iter().map(|n| id(*n)).collect::<Vec<_>>()
        );
        for (block, n) in batch.envelope["message"]["content"]
            .as_array()
            .unwrap()
            .iter()
            .zip(&expected)
        {
            let text = block["text"].as_str().unwrap();
            assert!(text.starts_with(&format!("From {}:\n", json!(origins[*n as usize]))));
        }
        let completed = complete(&mut q);
        for (i, n) in completed.iter().zip(expected) {
            assert_eq!(i.producer, origins[n as usize]);
        }
    }
    assert!(q.batch(100).unwrap().is_none());
    drop(q);
    assert!(queue(&root)
        .journal()
        .items()
        .iter()
        .all(|i| i.state == State::Done));
}

#[test]
fn count_bound_uuid_and_owned_candidate_are_stable() {
    let root = Root::new();
    let mut q = queue(&root);
    for n in 0..9 {
        send(&mut q, n, Producer::Human);
    }
    let batch = q.batch(100).unwrap().unwrap();
    assert_eq!(batch.items.len(), 8);
    assert_eq!(batch.envelope["uuid"], batch.id);
    assert_eq!(batch.id.len(), 36);
    assert_eq!(batch.id.as_bytes()[14], b'4');
    assert!(matches!(batch.id.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
    assert!(!batch.items.iter().any(|i| i.item_id == batch.id));
    complete(&mut q);
    let single = q.batch(100).unwrap().unwrap();
    assert_eq!(single.id, id(8));
    send(&mut q, 9, Producer::Human);
    assert_eq!(single.items.len(), 1, "candidate is an owned immutable cut");
    assert_eq!(batch.items.len(), 8);
}

#[test]
fn live_edit_leases_are_skipped_and_expired_leases_resume_in_order() {
    let root = Root::new();
    let mut q = queue(&root);
    send(&mut q, 0, Producer::Human);
    let mut edited = input(1);
    edited.metadata.insert(
        "edit_lease".into(),
        json!({"client":"editor","until_ms":200}),
    );
    q.enqueue(edited, Principal::Operator, Gate::Send).unwrap();
    send(&mut q, 2, Producer::Human);
    assert_eq!(
        q.batch(100)
            .unwrap()
            .unwrap()
            .items
            .iter()
            .map(|i| i.item_id.clone())
            .collect::<Vec<_>>(),
        [id(0), id(2)]
    );
    assert_eq!(q.batch(200).unwrap().unwrap().items.len(), 3);
    let mut malformed = input(3);
    malformed
        .metadata
        .insert("edit_lease".into(), json!({"until_ms":"invalid"}));
    q.enqueue(malformed, Principal::Operator, Gate::Send)
        .unwrap();
    assert_eq!(q.batch(200).unwrap().unwrap().items.len(), 3);
}

#[test]
fn continuation_waits_for_its_anchors_result_not_only_completion() {
    let root = Root::new();
    let mut q = queue(&root);
    send(&mut q, 0, Producer::Human);
    let mut continuation = input(1);
    continuation.kind = Kind::Continuation;
    continuation
        .metadata
        .insert("anchor".into(), json!({"item_id":id(0)}));
    q.enqueue(continuation, Principal::Operator, Gate::Send)
        .unwrap();
    assert_eq!(q.batch(100).unwrap().unwrap().items.len(), 1);
    let mut anchor = q.journal().items()[0].clone();
    anchor.state = State::Done;
    anchor
        .metadata
        .insert("awaiting_result".into(), json!(true));
    q.replace(vec![anchor.clone()]).unwrap();
    assert!(q.batch(100).unwrap().is_none());
    anchor
        .metadata
        .insert("awaiting_result".into(), json!("future"));
    q.replace(vec![anchor.clone()]).unwrap();
    assert!(
        q.batch(100).unwrap().is_none(),
        "unknown result flags fail closed"
    );
    anchor.metadata.remove("awaiting_result");
    q.replace(vec![anchor]).unwrap();
    assert_eq!(q.batch(100).unwrap().unwrap().id, id(1));
}

#[test]
fn command_is_verbatim_isolated_and_agent_slash_text_is_labelled() {
    let root = Root::new();
    let mut q = queue(&root);
    send(&mut q, 0, Producer::Human);
    let mut command = input(1);
    command.kind = Kind::Command;
    command.body = "/compact".into();
    q.enqueue(command, Principal::Operator, Gate::Send).unwrap();
    send(&mut q, 2, Producer::Human);
    let mut agent = input(3);
    agent.kind = Kind::Message;
    agent.body = "/compact".into();
    q.enqueue(
        agent,
        Principal::Origin(Producer::Agent("sender".into())),
        Gate::Send,
    )
    .unwrap();
    assert_eq!(complete(&mut q).len(), 1);
    let batch = q.batch(100).unwrap().unwrap();
    assert_eq!(batch.items.len(), 1);
    assert_eq!(
        batch.envelope["message"]["content"],
        json!([{"type":"text","text":"/compact"}])
    );
    assert_eq!(batch.items[0].producer, Producer::Human);
    complete(&mut q);
    assert_eq!(complete(&mut q).len(), 1);
    let agent = q.batch(100).unwrap().unwrap();
    assert_eq!(agent.items[0].kind, Kind::Message);
    let text = agent.envelope["message"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(text.starts_with("From {\"agent\":\"sender\"}:\n/compact\n"));
    assert!(!text.starts_with('/'));
}

#[test]
fn forged_commands_and_human_origins_are_typed_refusals() {
    for origin in [
        Producer::Human,
        Producer::Agent("sender".into()),
        Producer::Channel {
            binding: "chat".into(),
            user: "user".into(),
        },
        Producer::System("wake".into()),
        Producer::Fork("child".into()),
    ] {
        let root = Root::new();
        let mut q = queue(&root);
        let mut command = input(0);
        command.kind = Kind::Command;
        command.body = "/compact".into();
        let error = q
            .enqueue(command, Principal::Origin(origin), Gate::Send)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::OperatorOnly);
        assert_eq!(q.journal().replay(0).unwrap().through, 0);
    }
}

#[test]
fn unsupported_modes_unknown_types_and_invalid_metadata_do_not_commit() {
    let root = Root::new();
    let mut q = queue(&root);
    for field in ["steer", "interrupt", "urgent", "kind", "producer"] {
        let mut i = input(0);
        let origin = match field {
            "steer" => {
                i.delivery = Delivery::Steer;
                Principal::Operator
            }
            "interrupt" => {
                i.delivery = Delivery::Interrupt;
                Principal::Operator
            }
            "urgent" => {
                i.priority = Priority::Urgent;
                Principal::Operator
            }
            "kind" => {
                i.kind = Kind::Unknown(json!("future"));
                Principal::Operator
            }
            _ => Principal::Origin(Producer::Unknown(json!("future"))),
        };
        assert_eq!(
            q.enqueue(i, origin, Gate::Send).unwrap_err().code,
            ErrorCode::QueueModeUnsupported
        );
    }
    for key in [
        "gate",
        "batch_id",
        "awaiting_result",
        "refusal",
        "stash",
        "item_id",
        "producer",
        "kind",
        "state",
        "delivery",
        "priority",
        "body",
        "attachments",
    ] {
        let mut i = input(0);
        i.metadata.insert(key.into(), json!("forged"));
        assert_eq!(
            q.enqueue(i, Principal::Operator, Gate::Send)
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
    }
    let mut bad = input(0);
    bad.item_id = "not-a-uuid".into();
    assert_eq!(
        q.enqueue(bad, Principal::Operator, Gate::Send)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    for kind in [Kind::Reply, Kind::Continuation, Kind::Command] {
        let mut i = input(0);
        i.kind = kind;
        assert_eq!(
            q.enqueue(i, Principal::Operator, Gate::Send)
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
    }
    assert_eq!(
        q.enqueue(input(0), Principal::Operator, Gate::Stash(String::new()))
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(q.journal().replay(0).unwrap().through, 0);
}

#[test]
fn replies_name_the_question_and_attachments_stay_literal_refs() {
    let root = Root::new();
    let mut q = queue(&root);
    let mut i = input(0);
    i.kind = Kind::Reply;
    i.body = "answer".into();
    i.attachments = vec!["blob:abc".into()];
    i.metadata
        .insert("anchor".into(), json!({"question_id":"question-1"}));
    q.enqueue(
        i,
        Principal::Origin(Producer::Agent("sender".into())),
        Gate::Send,
    )
    .unwrap();
    assert_eq!(q.batch(100).unwrap().unwrap().envelope["message"]["content"][0]["text"],
        "From {\"agent\":\"sender\"}:\nAnswer to question-1:\nanswer\nAttachments (blob refs): [\"blob:abc\"]");
}

#[test]
fn count_and_byte_limits_keep_a_consecutive_prefix() {
    let root = Root::new();
    let mut q = queue(&root);
    for n in 0..3 {
        let mut i = input(n);
        i.body = "x".repeat(900_000);
        q.enqueue(i, Principal::Operator, Gate::Send).unwrap();
    }
    let batch = q.batch(100).unwrap().unwrap();
    assert_eq!(
        batch
            .items
            .iter()
            .map(|i| i.item_id.clone())
            .collect::<Vec<_>>(),
        [id(0), id(1)]
    );
    assert!(encode(&batch.envelope).is_ok());
    let mut members = batch.items;
    for i in &mut members {
        i.state = State::Done;
    }
    q.replace(members).unwrap();
    assert_eq!(q.batch(100).unwrap().unwrap().id, id(2));
}

#[test]
fn stdin_escaping_budget_is_checked_before_acceptance() {
    let root = Root::new();
    let mut q = queue(&root);
    let mut i = input(0);
    i.attachments = vec!["\\".repeat(600_000)];
    assert!(encode(&Record {
        seq: 1,
        change: Change::Upsert {
            item: Box::new(i.clone()),
            position: 0
        }
    })
    .is_ok());
    assert_eq!(
        q.enqueue(i, Principal::Operator, Gate::Send)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(q.journal().replay(0).unwrap().through, 0);
}

#[test]
fn future_lifecycle_record_budget_is_reserved_before_acceptance() {
    let root = Root::new();
    let mut q = queue(&root);
    let mut i = input(0);
    i.body.clear();
    i.metadata.insert("gate".into(), json!({"decision":"send"}));
    let empty = encode(&Record {
        seq: 1,
        change: Change::Upsert {
            item: Box::new(i.clone()),
            position: 0,
        },
    })
    .unwrap()
    .len();
    i.body = "x".repeat(LINE_LIMIT as usize - empty);
    assert_eq!(
        encode(&Record {
            seq: 1,
            change: Change::Upsert {
                item: Box::new(i.clone()),
                position: 0
            }
        })
        .unwrap()
        .len(),
        LINE_LIMIT as usize
    );
    i.metadata.remove("gate");
    assert_eq!(
        q.enqueue(i, Principal::Operator, Gate::Send)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(q.journal().replay(0).unwrap().through, 0);
}

#[test]
fn missing_metadata_is_ineligible_and_future_journals_are_refused() {
    let root = Root::new();
    let mut j = root.open();
    j.append(upsert("legacy", 0)).unwrap();
    drop(j);
    assert!(queue(&root).batch(100).unwrap().is_none());
    let future = Root::new();
    drop(future.open());
    future.write_records(encode(&json!({"seq":1,"kind":"future"})).unwrap());
    assert!(Admission::new(future.open(), MAX_BATCH).is_err());
    let empty = Root::new();
    assert!(Admission::new(empty.open(), 0).is_err());
}

#[test]
fn atomic_batch_rejects_insertions_without_partial_changes_or_cursor_spend() {
    let root = Root::new();
    let mut j = root.open();
    j.append(upsert("a", 0)).unwrap();
    j.append(upsert("b", 1)).unwrap();
    let before = fs::read(root.path()).unwrap();
    let mut updated = j.items()[0].clone();
    updated.body = "changed".into();
    assert!(j
        .append(Change::Batch {
            items: vec![updated, item("missing")]
        })
        .is_err());
    assert_eq!(fs::read(root.path()).unwrap(), before);
    assert_eq!(j.items()[0].body, "text for a");
    assert_eq!(j.replay(0).unwrap().through, 2);
}

#[test]
fn batch_sync_failure_poisons_without_publishing_any_member() {
    let root = Root::new();
    let mut j = root.open();
    j.append(upsert("a", 0)).unwrap();
    j.append(upsert("b", 1)).unwrap();
    let mut items = j.items().to_vec();
    for i in &mut items {
        i.state = State::Delivered;
    }
    let (socket, _peer) = socket_file();
    j.file = socket;
    assert!(j.append(Change::Batch { items }).is_err());
    assert!(j.replay(0).is_err());
    assert!(j.items().iter().all(|i| i.state == State::Queued));
    drop(j);
    assert!(root.open().items().iter().all(|i| i.state == State::Queued));
}

#[test]
fn batch_recovery_and_unknown_kind_reader_keep_later_positions_valid() {
    for knows_batch in [true, false] {
        let root = Root::new();
        let mut j = root.open();
        j.append(upsert("a", 0)).unwrap();
        j.append(upsert("b", 1)).unwrap();
        let mut changed = j.items().to_vec();
        for i in &mut changed {
            i.state = State::Delivered;
        }
        j.append(Change::Batch { items: changed }).unwrap();
        j.append(upsert("c", 2)).unwrap();
        j.append(Change::Remove {
            item_id: "a".into(),
        })
        .unwrap();
        drop(j);
        if !knows_batch {
            let bytes = fs::read_to_string(root.path()).unwrap();
            fs::write(
                root.path(),
                bytes.replace("\"kind\":\"batch\"", "\"kind\":\"future_batch\""),
            )
            .unwrap();
        }
        let mut reopened = root.open();
        assert_eq!(
            reopened
                .items()
                .iter()
                .map(|i| i.item_id.as_str())
                .collect::<Vec<_>>(),
            ["b", "c"]
        );
        assert_eq!(reopened.replay(0).unwrap().through, 5);
        assert_eq!(
            reopened.items()[0].state,
            if knows_batch {
                State::Delivered
            } else {
                State::Queued
            }
        );
        if !knows_batch {
            assert!(reopened.checkpoint().is_err());
        } else {
            reopened.checkpoint().unwrap();
        }
    }
}

#[test]
fn standalone_native_command_has_no_label_or_extra_blocks() {
    let root = Root::new();
    let mut q = queue(&root);
    let mut item = input(0);
    item.kind = Kind::Command;
    item.body = "/compact".into();
    q.enqueue(item, Principal::Operator, Gate::Send).unwrap();
    assert_eq!(
        q.batch(100).unwrap().unwrap().envelope["message"]["content"],
        json!([{"type":"text","text":"/compact"}])
    );
}

#[test]
fn standalone_agent_slash_text_cannot_form_a_harness_command() {
    let root = Root::new();
    let mut q = queue(&root);
    let mut item = input(0);
    item.kind = Kind::Message;
    item.body = "/compact".into();
    q.enqueue(
        item,
        Principal::Origin(Producer::Agent("sender".into())),
        Gate::Send,
    )
    .unwrap();
    let batch = q.batch(100).unwrap().unwrap();
    let content = batch.envelope["message"]["content"].as_array().unwrap();
    assert_eq!(content.len(), 1);
    assert!(content[0]["text"]
        .as_str()
        .unwrap()
        .starts_with("From {\"agent\":\"sender\"}:\n/compact\n"));
    assert_eq!(batch.items[0].kind, Kind::Message);
}

#[test]
fn single_candidate_uses_exact_item_uuid() {
    let root = Root::new();
    let mut q = queue(&root);
    send(&mut q, 0, Producer::Human);
    assert_eq!(q.batch(100).unwrap().unwrap().id, id(0));
}

#[test]
fn multiple_candidate_gets_a_fresh_batch_uuid() {
    let root = Root::new();
    let mut q = queue(&root);
    send(&mut q, 0, Producer::Human);
    send(&mut q, 1, Producer::Human);
    let first = q.batch(100).unwrap().unwrap();
    let second = q.batch(100).unwrap().unwrap();
    assert_eq!(first.items.len(), 2);
    assert_ne!(first.id, id(0));
    assert_ne!(first.id, id(1));
    assert_ne!(first.id, second.id);
    assert_eq!(first.envelope["uuid"], first.id);
}
