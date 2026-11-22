use super::*;

const FINISHED: &[u8] = include_bytes!("../../tests/fixtures/codex/finished.jsonl");
const RUNNING: &[u8] = include_bytes!("../../tests/fixtures/codex/in-progress.jsonl");

fn reducer() -> Reducer {
    Reducer::new(256 * 1024, 256, None)
}

fn event(kind: &str, id: &str) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&serde_json::json!({
        "type": "event_msg", "payload": {"type": kind, "turn_id": id}
    }))
    .unwrap();
    bytes.push(b'\n');
    bytes
}

fn started(now: Instant) -> Reducer {
    let mut reducer = reducer();
    reducer.feed(&event("task_started", "t1"), now);
    assert_eq!(reducer.observe(now).turn, Turn::Working);
    reducer
}

#[test]
fn both_start_and_completion_aliases_reduce_independently() {
    let now = Instant::now();
    for start in ["task_started", "turn_started"] {
        for complete in ["task_complete", "turn_complete"] {
            let mut reducer = reducer();
            reducer.feed(&event(start, "opaque-a"), now);
            assert_eq!(reducer.observe(now).turn, Turn::Working);
            reducer.feed(&event(complete, "opaque-a"), now);
            assert_eq!(reducer.observe(now).turn, Turn::Idle);
            assert_eq!(reducer.observe(now).boundary.unwrap().kind, Kind::Complete);
        }
    }
}

#[test]
fn synthetic_rollout_reduces_identically_at_every_byte_split() {
    let now = Instant::now();
    for split in 0..=FINISHED.len() {
        let mut reducer = reducer();
        reducer.feed(&FINISHED[..split], now);
        assert_split_prefix(FINISHED, split, &reducer, None, now);
        reducer.feed(&FINISHED[split..], now);
        let observation = reducer.observe(now);
        assert_eq!(observation.turn, Turn::Idle, "split {split}");
        assert_eq!(
            observation.boundary.unwrap().complete_offset,
            FINISHED.len() as u64
        );
        reducer.feed(RUNNING, now);
        assert_eq!(reducer.observe(now).turn, Turn::Working);
    }
}

#[test]
fn partial_completion_retains_working_until_newline() {
    let now = Instant::now();
    let mut reducer = started(now);
    let end = event("task_complete", "t1");
    reducer.feed(&end[..end.len() - 1], now);
    assert_eq!(reducer.observe(now).turn, Turn::Working);
    assert_eq!(reducer.observe(now).boundary.unwrap().kind, Kind::Start);
    reducer.feed(b"", now);
    assert_eq!(reducer.observe(now).turn, Turn::Working);
    reducer.feed(b"\n", now);
    assert_eq!(reducer.observe(now).turn, Turn::Idle);
}

#[test]
fn any_unterminated_record_masks_idle_even_when_json_is_complete() {
    let now = Instant::now();
    for pending in [
        event("task_started", "t2"),
        event("task_complete", "t1"),
        b"{\"type\":\"future_record\",\"payload\":{}}\n".to_vec(),
        b"{\n".to_vec(),
    ] {
        let mut reducer = started(now);
        reducer.feed(&event("task_complete", "t1"), now);
        reducer.feed(&pending[..pending.len() - 1], now);
        assert_eq!(reducer.observe(now).turn, Turn::Unknown);
        assert_eq!(reducer.observe(now).reason, Some(Reason::PartialRecord));
        assert_eq!(
            reducer.observe(now).reason.unwrap().as_str(),
            "codex_partial_record"
        );
    }
    let mut reducer = started(now);
    reducer.feed(&event("task_complete", "t1"), now);
    let next = event("task_started", "t2");
    reducer.feed(&next[..next.len() - 1], now);
    reducer.feed(b"\n", now);
    assert_eq!(reducer.observe(now).turn, Turn::Working);
}

#[test]
fn duplicate_boundaries_and_late_old_records_do_not_advance_the_cursor() {
    let now = Instant::now();
    let later = now + Duration::from_secs(3600);
    let mut reducer = started(now);
    let first = reducer.observe(now).boundary.unwrap().clone();
    reducer.feed(&event("turn_started", "t1"), later);
    assert_eq!(reducer.observe(later).boundary, Some(&first));
    reducer.feed(&event("task_complete", "t1"), later);
    let completed = reducer.observe(later).boundary.unwrap().clone();
    reducer.feed(
        &event("turn_complete", "t1"),
        later + Duration::from_secs(1),
    );
    assert_eq!(reducer.observe(later).boundary, Some(&completed));
    reducer.feed(&event("task_started", "not-contiguous-999"), later);
    reducer.feed(&event("task_complete", "not-contiguous-999"), later);
    reducer.feed(&event("task_started", "another-opaque-id"), later);
    let active = reducer.observe(later).boundary.unwrap().clone();
    for kind in ["task_started", "task_complete", "turn_aborted"] {
        reducer.feed(&event(kind, "t1"), later);
        assert_eq!(reducer.observe(later).turn, Turn::Working);
        assert_eq!(reducer.observe(later).boundary, Some(&active));
    }
}

