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
    let mut raw: Vec<_> = (0..300).map(|i| json!({"invalid": i})).collect();
    raw.extend((0..300).map(|_| forward()));
    let mut store = registry(Value::Array(raw));
    store.prepare_retained_save(100);
    let saved = serde_json::to_value(&store).unwrap();
    assert_eq!(saved.as_array().unwrap().len(), RETAINED_DEAD_MAX + 300);
    assert_eq!(saved[0], json!({"invalid":44}));
    assert_eq!(saved[255], json!({"invalid":299}));
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
    let raw =
        json!([{"invalid":true}, forward(), {"runtime":"structured","harness":"future-harness"}]);
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
fn retained_explicit_purge_respects_age_kind_missing_stamps_and_live_workers() {
    let known = super::tests::record("agent_fixture_live");
    let mut store =
        registry(json!([serde_json::to_value(&known).unwrap(), {"invalid":true}, forward()]));
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
