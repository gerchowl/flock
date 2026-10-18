use super::*;
use crate::api::agent_state::{Attention, Background, Phase, StateSource, Turn, Waiting};
use crate::structured::claude::SessionEvent;

fn record(id: &str) -> Record {
    Record {
        runtime: crate::structured::registry::StructuredRuntime::Structured,
        agent_id: id.into(),
        name: Some(format!("name-{id}")),
        harness: "claude".into(),
        session_id: format!("session-{id}"),
        cwd: std::env::temp_dir().join("registry-fixture"),
        model: Some("fixture-model".into()),
    }
}
fn send(registry: &mut Registry, id: &str, generation: u64, event: &SessionEvent, now: Instant) {
    registry
        .apply(
            id,
            generation,
            Input::Event(event),
            now,
            1000,
            Duration::from_secs(60),
        )
        .unwrap();
}
fn fixture(kind: &str) -> SessionEvent {
    let line = include_str!("../../../tests/fixtures/claude-session/events.jsonl")
        .lines()
        .find(|line| {
            let value = serde_json::from_str::<serde_json::Value>(line).unwrap();
            value["type"] == kind && (kind != "command_lifecycle" || value["state"] == "started")
        })
        .unwrap();
    SessionEvent::parse(line, "2.1.296")
}

#[test]
fn fixture_stream_publishes_keyed_s4_state_and_human_facts() {
    let mut registry = Registry::default();
    let a = registry.register(record("agent_fixture_a")).unwrap();
    registry.register(record("agent_fixture_b")).unwrap();
    let now = Instant::now();
    registry
        .apply(
            "agent_fixture_a",
            a,
            Input::Lifecycle(Lifecycle::Live),
            now,
            1000,
            Duration::from_secs(60),
        )
        .unwrap();
    send(
        &mut registry,
        "agent_fixture_a",
        a,
        &fixture("command_lifecycle"),
        now,
    );
    let (_, view, _) = registry.get("agent_fixture_a").unwrap();
    assert_eq!(view.update.state.phase, Phase::Working);
    assert_eq!(view.update.state.source.turn, Some(StateSource::Session));
    assert!(view.update.observe_at.is_some());
    send(
        &mut registry,
        "agent_fixture_a",
        a,
        &fixture("control_request"),
        now,
    );
    let (_, view, _) = registry.get("agent_fixture_a").unwrap();
    assert_eq!(view.update.state.phase, Phase::NeedsHuman);
    assert_eq!(view.update.needs_human.len(), 1);
    assert_eq!(
        view.update.phase_changed,
        Some((Phase::Working, Phase::NeedsHuman))
    );
    send(&mut registry, "agent_fixture_a", a, &fixture("result"), now);
    let (_, view, _) = registry.get("agent_fixture_a").unwrap();
    assert_eq!(view.update.state.turn, Turn::Idle);
    assert!(view.update.needs_human.is_empty());
    assert_eq!(view.update.state.phase, Phase::Settling);
    let (_, other, _) = registry.get("agent_fixture_b").unwrap();
    assert_eq!(other.update.state.phase, Phase::Offline);
    assert_eq!(other.update.state.turn, Turn::Unknown);
    assert!(other.update.needs_human.is_empty());
}

#[test]
fn conflicting_identity_name_and_session_never_replace_an_entry() {
    let mut registry = Registry::default();
    let original = record("agent_fixture_a");
    let generation = registry.register(original.clone()).unwrap();
    for field in ["agent_id", "name", "session_id"] {
        let mut duplicate = record("agent_fixture_b");
        match field {
            "agent_id" => duplicate.agent_id = original.agent_id.clone(),
            "name" => duplicate.name = original.name.clone(),
            _ => duplicate.session_id = original.session_id.clone(),
        }
        assert_eq!(registry.register(duplicate), Err(RegistryError::Conflict));
        assert_eq!(registry.records(), vec![original.clone()]);
        assert_eq!(registry.get(&original.agent_id).unwrap().2, generation);
    }
    for field in ["agent_id", "name", "session_id", "harness"] {
        let mut invalid = record("agent_fixture_b");
        match field {
            "agent_id" => invalid.agent_id.clear(),
            "name" => invalid.name = Some(String::new()),
            "session_id" => invalid.session_id.clear(),
            _ => invalid.harness = "unsupported".into(),
        }
        assert_eq!(
            registry.register(invalid),
            Err(RegistryError::InvalidRecord)
        );
    }
}

#[test]
fn replacement_fences_stale_delivery_and_removal() {
    let mut registry = Registry::default();
    let id = "agent_fixture_a";
    let old = registry.register(record(id)).unwrap();
    assert_eq!(registry.remove(id, old).unwrap(), record(id));
    let new = registry.register(record(id)).unwrap();
    assert_ne!(new, old);
    assert!(matches!(
        registry.apply(
            id,
            old,
            Input::ProcessExited,
            Instant::now(),
            1000,
            Duration::from_secs(60)
        ),
        Err(RegistryError::StaleGeneration)
    ));
    assert_eq!(
        registry.remove(id, old),
        Err(RegistryError::StaleGeneration)
    );
    assert_eq!(registry.get(id).unwrap().2, new);
    assert_eq!(
        registry.get(id).unwrap().1.update.state.phase,
        Phase::Offline
    );
}

