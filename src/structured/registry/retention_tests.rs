use super::*;
use serde_json::json;

fn registry(records: Value) -> Registry {
    serde_json::from_value(records).unwrap()
}
fn forward() -> Value {
    json!({"runtime":"future-runtime", "opaque":{"n":1.25,"extra":[true,null]}})
}
#[test]
fn retained_dead_count_prunes_oldest_and_never_caps_forward() {
    let mut raw: Vec<_> = (0..300).map(|i| json!(i)).collect();
    raw.extend((0..300).map(|i| json!({"runtime":"future-runtime", "index":i})));
    let mut store = registry(Value::Array(raw));
    store.prepare_retained_save(100);
    let saved = serde_json::to_value(&store).unwrap();
    assert_eq!(saved.as_array().unwrap().len(), RETAINED_DEAD_MAX + 300);
    assert_eq!(saved[0], json!(44));
    assert_eq!(saved[255], json!(299));
    let info = store.retained_records();
    assert_eq!(
        info.iter()
            .filter(|r| r.kind == RetainedKind::Forward)
            .count(),
        300
    );
    assert!(info.iter().all(|r| r.first_seen_ms == Some(100)));
    assert_eq!(
        info.iter().map(|r| r.slot).collect::<Vec<_>>(),
        (0..556).collect::<Vec<_>>()
    );
}
#[test]
fn retained_dead_age_is_strict_and_clock_rollback_is_safe() {
    let raw = json!([true, forward(), {"runtime":"structured","harness":"future-harness"}]);
    let mut store = registry(raw.clone());
    store.prepare_retained_save(100);
    store.prepare_retained_save(50);
    assert_eq!(serde_json::to_value(&store).unwrap(), raw);
    let age = RETAINED_DEAD_MAX_AGE.as_millis() as u64;
    store.prepare_retained_save(100 + age);
    assert_eq!(serde_json::to_value(&store).unwrap(), raw);
    store.prepare_retained_save(101 + age);
    assert_eq!(
        serde_json::to_value(&store).unwrap(),
        json!([forward(), {"runtime":"structured","harness":"future-harness"}])
    );
    assert!(store
        .retained_records()
        .iter()
        .all(|r| r.kind == RetainedKind::Forward));
}
#[test]
fn retained_stamps_survive_roundtrip_without_changing_raw_record_bytes() {
    let mut store = registry(json!([forward(), {"invalid": 1}]));
    let before = serde_json::to_vec(&store).unwrap();
    store.prepare_retained_save(42);
    assert_eq!(serde_json::to_vec(&store).unwrap(), before);
    let mut sidecar = store.retained_stamps();
    assert!(!sidecar.is_null(), "next save must stamp retained records");
    sidecar[0]["future_metadata"] = json!({"keep":true});
    sidecar
        .as_array_mut()
        .unwrap()
        .push(json!({"future_stamp":[1,2]}));
    let mut restored = registry(serde_json::from_slice(&before).unwrap());
    restored.restore_retained_stamps(&sidecar);
    restored.restore_retained_stamps(&sidecar);
    restored.prepare_retained_save(99);
    assert!(restored
        .retained_records()
        .iter()
        .all(|r| r.first_seen_ms == Some(42)));
    assert_eq!(restored.retained_stamps(), sidecar);
    let copy = restored.snapshot();
    assert_eq!(copy.retained_stamps(), sidecar);
    assert_eq!(serde_json::to_vec(&copy).unwrap(), before);
}
#[test]
fn retained_sidecar_mismatch_does_not_assign_another_records_age() {
    let mut original = registry(json!([{ "invalid": 1 }]));
    original.prepare_retained_save(0);
    let mut changed = registry(json!([{ "invalid": 2 }]));
    changed.restore_retained_stamps(&original.retained_stamps());
    changed.prepare_retained_save(10);
    assert_eq!(changed.retained_records()[0].first_seen_ms, Some(10));
    let sidecar = changed.retained_stamps();
    let mut again = registry(serde_json::to_value(&changed).unwrap());
    again.restore_retained_stamps(&sidecar);
    assert_eq!(again.retained_records()[0].first_seen_ms, Some(10));
}
#[test]
fn retained_unknown_sidecar_shape_is_preserved_without_automatic_pruning() {
    let mut store = registry(json!([{ "invalid": 1 }]));
    let opaque = json!({"future_retention_format":[1,2]});
    store.restore_retained_stamps(&opaque);
    store.prepare_retained_save(u64::MAX);
    assert_eq!(store.retained_stamps(), opaque);
    assert_eq!(store.retained_count(), 1);
    let mut metadata_only = registry(json!([]));
    metadata_only.restore_retained_stamps(&opaque);
    assert!(
        !metadata_only.is_empty(),
        "opaque metadata must survive empty-session cleanup"
    );
}
#[test]
fn retained_non_array_admission_has_typed_actionable_error() {
    let raw = json!({"future_collection":[1,2]});
    let mut store = registry(raw.clone());
    let error = store
        .register(super::tests::record("agent_fixture_repair"))
        .unwrap_err();
    assert_eq!(error, RegistryError::UnsupportedCollectionShape);
    let message = error.to_string();
    assert!(message.contains("session-state repair guide"));
    assert!(message.contains("back up session.json"));
    store.prepare_retained_save(u64::MAX);
    assert_eq!(serde_json::to_value(store).unwrap(), raw);
}
#[test]
fn retained_shadow_duplicates_are_dead_and_removal_reindexes_stamps() {
    let known = super::tests::record("agent_fixture_known");
    let raw = serde_json::to_value(&known).unwrap();
    let mut store = registry(json!([raw, raw, forward()]));
    store.prepare_retained_save(42);
    assert_eq!(store.retained_records()[0].kind, RetainedKind::Dead);
    let generation = store.get(&known.agent_id).unwrap().2;
    store.remove(&known.agent_id, generation).unwrap();
    assert_eq!(store.retained_records()[0].slot, 0);
    let mut restored = registry(serde_json::to_value(&store).unwrap());
    restored.restore_retained_stamps(&store.retained_stamps());
    assert_eq!(restored.retained_records()[0].first_seen_ms, Some(42));
}