#[test]
fn unknown_and_non_boundary_events_never_change_the_parent_turn() {
    let now = Instant::now();
    let mut reducer = started(now);
    let boundary = reducer.observe(now).boundary.unwrap().clone();
    for kind in [
        "error",
        "exec_command_begin",
        "exec_command_end",
        "mcp_tool_call_begin",
        "agent_message",
        "token_count",
        "compaction",
        "subagent_complete",
        "future_event",
    ] {
        reducer.feed(
            &event(kind, "different-child-turn"),
            now + Duration::from_secs(1),
        );
        assert_eq!(reducer.observe(now).turn, Turn::Working, "{kind}");
        assert_eq!(reducer.observe(now).boundary, Some(&boundary));
    }
    reducer.feed(b"{\"type\":\"future_record\"}\n", now);
    assert_eq!(reducer.observe(now).turn, Turn::Working);
    for kind in [
        "response_item",
        "compacted",
        "turn_context",
        "future_record",
    ] {
        let record = format!("{{\"type\":\"{kind}\",\"payload\":{{\"type\":\"task_complete\",\"turn_id\":\"t1\"}}}}\n");
        reducer.feed(record.as_bytes(), now);
        assert_eq!(reducer.observe(now).turn, Turn::Working);
        assert_eq!(reducer.observe(now).boundary, Some(&boundary));
    }
}

#[test]
fn malformed_complete_records_invalidate_and_cannot_silently_recover() {
    let now = Instant::now();
    for invalid in [
        b"{\n".as_slice(),
        b"\xff\n",
        b"null\n",
        b"{}\n",
        b"\n",
        b"{\"type\":\"event_msg\",\"payload\":{}}\n",
        b"{\"type\":\"event_msg\",\"payload\":42}\n",
    ] {
        let mut reducer = started(now);
        reducer.feed(invalid, now);
        assert_eq!(reducer.observe(now).turn, Turn::Unknown, "{invalid:?}");
        assert_eq!(reducer.observe(now).reason, Some(Reason::MalformedRecord));
        reducer.feed(&event("task_complete", "t1"), now);
        reducer.feed(&event("task_started", "new"), now);
        assert_eq!(reducer.observe(now).turn, Turn::Unknown);
    }
}

#[test]
fn conflicting_start_complete_and_abort_ids_invalidate() {
    let now = Instant::now();
    for kind in ["task_started", "task_complete", "turn_aborted"] {
        let mut reducer = started(now);
        reducer.feed(&event(kind, "unrecognized"), now);
        assert_eq!(reducer.observe(now).turn, Turn::Unknown);
        assert_eq!(reducer.observe(now).reason, Some(Reason::ConflictingTurn));
    }
}

#[test]
fn matching_abort_is_idle_but_unattributed_abort_is_unknown() {
    let now = Instant::now();
    let mut reducer = started(now);
    reducer.feed(&event("turn_aborted", "t1"), now);
    assert_eq!(reducer.observe(now).turn, Turn::Idle);
    assert_eq!(reducer.observe(now).boundary.unwrap().kind, Kind::Abort);
    for missing in [
        serde_json::Value::Null,
        serde_json::json!(""),
        serde_json::json!(123),
    ] {
        let mut reducer = started(now);
        let record = format!("{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"turn_aborted\",\"turn_id\":{missing}}}}}\n");
        reducer.feed(record.as_bytes(), now);
        assert_eq!(reducer.observe(now).turn, Turn::Unknown);
        assert_eq!(reducer.observe(now).reason, Some(Reason::UnattributedAbort));
    }
}

#[test]
fn terminal_failure_completion_ends_the_turn_but_an_error_notice_does_not() {
    let now = Instant::now();
    let mut reducer = started(now);
    reducer.feed(&event("error", "t1"), now);
    assert_eq!(reducer.observe(now).turn, Turn::Working);
    reducer.feed(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"t1\",\"error\":\"failed\"}}\n", now);
    assert_eq!(reducer.observe(now).turn, Turn::Idle);
}