#[test]
fn durable_restore_discards_live_observations_and_authority() {
    let mut registry = Registry::default();
    let id = "agent_fixture_a";
    let generation = registry.register(record(id)).unwrap();
    let now = Instant::now();
    registry
        .apply(
            id,
            generation,
            Input::Lifecycle(Lifecycle::Live),
            now,
            1000,
            Duration::from_secs(60),
        )
        .unwrap();
    send(
        &mut registry,
        id,
        generation,
        &fixture("command_lifecycle"),
        now,
    );
    send(
        &mut registry,
        id,
        generation,
        &fixture("control_request"),
        now,
    );
    assert_eq!(
        registry.get(id).unwrap().1.update.state.phase,
        Phase::NeedsHuman
    );
    let encoded = serde_json::to_value(registry.records()).unwrap();
    assert_eq!(encoded[0]["runtime"], "structured");
    let mut wrong_runtime = encoded[0].clone();
    wrong_runtime["runtime"] = "pty".into();
    assert!(serde_json::from_value::<Record>(wrong_runtime.clone()).is_err());
    wrong_runtime.as_object_mut().unwrap().remove("runtime");
    assert!(serde_json::from_value::<Record>(wrong_runtime).is_err());
    for field in [
        "pane_id",
        "terminal_id",
        "workspace_id",
        "tab_id",
        "view",
        "lease",
        "pid",
        "state",
        "observe_at",
        "generation",
    ] {
        assert!(encoded[0].get(field).is_none(), "must not persist {field}");
    }
    let restored = Registry::restore(serde_json::from_value(encoded).unwrap()).unwrap();
    let (metadata, view, _) = restored.get(id).unwrap();
    assert_eq!(metadata, &record(id));
    assert_eq!(view.update.state.lifecycle, Lifecycle::Offline);
    assert_eq!(view.update.state.phase, Phase::Offline);
    assert_eq!(view.update.state.turn, Turn::Unknown);
    assert_eq!(view.update.state.background, Background::Unknown);
    assert_eq!(view.update.state.attention, Attention::Unknown);
    assert_eq!(view.update.state.waiting, Waiting::Unknown);
    assert!(view.update.state.held_ms.is_none());
    assert!(view.update.needs_human.is_empty());
    assert!(view.update.observe_at.is_none());
    assert!(view.update.phase_changed.is_none());
    assert!(view.update.stalled_since_ms.is_none());
    assert!(matches!(
        Registry::restore(vec![record(id), record(id)]),
        Err(RegistryError::Conflict)
    ));
}

#[test]
fn disk_duplicates_keep_first_and_retain_raw_order_after_mutation() {
    let original = record("agent_fixture_a");
    for field in ["agent_id", "name", "session_id"] {
        let mut duplicate = record("agent_fixture_b");
        match field {
            "agent_id" => duplicate.agent_id = original.agent_id.clone(),
            "name" => duplicate.name = original.name.clone(),
            _ => duplicate.session_id = original.session_id.clone(),
        }
        let raw = serde_json::json!([original, duplicate]);
        let mut registry: Registry = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(registry.records(), vec![original.clone()]);
        assert_eq!(registry.retained_count(), 1);
        assert_eq!(serde_json::to_value(registry.snapshot()).unwrap(), raw);
        let extra = record("agent_fixture_c");
        let generation = registry.register(extra.clone()).unwrap();
        registry.remove(&extra.agent_id, generation).unwrap();
        assert_eq!(serde_json::to_value(&registry).unwrap(), raw);
        let generation = registry.get(&original.agent_id).unwrap().2;
        registry.remove(&original.agent_id, generation).unwrap();
        assert!(registry.records().is_empty());
        assert!(registry.is_empty(), "explicit removal also removes shadows");
        let saved = serde_json::to_vec(&registry.snapshot()).unwrap();
        let restored: Registry = serde_json::from_slice(&saved).unwrap();
        assert!(
            restored.records().is_empty(),
            "removed agent must stay gone"
        );
        assert_eq!(registry.preserved.len(), 0, "removed slots are compacted");
    }
}

#[test]
fn opaque_prefix_and_known_extra_fields_survive_save_in_original_order() {
    let mut known = serde_json::to_value(record("agent_fixture_a")).unwrap();
    known["newer_field"] = serde_json::json!({"kept":[1,2]});
    let raw = serde_json::json!([
        {"runtime":"future-runtime","native_data":{"kept":true}}, known, 42,
    ]);
    let registry: Registry = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(registry.records(), vec![record("agent_fixture_a")]);
    assert_eq!(registry.retained_count(), 2);
    assert_eq!(serde_json::to_value(registry.snapshot()).unwrap(), raw);
}