#[test]
fn retained_sidecar_sequence_entry_is_preserved_without_becoming_a_stamp() {
    let mut original = registry(json!([forward()]));
    original.prepare_retained_save(42);
    let stamp = original.retained_stamps()[0].clone();
    let sequence = json!([stamp["slot"], stamp["digest"], 7]);
    let mut restored = registry(json!([forward()]));
    restored.restore_retained_stamps(&json!([sequence, stamp]));
    assert_eq!(restored.retained_records()[0].first_seen_ms, Some(42));
    assert_eq!(restored.retained_stamps(), json!([stamp, sequence]));
}

#[test]
fn retained_supported_tags_changed_shape_are_forward_past_age_and_count() {
    let raw: Vec<_> = (0..300)
        .map(|i| {
            json!({
                "runtime":"structured","harness":"claude", "agent_id":format!("future-{i}"),
                "session_id":format!("session-{i}"),"cwd":{"future_path":["workspace"]}
            })
        })
        .collect();
    let mut store = registry(json!(raw));
    assert!(store
        .retained_records()
        .iter()
        .all(|r| r.kind == RetainedKind::Forward));
    store.prepare_retained_save(42);
    store.prepare_retained_save(43 + RETAINED_DEAD_MAX_AGE.as_millis() as u64);
    assert_eq!(serde_json::to_value(&store).unwrap(), json!(raw));
    assert_eq!(store.retained_count(), 300);
}
#[test]
fn retained_non_identical_conflicts_and_odd_identities_are_forward() {
    let record = super::tests::record("agent_fixture_conflict");
    let original = serde_json::to_value(&record).unwrap();
    let mut conflicting = original.clone();
    conflicting["model"] = json!("different-model");
    let mut odd = original.clone();
    odd["agent_id"] = json!("");
    let mut store = registry(json!([original, conflicting, odd]));
    assert_eq!(store.retained_records().len(), 2);
    assert!(store
        .retained_records()
        .iter()
        .all(|r| r.kind == RetainedKind::Forward));
    store.prepare_retained_save(0);
    store.prepare_retained_save(u64::MAX);
    assert_eq!(store.retained_count(), 2);
    assert_eq!(store.records().len(), 1);
}
#[test]
fn retained_exact_forward_duplicates_and_non_objects_are_dead_and_pruned() {
    let raw = forward();
    let sequence = json!([
        "structured",
        "agent_fixture_sequence",
        null,
        "claude",
        "session-sequence",
        "workspace",
        null
    ]);
    let mut store = registry(json!([raw, raw, null, false, 7, "broken", [], sequence]));
    assert!(
        store.records().is_empty(),
        "non-object Record sequences must never activate"
    );
    let info = store.retained_records();
    assert_eq!(info[0].kind, RetainedKind::Forward);
    assert!(info[1..].iter().all(|r| r.kind == RetainedKind::Dead));
    store.prepare_retained_save(42);
    store.prepare_retained_save(43 + RETAINED_DEAD_MAX_AGE.as_millis() as u64);
    assert_eq!(serde_json::to_value(&store).unwrap(), json!([raw]));
}

