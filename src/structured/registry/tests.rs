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
        assert!(
            !registry.is_empty(),
            "retained duplicate must not be cleared"
        );
        assert_eq!(
            serde_json::to_value(registry).unwrap(),
            serde_json::json!([duplicate])
        );
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
