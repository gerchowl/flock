//! Upgrade old delegate records through the CLI, preserving unknown data.
mod support;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

struct Fixture {
    dir: support::socket_dir::SocketDir,
    socket: std::path::PathBuf,
    entry: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<Value>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
fn session(value: &str) -> Value {
    json!({"source":"flock:codex","agent":"codex","kind":"id","value":value})
}
fn agent() -> Value {
    json!({"name":"fixture","agent":"codex","agent_id":"agent_fixture.example_backfill",
        "terminal_id":"term_new","pane_id":"w1:p1","agent_status":"working",
        "turn_cursor":"term_new:0:1:2:w","agent_session":session("saved")})
}
impl Fixture {
    fn new(saved: Value, agents: Vec<Value>) -> Self {
        let dir = support::socket_dir::SocketDir::reserve("d3088");
        let socket = dir.path().join("s");
        let registry = dir
            .path()
            .join("state")
            .join(support::paths::app_dir_name())
            .join("delegates")
            .join(support::paths::server_key(&socket));
        std::fs::create_dir_all(&registry).unwrap();
        let entry = registry.join("fixture.json");
        std::fs::write(
            &entry,
            json!({"name":"fixture","terminal_id":"term_old","pane_id":"w1:p1",
            "root_pane":"w1:p0","workspace_id":"w1","mode":"cwd","worktree":null,
            "branch":null,"harness":"codex","model":null,"round":2,"brief":"brief.md",
            "submitted_at_ms":1,"created_at_ms":1,"cursor":"term_old:0:0:0:i",
            "session":saved,"future_field":{"items":[1,"unknown"]}})
            .to_string(),
        )
        .unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut line = String::new();
                BufReader::new(&mut stream).read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                seen.lock().unwrap().push(request.clone());
                let response = match request["method"].as_str().unwrap() {
                    "ping" => json!({"result":{"protocol_version":34}}),
                    "agent.list" => json!({"result":{"agents":agents,"structured_agents":[]}}),
                    "agent.get" => json!({"result":{"agent":agent()}}),
                    "delegate.rebind" => json!({"result":{"agent":agent()}}),
                    other => panic!("unexpected request: {other}"),
                };
                writeln!(stream, "{response}").unwrap();
            }
        });
        Self {
            dir,
            socket,
            entry,
            stop,
            requests,
            thread: Some(thread),
        }
    }
    fn run(&self, extra: &[&str]) -> std::process::Output {
        support::environment::Command::new(env!("CARGO_BIN_EXE_flk"))
            .env_remove("FLOCK_SOCKET_PATH")
            .env_remove("FLOCK_PANE_ID")
            .env_remove("FLOCK_AGENT_ID")
            .env("HOME", self.dir.path())
            .env("XDG_STATE_HOME", self.dir.path().join("state"))
            .env("XDG_CONFIG_HOME", self.dir.path().join("cfg"))
            .env("FLOCK_SOCKET_PATH", &self.socket)
            .arg("delegate")
            .args(extra)
            .arg("--json")
            .output()
            .unwrap()
    }
    fn read(&self) -> Value {
        serde_json::from_slice(&std::fs::read(&self.entry).unwrap()).unwrap()
    }
    fn unbound(&self, reason: &str) {
        let before = std::fs::read(&self.entry).unwrap();
        let output = self.run(&["status", "fixture"]);
        assert_eq!(output.status.code(), Some(12), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("session_unbound"),
            "{output:?}"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("delegate rebind"),
            "{output:?}"
        );
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["reason"], reason);
        assert_eq!(std::fs::read(&self.entry).unwrap(), before);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.socket);
        self.thread.take().unwrap().join().unwrap();
    }
}
#[test]
fn upgrade_exact_session_is_durable_and_idempotent() {
    let fixture = Fixture::new(session("saved"), vec![agent()]);
    let output = fixture.run(&["status", "fixture"]);
    assert!(output.status.success(), "{output:?}");
    let saved = fixture.read();
    assert_eq!(saved["agent_id"], agent()["agent_id"]);
    assert_eq!(saved["terminal_id"], "term_new");
    assert_eq!(saved["round"], 2);
    assert_eq!(saved["future_field"], json!({"items":[1,"unknown"]}));
    assert!(fixture.run(&["status", "fixture"]).status.success());
    assert_eq!(fixture.read(), saved);
    assert_eq!(
        fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["method"] == "agent.list")
            .count(),
        1
    );
}
#[test]
fn reused_terminal_different_session_stays_unbound() {
    let mut replacement = agent();
    replacement["terminal_id"] = json!("term_old");
    replacement["agent_session"] = session("replacement");
    Fixture::new(session("saved"), vec![replacement]).unbound("saved_session_not_found");
}
#[test]
fn no_session_stays_unbound() {
    Fixture::new(Value::Null, vec![agent()]).unbound("saved_session_missing");
}
#[test]
fn duplicate_session_stays_unbound() {
    let mut duplicate = agent();
    duplicate["agent_id"] = json!("agent_fixture.example_duplicate");
    Fixture::new(session("saved"), vec![agent(), duplicate]).unbound("saved_session_ambiguous");
}
#[test]
fn operator_rebind_updates_only_binding_and_preserves_unknown_fields() {
    let fixture = Fixture::new(Value::Null, vec![agent()]);
    let output = fixture.run(&[
        "rebind",
        "fixture",
        "--agent",
        "agent_fixture.example_backfill",
    ]);
    assert!(output.status.success(), "{output:?}");
    let saved = fixture.read();
    assert_eq!(saved["agent_id"], agent()["agent_id"]);
    assert_eq!(saved["session"], session("saved"));
    assert_eq!(saved["round"], 2);
    assert_eq!(saved["future_field"], json!({"items":[1,"unknown"]}));
    assert!(fixture
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|r| r["method"] == "delegate.rebind"));
}

#[test]
fn same_id_from_another_harness_is_not_a_match() {
    let mut other = agent();
    other["agent_session"]["agent"] = json!("claude");
    Fixture::new(session("saved"), vec![other]).unbound("saved_session_not_found");
}

#[test]
fn incomplete_live_binding_stays_unbound() {
    let mut incomplete = agent();
    incomplete.as_object_mut().unwrap().remove("agent_id");
    Fixture::new(session("saved"), vec![incomplete]).unbound("live_binding_incomplete");
}

#[test]
fn failed_atomic_upgrade_leaves_old_record_intact() {
    let fixture = Fixture::new(session("saved"), vec![agent()]);
    let before = std::fs::read(&fixture.entry).unwrap();
    std::fs::create_dir(fixture.entry.parent().unwrap().join(".fixture.json.tmp")).unwrap();
    let output = fixture.run(&["status", "fixture"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(std::fs::read(&fixture.entry).unwrap(), before);
}

#[test]
fn unknown_neighbor_record_is_preserved_raw_during_upgrade() {
    let fixture = Fixture::new(session("saved"), vec![agent()]);
    let neighbor = fixture.entry.with_file_name("future.json");
    let raw = b"{\n  \"schema\":999, \"newer_entry\":[true]\n}\n";
    std::fs::write(&neighbor, raw).unwrap();
    assert!(fixture.run(&["status", "fixture"]).status.success());
    assert_eq!(std::fs::read(neighbor).unwrap(), raw);
}