#[test]
fn supplied_clock_age_has_no_working_or_idle_silence_lease() {
    let now = Instant::now();
    let hours = Duration::from_secs(24 * 3600);
    let mut reducer = started(now);
    let boundary = reducer.observe(now).boundary.unwrap().clone();
    reducer.feed(b"", now + hours);
    assert_eq!(reducer.observe(now + hours).turn, Turn::Working);
    assert_eq!(reducer.observe(now + hours).last_evidence_age, Some(hours));
    assert_eq!(reducer.observe(now + hours).boundary, Some(&boundary));
    reducer.feed(&event("task_complete", "t1"), now + hours);
    reducer.feed(b"", now + hours + hours);
    assert_eq!(reducer.observe(now + hours + hours).turn, Turn::Idle);
    assert_eq!(
        reducer.observe(now + hours + hours).last_evidence_age,
        Some(hours)
    );
}

#[test]
fn legacy_records_need_no_ordinal_and_paginated_records_require_exact_continuity() {
    let now = Instant::now();
    let mut legacy = started(now);
    legacy.feed(&event("task_complete", "t1"), now);
    assert_eq!(legacy.observe(now).turn, Turn::Idle);
    for wrong in [
        serde_json::Value::Null,
        serde_json::json!(43),
        serde_json::json!("42"),
        serde_json::json!(-1),
    ] {
        let mut reducer = Reducer::new(4096, 16, Some(41));
        let mut start: Value = serde_json::from_slice(&event("task_started", "t1")).unwrap();
        start["ordinal"] = serde_json::json!(41);
        reducer.feed(format!("{start}\n").as_bytes(), now);
        assert_eq!(reducer.observe(now).turn, Turn::Working);
        let next = serde_json::json!({"type":"future_record", "payload":{}, "ordinal":wrong});
        reducer.feed(format!("{next}\n").as_bytes(), now);
        assert_eq!(reducer.observe(now).turn, Turn::Unknown);
        assert_eq!(
            reducer.observe(now).reason,
            Some(Reason::OrdinalDiscontinuity)
        );
    }
    let mut reducer = Reducer::new(4096, 16, Some(41));
    for (ordinal, kind) in [
        (41, "task_started"),
        (42, "future_event"),
        (43, "task_complete"),
    ] {
        let mut record: Value = serde_json::from_slice(&event(kind, "t1")).unwrap();
        record["ordinal"] = serde_json::json!(ordinal);
        reducer.feed(format!("{record}\n").as_bytes(), now);
    }
    assert_eq!(reducer.observe(now).turn, Turn::Idle);
}

#[test]
fn source_discontinuity_is_sticky_until_verified_resynchronization() {
    let now = Instant::now();
    let mut reducer = started(now);
    reducer.invalidate(Reason::SourceDiscontinuity);
    reducer.feed(&event("task_complete", "t1"), now);
    assert_eq!(reducer.observe(now).turn, Turn::Unknown);
    assert_eq!(
        reducer.observe(now).reason,
        Some(Reason::SourceDiscontinuity)
    );
    // #1663 must verify file/prefix/cursor and the current start before supplying
    // a fresh reducer. Constructing one here models that contract only.
    let resynchronized = started(now);
    assert_eq!(resynchronized.observe(now).turn, Turn::Working);
}

#[test]
fn capacity_failure_neither_allocates_unbounded_partial_data_nor_evicts_old_ids() {
    let now = Instant::now();
    let mut reducer = Reducer::new(128, 1, None);
    reducer.feed(&event("task_started", "t1"), now);
    assert_eq!(reducer.observe(now).turn, Turn::Working);
    reducer.feed(&event("task_complete", "t1"), now);
    reducer.feed(&event("task_started", "t2"), now);
    assert_eq!(reducer.observe(now).reason, Some(Reason::CapacityExceeded));
    let mut reducer = Reducer::new(128, 16, None);
    reducer.feed(&event("task_started", "t1"), now);
    assert_eq!(reducer.observe(now).turn, Turn::Working);
    reducer.feed(&[b'x'; 129], now);
    assert_eq!(reducer.observe(now).turn, Turn::Unknown);
    assert!(reducer.partial.is_empty());
    assert_eq!(reducer.observe(now).reason, Some(Reason::CapacityExceeded));
}

