use super::*;
use crate::api::schema::{EmptyParams, Request};
use crate::structured::identity::UidPolicy;
use serde_json::{json, Value};
fn operator() -> App {
    let (_, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &crate::config::Config::default(),
        true,
        None,
        rx,
        crate::api::EventHub::default(),
    );
    app.runtime_config.uid_policy = UidPolicy {
        operator: 7,
        dedicated: Ok(Some(42)).into(),
    };
    app.current_api_peer_pid = Some(std::process::id());
    app.current_api_peer_birth = Some(crate::platform::process_start_time(std::process::id()));
    app.current_api_peer_uid = Some(Some(7));
    app
}
fn list() -> Method {
    Method::SessionRetainedList(EmptyParams {})
}
fn purge(selection: Selection, yes: bool) -> Method {
    Method::SessionRetainedPurge(PurgeParams {
        selection,
        yes,
        older_than_ms: None,
    })
}
fn request(app: &mut App, method: Method) -> Value {
    serde_json::from_str(
        &app.handle_api_request_after_internal_events_drained(Request {
            id: "retained-fixture".into(),
            method,
        }),
    )
    .unwrap()
}
#[tokio::test]
async fn retained_api_requires_operator_even_for_list_and_refuses_all_without_yes() {
    let mut app = operator();
    app.state.structured_agents =
        serde_json::from_value(json!([true, {"runtime":"future-runtime"}])).unwrap();
    for uid in [Some(42), Some(99), None] {
        app.current_api_peer_uid = Some(uid);
        for method in [
            list(),
            purge(Selection::Dead, false),
            purge(Selection::All, true),
        ] {
            assert_eq!(request(&mut app, method)["error"]["code"], "operator_only");
        }
    }
    app.current_api_peer_uid = Some(Some(7));
    app.current_api_peer_pid = None;
    assert_eq!(
        request(&mut app, purge(Selection::All, true))["error"]["code"],
        "operator_only"
    );
    app.current_api_peer_pid = Some(std::process::id());
    assert_eq!(
        request(&mut app, purge(Selection::All, false))["error"]["code"],
        "invalid_params"
    );
    assert_eq!(app.state.structured_agents.retained_count(), 2);
    assert_eq!(
        request(&mut app, purge(Selection::Dead, false))["result"]["removed"],
        1
    );
    assert_eq!(request(&mut app, list())["result"]["forward"], 1);
    assert_eq!(
        request(&mut app, purge(Selection::All, true))["result"]["removed"],
        1
    );
    assert_eq!(app.state.structured_agents.retained_count(), 0);
}
#[tokio::test]
async fn retained_doctor_hints_at_forward_or_more_than_half_dead_cap_only() {
    let mut app = operator();
    for (dead, forward, hint) in [(0, 0, false), (128, 0, false), (129, 0, true), (0, 1, true)] {
        let mut records: Vec<_> = (0..dead).map(|i| json!(i)).collect();
        records.extend((0..forward).map(|_| json!({"harness":"future-harness"})));
        app.state.structured_agents = serde_json::from_value(json!(records)).unwrap();
        let response = request(&mut app, list());
        assert_eq!(response["result"]["dead"], dead);
        assert_eq!(response["result"]["forward"], forward);
        assert_eq!(response["result"]["purge_hint"].is_string(), hint);
        if hint {
            assert!(response["result"]["purge_hint"]
                .as_str()
                .unwrap()
                .contains("flk session retained purge"));
        }
    }
}
#[tokio::test]
async fn retained_non_array_api_error_is_typed_and_names_offline_repair() {
    let mut app = operator();
    let raw = json!({"future_collection":[1,2]});
    app.state.structured_agents = serde_json::from_value(raw.clone()).unwrap();
    for method in [list(), purge(Selection::All, true)] {
        let response = request(&mut app, method);
        assert_eq!(response["error"]["code"], "retained_collection_shape");
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("session-state repair guide"));
    }
    assert_eq!(
        serde_json::to_value(&app.state.structured_agents).unwrap(),
        raw
    );
}
struct SavedEnv(Vec<(&'static str, Option<std::ffi::OsString>)>);
impl Drop for SavedEnv {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}
#[tokio::test]
async fn retained_purge_persists_before_mutation_and_refuses_failed_commit() {
    let _guard = crate::config::test_config_env_guard();
    let root = crate::test_support::TempDir::socket();
    let keys = [
        crate::config::CONFIG_PATH_ENV_VAR,
        "XDG_STATE_HOME",
        "XDG_CONFIG_HOME",
    ];
    let _env = SavedEnv(
        keys.iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect(),
    );
    std::env::set_var(keys[0], root.path().join("config.toml"));
    std::env::set_var(keys[1], root.path().join("state"));
    std::env::set_var(keys[2], root.path().join("config"));
    let mut app = operator();
    app.no_session = false;
    let raw = json!([true,{"runtime":"future-runtime"}]);
    app.state.structured_agents = serde_json::from_value(raw.clone()).unwrap();
    let dir = crate::session::data_dir();
    assert!(dir.starts_with(root.path()));
    std::fs::create_dir_all(&dir).unwrap();
    let history = dir.join("session-history.json");
    std::fs::write(&history, "history fixture bytes").unwrap();
    let response = request(&mut app, purge(Selection::Dead, false));
    assert_eq!(response["result"]["removed"], 1, "{response}");
    let saved = crate::persist::load().unwrap();
    assert_eq!(
        serde_json::to_value(saved.structured_agents).unwrap(),
        json!([raw[1]])
    );
    assert_eq!(
        std::fs::read_to_string(&history).unwrap(),
        "history fixture bytes"
    );
    let path = dir.join("session.json");
    std::fs::rename(&path, dir.join("saved.json")).unwrap();
    std::fs::create_dir(&path).unwrap();
    app.state.structured_agents = serde_json::from_value(raw.clone()).unwrap();
    let response = request(&mut app, purge(Selection::All, true));
    assert_eq!(response["error"]["code"], "internal_error", "{response}");
    assert_eq!(
        serde_json::to_value(&app.state.structured_agents).unwrap(),
        raw
    );
}

#[tokio::test]
async fn retained_agent_token_without_operator_evidence_is_refused() {
    let mut app = operator();
    app.current_api_peer_uid = None;
    let raw = json!([true, {"runtime":"future-runtime"}]);
    app.state.structured_agents = serde_json::from_value(raw.clone()).unwrap();
    for method in [list(), purge(Selection::All, true)] {
        let mut raw_request = serde_json::to_value(Request {
            id: "agent-token".into(),
            method,
        })
        .unwrap();
        raw_request["token"] = json!("fixture-agent-token");
        let request = serde_json::from_value(raw_request).unwrap();
        let response: Value =
            serde_json::from_str(&app.handle_api_request_after_internal_events_drained(request))
                .unwrap();
        assert_eq!(response["error"]["code"], "operator_only");
    }
    assert_eq!(
        serde_json::to_value(&app.state.structured_agents).unwrap(),
        raw
    );
}
