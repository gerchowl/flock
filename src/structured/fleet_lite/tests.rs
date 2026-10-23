use super::*;
use crate::api::agent_state::{Attention, Phase, Turn};
use crate::structured::{
    permissions::Book,
    registry::{Registry, StructuredRuntime},
    state::{Input, SessionState},
};
use std::time::{Duration, Instant};

struct Agent {
    record: Record,
    state: AgentState,
    activity: Option<String>,
    waiting: Vec<String>,
}
fn agent(id: &str) -> Agent {
    Agent {
        record: Record {
            runtime: StructuredRuntime::Structured,
            agent_id: id.into(),
            name: Some(format!("label-{id}")),
            harness: "claude".into(),
            session_id: format!("session-{id}"),
            cwd: crate::test_support::unique_temp_path("fleet-cwd"),
            model: None,
        },
        state: AgentState::unknown(),
        activity: None,
        waiting: Vec::new(),
    }
}
struct Source(Vec<Agent>);
impl Provider for Source {
    fn agents(&self) -> io::Result<Vec<StructuredAgent<'_>>> {
        Ok(self
            .0
            .iter()
            .map(|a| StructuredAgent {
                record: &a.record,
                state: &a.state,
                activity: a.activity.as_deref(),
                waiting_on_human: &a.waiting,
            })
            .collect())
    }
}
fn path() -> PathBuf {
    let dir = crate::test_support::unique_temp_path("fleet-core");
    fs::create_dir_all(&dir).unwrap();
    dir.join("fleet.json")
}
fn disk(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn registry_s4_and_s6_project_into_the_generated_structured_only_contract() {
    let mut registry = Registry::default();
    let record = agent("a").record;
    registry.register(record).unwrap();
    let (record, cold, _) = registry.get("a").unwrap();
    assert_eq!(cold.update.state.phase, Phase::Offline);
    let request = json!({"type":"control_request","request_id":"p1", "request":{
        "subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"t1","input":{"command":"true"}}});
    let event = super::super::claude::SessionEvent::parse(&request.to_string(), "fixture-version");
    let mut reducer = SessionState::new("a".into());
    let update = reducer.reduce(
        Input::Event(&event),
        Instant::now(),
        100,
        Duration::from_secs(1),
    );
    assert!(matches!(
        update.state.attention,
        Attention::NeedsHuman { .. }
    ));
    let mut book = Book::new("a".into());
    book.observe(
        "p1",
        serde_json::from_value(request["request"].clone()).unwrap(),
    )
    .unwrap();
    let waiting: Vec<_> = book.waiting().into_iter().map(|(id, _)| id).collect();
    let row = project(StructuredAgent {
        record,
        state: &update.state,
        activity: Some("Reading source"),
        waiting_on_human: &waiting,
    });
    assert_eq!(row.state, update.state);
    assert_eq!(row.waiting_on_human, vec!["p1"]);
    assert_eq!(row.label, "label-a");
    assert_eq!(row.activity.as_deref(), Some("Reading source"));
    let wire = serde_json::to_value(FleetDelta::Upsert { row: row.clone() }).unwrap();
    assert_eq!(wire["type"], "fleet.upsert");
    assert_eq!(wire["row"]["id"], "a");
    assert_eq!(
        serde_json::from_value::<FleetDelta>(wire.clone()).unwrap(),
        FleetDelta::Upsert { row }
    );
    let keys: BTreeSet<_> = wire["row"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        BTreeSet::from(["id", "label", "state", "activity", "waiting_on_human"])
    );
    let mut unnamed = agent("unnamed");
    unnamed.record.name = None;
    let source = Source(vec![unnamed]);
    assert_eq!(project(source.agents().unwrap().remove(0)).label, "unnamed");
}

#[test]
fn membership_activity_attention_and_removal_generate_dense_deltas() {
    let path = path();
    let mut store = Store::open(path.clone()).unwrap();
    let mut source = Source(vec![agent("b"), agent("a")]);
    let first = store.refresh(&source).unwrap();
    assert_eq!(first.iter().map(|d| d.seq).collect::<Vec<_>>(), vec![1, 2]);
    assert!(matches!(&first[0].event, FleetDelta::Upsert { row } if row.id == "a"));
    assert!(store.refresh(&source).unwrap().is_empty());
    source.0[1].activity = Some("Inspecting changes".into());
    source.0[1].waiting = vec!["permission-1".into()];
    source.0[1].state.turn = Turn::Working;
    let delta = store.refresh(&source).unwrap();
    assert_eq!(delta.len(), 1);
    assert_eq!(delta[0].seq, 3);
    assert!(
        matches!(&delta[0].event, FleetDelta::Upsert { row } if row.activity.as_deref() == Some("Inspecting changes") && row.waiting_on_human == ["permission-1"] && row.state.turn == Turn::Working)
    );
    source.0[1].activity = None;
    source.0[1].waiting.clear();
    let clear = store.refresh(&source).unwrap();
    assert_eq!(clear[0].seq, 4);
    assert!(
        matches!(&clear[0].event, FleetDelta::Upsert { row } if row.activity.is_none() && row.waiting_on_human.is_empty())
    );
    source.0.remove(0);
    assert_eq!(
        store.refresh(&source).unwrap(),
        vec![Delta {
            seq: 5,
            event: FleetDelta::Remove { id: "b".into() }
        }]
    );
    assert_eq!(disk(&path)["high_water"], 5);
    assert_eq!(disk(&path)["rows"].as_array().unwrap().len(), 1);
}

#[test]
fn empty_and_nonzero_empty_snapshots_and_restart_never_reset_the_high_water() {
    let path = path();
    let mut store = Store::open(path.clone()).unwrap();
    assert_eq!(
        store.snapshot().unwrap(),
        Snapshot {
            to: 0,
            snapshot_to: 0,
            rows: Vec::new()
        }
    );
    assert_eq!(disk(&path)["high_water"], 0);
    let mut source = Source(vec![agent("a")]);
    assert_eq!(store.refresh(&source).unwrap()[0].seq, 1);
    let cut = store.snapshot().unwrap();
    assert_eq!((cut.to, cut.snapshot_to), (1, 2));
    assert_eq!(cut.rows[0].seq, 2);
    source.0.clear();
    assert_eq!(store.refresh(&source).unwrap()[0].seq, 3);
    assert_eq!(
        store.snapshot().unwrap(),
        Snapshot {
            to: 3,
            snapshot_to: 3,
            rows: Vec::new()
        }
    );
    assert_eq!(
        cut.rows.len(),
        1,
        "an owned read cut cannot change after removal"
    );
    drop(store);
    let mut restored = Store::open(path.clone()).unwrap();
    assert_eq!(
        restored.snapshot().unwrap(),
        Snapshot {
            to: 3,
            snapshot_to: 3,
            rows: Vec::new()
        }
    );
    source.0.push(agent("new"));
    assert_eq!(restored.refresh(&source).unwrap()[0].seq, 4);
    assert_eq!(disk(&path)["high_water"], 4);
}

#[test]
fn restart_restores_rows_and_checkpoints_consume_dense_positions() {
    let path = path();
    let mut store = Store::open(path.clone()).unwrap();
    let source = Source(vec![agent("b"), agent("a")]);
    store.refresh(&source).unwrap();
    drop(store);
    let mut restored = Store::open(path.clone()).unwrap();
    assert!(restored.refresh(&source).unwrap().is_empty());
    let snapshot = restored.snapshot().unwrap();
    assert_eq!((snapshot.to, snapshot.snapshot_to), (2, 4));
    assert_eq!(
        snapshot.rows.iter().map(|d| d.seq).collect::<Vec<_>>(),
        vec![3, 4]
    );
    assert!(matches!(&snapshot.rows[0].event, FleetDelta::Upsert { row } if row.id == "a"));
    assert_eq!(disk(&path)["high_water"], 4);
}

#[test]
fn unknown_invalid_duplicate_rows_are_skipped_warned_and_preserved_on_save() {
    let path = path();
    let mut source = Source(vec![agent("a")]);
    let mut known = serde_json::to_value(project(source.agents().unwrap().remove(0))).unwrap();
    known["future_field"] = json!({"keep":true});
    known["state"]["phase"] = json!("future_phase");
    let unknown = json!({"future_kind":"not_a_row","payload":[1,2]});
    let invalid = json!({"id":"broken","label":12});
    let mut duplicate = known.clone();
    duplicate["label"] = json!("shadow");
    fs::write(
        &path,
        serde_json::to_vec(&json!({"high_water":41,"future_header":true,
        "rows":[known,unknown,invalid,duplicate]}))
        .unwrap(),
    )
    .unwrap();
    let mut result = None;
    let logs = crate::logging::capture_logs(|| {
        result = Some(Store::open(path.clone()).unwrap());
    });
    assert_eq!(logs.matches("skipping and retaining").count(), 3);
    assert!(!logs.contains("payload"));
    let mut store = result.unwrap();
    let snapshot = store.snapshot().unwrap();
    assert_eq!((snapshot.to, snapshot.snapshot_to), (41, 42));
    assert_eq!(snapshot.rows.len(), 1);
    assert!(
        matches!(&snapshot.rows[0].event, FleetDelta::Upsert { row } if row.label == "label-a" && row.state.phase == Phase::Unknown)
    );
    let saved = disk(&path);
    assert_eq!(saved["future_header"], true);
    assert_eq!(saved["rows"][0]["future_field"], json!({"keep":true}));
    assert!(saved["rows"].as_array().unwrap().contains(&unknown));
    assert!(saved["rows"].as_array().unwrap().contains(&invalid));
    assert!(saved["rows"].as_array().unwrap().contains(&duplicate));
    source.0[0].activity = Some("New observation".into());
    store.refresh(&source).unwrap();
    let updated = disk(&path);
    assert!(updated["rows"].as_array().unwrap().contains(&duplicate));
    assert!(updated["rows"].as_array().unwrap().contains(&unknown));
    assert_eq!(updated["rows"][0]["future_field"], json!({"keep":true}));
    drop(store);
    let mut restored = Store::open(path.clone()).unwrap();
    assert_eq!(restored.snapshot().unwrap().rows.len(), 1);
    restored.refresh(&Source(Vec::new())).unwrap();
    drop(restored);
    let mut removed = Store::open(path.clone()).unwrap();
    assert!(
        removed.snapshot().unwrap().rows.is_empty(),
        "retained shadows cannot resurrect a removed row"
    );
    assert!(disk(&path)["rows"].as_array().unwrap().contains(&unknown));
}

#[test]
fn malformed_cursor_metadata_refuses_loading_instead_of_resetting() {
    for document in [
        json!({"rows":[]}),
        json!({"high_water":"new-format","rows":[]}),
        json!({"high_water":12,"rows":{}}),
    ] {
        let path = path();
        let bytes = serde_json::to_vec(&document).unwrap();
        fs::write(&path, &bytes).unwrap();
        assert_eq!(
            Store::open(path.clone()).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn exclusive_writer_is_released_on_drop_without_losing_the_cursor() {
    let path = path();
    let mut store = Store::open(path.clone()).unwrap();
    store.refresh(&Source(vec![agent("a")])).unwrap();
    assert_eq!(
        Store::open(path.clone()).err().unwrap().kind(),
        io::ErrorKind::WouldBlock
    );
    drop(store);
    assert_eq!(Store::open(path).unwrap().snapshot().unwrap().to, 1);
}

#[test]
fn failed_write_publishes_nothing_and_requires_reopen() {
    let path = path();
    let mut store = Store::open(path.clone()).unwrap();
    store.snapshot().unwrap();
    fs::create_dir(path.with_extension("tmp")).unwrap();
    assert!(store.refresh(&Source(vec![agent("a")])).is_err());
    assert_eq!(disk(&path)["high_water"], 0);
    assert!(store.rows.is_empty());
    assert!(store.snapshot().is_err());
    fs::remove_dir(path.with_extension("tmp")).unwrap();
    assert!(store.refresh(&Source(vec![agent("a")])).is_err());
    drop(store);
    assert_eq!(
        Store::open(path)
            .unwrap()
            .refresh(&Source(vec![agent("a")]))
            .unwrap()[0]
            .seq,
        1
    );
}

#[test]
fn exhausted_sequence_refuses_without_replacing_durable_state() {
    let path = path();
    fs::write(&path, json!({"high_water":u64::MAX,"rows":[]}).to_string()).unwrap();
    let mut store = Store::open(path.clone()).unwrap();
    assert!(store.refresh(&Source(vec![agent("a")])).is_err());
    assert_eq!(disk(&path)["high_water"], u64::MAX);
    assert_eq!(store.snapshot().unwrap().snapshot_to, u64::MAX);
}

#[test]
fn unavailable_or_invalid_provider_does_not_remove_existing_members() {
    struct Unavailable;
    impl Provider for Unavailable {
        fn agents(&self) -> io::Result<Vec<StructuredAgent<'_>>> {
            Err(io::Error::other("fixture unavailable"))
        }
    }
    let path = path();
    let mut store = Store::open(path.clone()).unwrap();
    store.refresh(&Source(vec![agent("a")])).unwrap();
    assert!(store.refresh(&Unavailable).is_err());
    assert!(store
        .refresh(&Source(vec![agent("a"), agent("a")]))
        .is_err());
    let mut invalid = agent("bad");
    invalid.record.name = Some(String::new());
    assert!(store.refresh(&Source(vec![invalid])).is_err());
    assert_eq!(disk(&path)["high_water"], 1);
    assert_eq!(store.snapshot().unwrap().rows.len(), 1);
}