#[test]
fn retained_explicit_purge_respects_age_kind_missing_stamps_and_live_workers() {
    let known = super::tests::record("agent_fixture_live");
    let mut store = registry(json!([
        serde_json::to_value(&known).unwrap(),
        true,
        forward()
    ]));
    assert_eq!(store.purge_retained(true, Some(0), 100), 0);
    store.prepare_retained_save(42);
    let generation = store.get(&known.agent_id).unwrap().2;
    store
        .apply(
            &known.agent_id,
            generation,
            Input::Lifecycle(Lifecycle::Live),
            Instant::now(),
            42,
            Duration::from_secs(60),
        )
        .unwrap();
    assert_eq!(store.purge_retained(true, Some(10), 52), 0);
    assert_eq!(store.purge_retained(false, Some(10), 53), 1);
    assert_eq!(store.retained_records()[0].kind, RetainedKind::Forward);
    assert_eq!(store.purge_retained(true, Some(10), 53), 1);
    let (_, projection, unchanged_generation) = store.get(&known.agent_id).unwrap();
    assert_eq!(generation, unchanged_generation);
    assert_eq!(projection.update.state.lifecycle, Lifecycle::Live);
    assert_eq!(store.records().len(), 1);
}

#[test]
fn retained_purge_reclassifies_the_last_forward_copy_before_automatic_pruning() {
    let raw = forward();
    let mut store = registry(json!([raw, raw]));
    store.stamp_retained_records(100);
    let mut stamps = store.retained_stamps();
    stamps[1]["first_seen_ms"] = json!(200);
    store.restore_retained_stamps(&stamps);
    assert_eq!(store.purge_retained(true, Some(50), 200), 1);
    assert_eq!(store.retained_records()[0].kind, RetainedKind::Forward);
    store.prepare_retained_save(u64::MAX);
    assert_eq!(serde_json::to_value(&store).unwrap(), json!([raw]));
}

#[test]
fn retained_logs_separate_class_from_each_cause() {
    let original = serde_json::to_value(super::tests::record("agent_fixture_logs")).unwrap();
    let mut conflict = original.clone();
    conflict["model"] = json!("another-model");
    let mut invalid = original.clone();
    invalid["agent_id"] = json!("");
    let logs = crate::logging::capture_logs(|| {
        let _ = registry(json!([
            original,
            true,
            original,
            conflict,
            invalid,
            forward()
        ]));
    });
    let lines: Vec<_> = logs
        .lines()
        .filter(|line| line.contains("structured record skipped"))
        .collect();
    assert_eq!(lines.len(), 5, "{logs}");
    for (line, (class, cause)) in lines.iter().zip([
        ("DEAD", "non_object"),
        ("DEAD", "exact_duplicate"),
        ("FORWARD", "Conflict"),
        ("FORWARD", "InvalidRecord"),
        ("FORWARD", "unsupported_shape"),
    ]) {
        assert!(line.contains(&format!("class={class:?}")), "{line}");
        assert!(line.contains(&format!("cause={cause:?}")), "{line}");
    }
}

#[test]
fn retained_named_stamp_only_keeps_dead_beyond_age_and_count() {
    let raw = json!((0..300).collect::<Vec<_>>());
    let mut store = registry(raw.clone());
    store.stamp_retained_records(0);
    store.stamp_retained_records(u64::MAX);
    assert_eq!(serde_json::to_value(&store).unwrap(), raw);
    assert!(store
        .retained_records()
        .iter()
        .all(|r| r.first_seen_ms == Some(0)));
    store.prepare_retained_save(u64::MAX);
    assert_eq!(store.retained_count(), 0);
}

#[test]
fn retained_swap_installs_saved_state_exactly_without_cold_live_entries() {
    let known = super::tests::record("agent_fixture_swap");
    let mut live = registry(json!([
        serde_json::to_value(&known).unwrap(),
        true,
        forward()
    ]));
    live.stamp_retained_records(42);
    let generation = live.get(&known.agent_id).unwrap().2;
    live.apply(
        &known.agent_id,
        generation,
        Input::Lifecycle(Lifecycle::Live),
        Instant::now(),
        42,
        Duration::from_secs(60),
    )
    .unwrap();
    let mut saved = live.snapshot();
    saved.purge_retained(false, None, 100);
    saved.prepare_retained_save(100);
    let expected = serde_json::to_value(&saved).unwrap();
    let mut sidecar = saved.retained_stamps();
    sidecar[0]["first_seen_ms"] = json!(77);
    sidecar[0]["future_metadata"] = json!({"keep":true});
    sidecar
        .as_array_mut()
        .unwrap()
        .push(json!({"future_sidecar":"keep"}));
    saved.restore_retained_stamps(&sidecar);
    live.swap_retained_state(&mut saved);
    assert_eq!(serde_json::to_value(&live).unwrap(), expected);
    assert_eq!(live.retained_stamps(), sidecar);
    let (_, projection, actual_generation) = live.get(&known.agent_id).unwrap();
    assert_eq!(actual_generation, generation);
    assert_eq!(projection.update.state.lifecycle, Lifecycle::Live);
    // Slot bindings remain valid after installing a snapshot with holes.
    live.remove(&known.agent_id, generation).unwrap();
    assert_eq!(serde_json::to_value(&live).unwrap(), json!([forward()]));
}