#[test]
fn an_unattributed_completion_or_empty_history_never_supplies_bootstrap_idle() {
    let now = Instant::now();
    let mut reducer = started(now);
    reducer.feed(&event("task_complete", "t1"), now);
    assert_eq!(reducer.observe(now).turn, Turn::Idle);
    let mut fresh = Reducer::new(4096, 16, None);
    assert_eq!(fresh.observe(now).turn, Turn::Unknown);
    assert_eq!(fresh.observe(now).boundary, None);
    fresh.feed(&event("task_complete", "old-history"), now);
    assert_eq!(fresh.observe(now).turn, Turn::Unknown);
    assert_eq!(fresh.observe(now).reason, Some(Reason::ConflictingTurn));
}

#[test]
fn malformed_known_non_boundary_envelopes_invalidate() {
    let now = Instant::now();
    for kind in ["session_meta", "response_item", "compacted", "turn_context"] {
        for payload in [Value::Null, serde_json::json!(42), serde_json::json!([])] {
            let mut reducer = started(now);
            let record = serde_json::json!({"type":kind, "payload":payload});
            reducer.feed(format!("{record}\n").as_bytes(), now);
            assert_eq!(reducer.observe(now).turn, Turn::Unknown, "{record}");
            assert_eq!(reducer.observe(now).reason, Some(Reason::MalformedRecord));
        }
        let mut reducer = started(now);
        reducer.feed(format!("{{\"type\":\"{kind}\"}}\n").as_bytes(), now);
        assert_eq!(reducer.observe(now).reason, Some(Reason::MalformedRecord));
    }
}

/// Independently replay only complete records as the oracle for a split prefix.
/// Partial bytes retain working, but cannot claim idle or a new boundary.
fn assert_split_prefix(
    bytes: &[u8],
    split: usize,
    actual: &Reducer,
    ordinal: Option<u64>,
    now: Instant,
) {
    let complete = bytes[..split]
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |i| i + 1);
    let mut expected = Reducer::new(256 * 1024, 256, ordinal);
    expected.feed(&bytes[..complete], now);
    let expected = expected.observe(now);
    let actual = actual.observe(now);
    assert_eq!(
        actual.boundary, expected.boundary,
        "prefix boundary at {split}"
    );
    if split == complete {
        assert_eq!(actual, expected, "complete prefix at {split}");
    } else {
        assert_eq!(
            actual.turn,
            if expected.turn == Turn::Working {
                Turn::Working
            } else {
                Turn::Unknown
            },
            "partial turn at {split}"
        );
        assert_eq!(
            actual.reason,
            if expected.turn == Turn::Working {
                None
            } else {
                Some(Reason::PartialRecord)
            },
            "partial reason at {split}"
        );
    }
}

#[test]
fn real_01601_rollout_reduces_at_every_byte_split_including_partial_state() {
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/codex-rollout/real-0.160.1.jsonl"
    ))
    .expect("derived real rollout fixture");
    let now = Instant::now();
    let mut checkpoint = Reducer::new(256 * 1024, 256, Some(0));
    let mut offset = 0;
    for record in bytes.split_inclusive(|byte| *byte == b'\n') {
        let mut expected = checkpoint.clone();
        expected.feed(record, now);
        assert_eq!(
            expected.observe(now).reason,
            None,
            "complete record at {offset}"
        );
        for split in 0..=record.len() {
            // Reuse the exactly replayed prefix, then split this record at every
            // byte. Full-state equality after its suffix proves that continuing
            // with the remaining file yields the same final state for every split.
            let mut actual = checkpoint.clone();
            actual.feed(&record[..split], now);
            let prefix = actual.observe(now);
            if split > 0 && split < record.len() {
                assert_eq!(
                    prefix.boundary,
                    checkpoint.observe(now).boundary,
                    "boundary at {}",
                    offset + split
                );
                let working = checkpoint.observe(now).turn == Turn::Working;
                assert_eq!(
                    prefix.turn,
                    if working {
                        Turn::Working
                    } else {
                        Turn::Unknown
                    },
                    "partial turn at {}",
                    offset + split
                );
                assert_eq!(
                    prefix.reason,
                    if working {
                        None
                    } else {
                        Some(Reason::PartialRecord)
                    },
                    "partial reason at {}",
                    offset + split
                );
            } else if split == 0 {
                assert_eq!(actual, checkpoint);
            }
            actual.feed(&record[split..], now);
            assert_eq!(actual, expected, "complete state at {}", offset + split);
        }
        checkpoint = expected;
        offset += record.len();
    }
    let final_state = checkpoint.observe(now);
    assert_eq!(final_state.turn, Turn::Idle);
    assert_eq!(final_state.reason, None);
    assert_eq!(final_state.boundary.unwrap().kind, Kind::Complete);
    assert_eq!(
        final_state.boundary.unwrap().complete_offset,
        bytes.len() as u64
    );
}