#[test]
fn removal_compacts_slots_and_drops_only_matching_retained_identities() {
    let first = record("agent_fixture_a");
    let second = record("agent_fixture_b");
    for runtime in ["structured", "future-runtime"] {
        for field in ["agent_id", "name", "session_id"] {
            let mut retained = serde_json::to_value(record("agent_fixture_c")).unwrap();
            retained["runtime"] = runtime.into();
            retained[field] = serde_json::to_value(&first).unwrap()[field].clone();
            let opaque = serde_json::json!({"runtime":"future-runtime","opaque":[1,2]});
            let mut registry: Registry =
                serde_json::from_value(serde_json::json!([opaque, first, retained, second]))
                    .unwrap();
            let generation = registry.get(&first.agent_id).unwrap().2;
            registry.remove(&first.agent_id, generation).unwrap();
            let distinct_future = runtime == "future-runtime" && field == "name";
            let expected = if distinct_future {
                serde_json::json!([opaque, retained, second])
            } else {
                serde_json::json!([opaque, second])
            };
            assert_eq!(serde_json::to_value(&registry).unwrap(), expected);
            assert_eq!(registry.preserved.len(), expected.as_array().unwrap().len());
            // Compaction must update the surviving entry's slot before later removal.
            let generation = registry.get(&second.agent_id).unwrap().2;
            registry.remove(&second.agent_id, generation).unwrap();
            let expected = if distinct_future {
                serde_json::json!([opaque, retained])
            } else {
                serde_json::json!([opaque])
            };
            let saved = serde_json::to_vec(&registry).unwrap();
            let restored: Registry = serde_json::from_slice(&saved).unwrap();
            assert!(restored.records().is_empty());
            assert_eq!(serde_json::to_value(restored).unwrap(), expected);
            for _ in 0..100 {
                let mut fresh = first.clone();
                fresh.name = Some("unrelated-name".into());
                let generation = registry.register(fresh.clone()).unwrap();
                registry.remove(&fresh.agent_id, generation).unwrap();
            }
            assert_eq!(
                registry.preserved.len(),
                expected.as_array().unwrap().len(),
                "churn does not accumulate slots"
            );
        }
    }
}

#[test]
fn retained_names_refuse_new_admission_without_changing_raw_data() {
    for runtime in ["structured", "future-runtime"] {
        let mut retained = serde_json::to_value(record("agent_fixture_c")).unwrap();
        retained["runtime"] = runtime.into();
        retained["harness"] = "future-harness".into();
        let raw = serde_json::json!([retained]);
        let mut registry: Registry = serde_json::from_value(raw.clone()).unwrap();
        assert!(registry.records().is_empty());
        let mut fresh = record("agent_fixture_d");
        fresh.name = Some("name-agent_fixture_c".into());
        assert!(matches!(
            registry.register(fresh),
            Err(RegistryError::Conflict)
        ));
        assert_eq!(serde_json::to_value(&registry).unwrap(), raw);
    }
}

#[test]
fn fresh_launch_resets_observations_and_fences_the_previous_generation() {
    let id = "agent_fixture_a";
    let mut registry = Registry::default();
    let old = registry.register(record(id)).unwrap();
    let now = Instant::now();
    send(&mut registry, id, old, &fixture("control_request"), now);
    let raw = serde_json::to_value(&registry).unwrap();
    let current = registry.begin_launch(id, old).unwrap();
    assert_ne!(current, old);
    let (saved, view, generation) = registry.get(id).unwrap();
    assert_eq!(generation, current);
    assert_eq!(saved, &record(id));
    assert!(view.update.needs_human.is_empty());
    assert_eq!(view.update.state.lifecycle, Lifecycle::Offline);
    assert!(view.update.observe_at.is_none());
    assert_eq!(serde_json::to_value(&registry).unwrap(), raw);
    assert!(matches!(
        registry.apply(id, old, Input::Observe, now, 1000, Duration::from_secs(1)),
        Err(RegistryError::StaleGeneration)
    ));
    assert!(matches!(
        registry.begin_launch(id, old),
        Err(RegistryError::StaleGeneration)
    ));
}

#[test]
fn security_agent_name_structured_registry_rejects_and_retains_inactive() {
    let mut failures = Vec::new();
    for (case, name) in crate::test_support::hostile_agent_names() {
        let mut bad = record("agent-name-security");
        bad.name = Some(name);
        let raw = serde_json::json!([bad]);
        let mut registry = Registry::default();
        let refused = registry.register(bad).is_err();
        let restored: Registry = serde_json::from_value(raw.clone()).unwrap();
        if !refused
            || !restored.records().is_empty()
            || restored.is_empty()
            || restored.retained_count() != 1
        {
            failures.push(case);
        }
        assert_eq!(serde_json::to_value(restored).unwrap(), raw);
    }
    assert!(
        failures.is_empty(),
        "accepted structured name cases: {failures:?}"
    );
}
