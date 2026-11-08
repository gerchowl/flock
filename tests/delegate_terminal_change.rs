//! Delegate identity survives terminal replacement, with authoritative sessions.
#![allow(clippy::disallowed_methods)]
mod support;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::process::Output;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use support::environment::Command;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionCase {
    Normal,
    ConflictingRecord,
    ConflictingResult,
    AmbiguousClaude,
    UnreadableClaude,
    Protocol32,
    CachedGrace,
    EmptyGrace,
    TurnCheckGrace,
    ResultReadRace,
    QuietGrace,
}

struct Fixture {
    dir: support::socket_dir::SocketDir,
    socket: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<Value>>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Fixture {
    fn new(evidence: bool, during_wait: bool) -> Self {
        Self::serve(evidence, during_wait, false)
    }
    fn serve(evidence: bool, during_wait: bool, empty_result: bool) -> Self {
        Self::scenario(evidence, during_wait, empty_result, false)
    }
    fn scenario(evidence: bool, during_wait: bool, empty_result: bool, legacy: bool) -> Self {
        Self::case(
            evidence,
            during_wait,
            empty_result,
            legacy,
            SessionCase::Normal,
        )
    }
    fn case(
        evidence: bool,
        during_wait: bool,
        empty_result: bool,
        legacy: bool,
        case: SessionCase,
    ) -> Self {
        let conflict = case == SessionCase::ConflictingRecord;
        let grace = matches!(
            case,
            SessionCase::CachedGrace
                | SessionCase::EmptyGrace
                | SessionCase::TurnCheckGrace
                | SessionCase::ResultReadRace
                | SessionCase::QuietGrace
        );
        let dir = support::socket_dir::SocketDir::reserve("d2993");
        let socket = dir.path().join("s");
        let home = dir.path().join("home");
        let registry = home
            .join("state")
            .join(support::paths::app_dir_name())
            .join("delegates")
            .join(support::paths::server_key(&socket));
        std::fs::create_dir_all(&registry).unwrap();
        let harness = if matches!(
            case,
            SessionCase::AmbiguousClaude | SessionCase::UnreadableClaude
        ) {
            "claude"
        } else {
            "codex"
        };
        let session = json!({"source":format!("flock:{harness}"),"agent":harness,"kind":"id","value":"session-2993"});
        let mut entry = json!({"name":"fixture", "terminal_id":"term_old", "pane_id":"w1:p1",
            "root_pane":"w1:p0", "workspace_id":"w1", "mode":"cwd", "worktree":null,
            "branch":null,"harness":harness,"model":null,"round":1,"brief":"brief.md",
            "submitted_at_ms":1,"created_at_ms":1,"cursor":"term_old:0:0:0:i",
            "agent_id":"agent_fixture.example_2993","session":if evidence {session.clone()} else {Value::Null}});
        if legacy {
            entry["agent_id"] = Value::Null;
            entry["session"] = Value::Null;
            entry["worktree"] = json!(home);
        }
        std::fs::write(registry.join("fixture.json"), entry.to_string()).unwrap();
        let sessions = home.join(".codex/sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("rollout-test-session-2993.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session-2993\"}}\n",
        )
        .unwrap();
        if conflict {
            std::fs::write(
                sessions.join("rollout-test-session-replacement.jsonl"),
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session-replacement\"}}\n",
            )
            .unwrap();
        }
        if matches!(
            case,
            SessionCase::AmbiguousClaude | SessionCase::UnreadableClaude
        ) {
            let profile = home.join("profile");
            let records = home
                .join("state")
                .join(support::paths::app_dir_name())
                .join("claude-config-dirs");
            std::fs::create_dir_all(&records).unwrap();
            std::fs::write(records.join("session-2993"), profile.to_str().unwrap()).unwrap();
            for store in [profile, home.join(".claude")] {
                let project = store.join("projects/fixture");
                std::fs::create_dir_all(&project).unwrap();
                std::fs::write(
                    project.join("session-2993.jsonl"),
                    "{\"sessionId\":\"session-2993\"}\n",
                )
                .unwrap();
            }
        }
        if case == SessionCase::UnreadableClaude {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                home.join("profile/projects/fixture"),
                std::fs::Permissions::from_mode(0o000),
            )
            .unwrap();
        }
        std::fs::write(home.join("brief.md"), "Finish this round.").unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let worker = std::thread::spawn(move || {
            let mut first = true;
            let mut bound = empty_result || legacy || conflict || grace;
            let mut old_sampled = false;
            let mut result_polled = false;
            let mut replaced = false;
            while !stopped.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut line = String::new();
                if BufReader::new(&mut stream).read_line(&mut line).is_err() {
                    continue;
                }
                let Ok(request) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                seen.lock().unwrap().push(request.clone());
                let target = request["params"]["target"].as_str().unwrap_or_default();
                let result = match request["method"].as_str().unwrap() {
                    "agent.get" if grace => {
                        if target == "term_old" {
                            old_sampled = true;
                        } else if !matches!(
                            case,
                            SessionCase::ResultReadRace | SessionCase::QuietGrace
                        ) && old_sampled
                            && (case != SessionCase::TurnCheckGrace || result_polled)
                        {
                            replaced = true;
                        }
                        json!({"result":{"agent":{"name":"fixture","agent_id":"agent_fixture.example_2993",
                            "terminal_id":if replaced {"term_new"} else {"term_old"},"pane_id":"w1:p1",
                            "agent_status":if replaced {if case == SessionCase::ResultReadRace {"unknown"} else {"working"}} else {"done"},
                            "turn_cursor":if replaced {"term_new:0:10:20:w"} else {"term_old:0:1:2:i"},
                            "agent_session":session}}})
                    }
                    "agent.get" if target == "other" => json!({"result":{"agent":{
                        "name":"other","agent_id":"agent_fixture.example_other", "cwd":home,
                        "terminal_id":"term_other","pane_id":"w1:p2","agent_session":session}}}),
                    "agent.get" if target == "term_old" && !first => {
                        json!({"error":{"code":"agent_not_found","message":"old terminal replaced"}})
                    }
                    "agent.get" => {
                        let old = first && during_wait;
                        first = false;
                        let mut observed = session.clone();
                        if conflict && !old {
                            observed["value"] = json!("session-replacement");
                        }
                        json!({"result":{"agent":{"name":"fixture","agent_id":"agent_fixture.example_2993",
                            "terminal_id":if old {"term_old"} else {"term_new"},"pane_id":"w1:p1",
                            "agent_status":if empty_result {"working"} else {"done"},"turn_cursor":if old {"term_old:0:0:0:i"} else {"term_new:0:1:2:i"},
                            "cwd":home,"agent_session":if bound || evidence {observed} else {Value::Null}}}})
                    }
                    "pane.get" => {
                        json!({"result":{"pane":{"terminal_id":"term_new","pane_id":"w1:p1"}}})
                    }
                    "pane.report_agent_session" => {
                        assert_eq!(
                            request["params"]["agent_session_id"],
                            if conflict {
                                "session-replacement"
                            } else {
                                "session-2993"
                            }
                        );
                        bound = true;
                        json!({"result":{}})
                    }
                    "agent.result"
                        if case == SessionCase::EmptyGrace
                            || case == SessionCase::TurnCheckGrace
                            || case == SessionCase::QuietGrace =>
                    {
                        result_polled = true;
                        json!({"error":{"code":"no_result","message":"turn still running"}})
                    }
                    "agent.result" if empty_result => {
                        json!({"error":{"code":"no_result","message":"turn still running"}})
                    }
                    "agent.result" if case == SessionCase::ResultReadRace => {
                        replaced = true;
                        json!({"result":{"result":{"terminal_id":"term_new", "finished":true,
                            "at_ms":2,"status":"done","text":"DONE: cached replacement", "session_id":"session-2993"}}})
                    }
                    "agent.result" if bound => json!({"result":{"result":{"finished":true,
                        "at_ms":2,"status":"done","text":if conflict || case == SessionCase::ConflictingResult {"DONE: replacement"} else {"DONE: rebound"},"session_id":if conflict || case == SessionCase::ConflictingResult {"session-replacement"} else {"session-2993"}}}}),
                    "ping" => {
                        json!({"result":{"type":"pong","version":"3.3.0","protocol":if case == SessionCase::Protocol32 {32} else {support::PROTOCOL_VERSION},"capabilities":{"pane_input_write_ack":true}}})
                    }
                    "events.read" => json!({"result":{"events":[],"next_after":0}}),
                    "pane.submit" if !bound => {
                        json!({"result":{"type":"guarded_submit","retried":false,"pane_id":"w1:p1","outcome":"unconfirmed","reason":"owned_composer_not_visible"}})
                    }
                    "pane.submit" if bound => {
                        json!({"result":{"type":"guarded_submit","retried":false,"pane_id":"w1:p1","outcome":"accepted"}})
                    }
                    "pane.read" => {
                        json!({"result":{"read":{"text":"› \n gpt-6.1-sol · 100% left"}}})
                    }
                    _ => {
                        json!({"error":{"code":"no_agent_session","message":"no session binding"}})
                    }
                };
                let mut response = result;
                response["id"] = request["id"].clone();
                let _ = writeln!(stream, "{response}");
                let _ = stream.read(&mut [0]);
            }
        });
        Self {
            dir,
            socket,
            stop,
            requests,
            worker: Some(worker),
        }
    }
    fn cli(&self, verb: &str) -> Output {
        let home = self.dir.path().join("home");
        let mut command = Command::new(env!("CARGO_BIN_EXE_flk"));
        command
            .env_remove("FLOCK_SOCKET_PATH")
            .env_remove("FLOCK_PANE_ID")
            .env_remove("FLOCK_AGENT_ID")
            .env("SYSTEMONE_URL", "")
            .env_remove("CODEX_HOME")
            .env("HOME", &home)
            .env("XDG_STATE_HOME", home.join("state"))
            .env("XDG_CONFIG_HOME", self.dir.path().join("cfg"))
            .env("FLOCK_SOCKET_PATH", &self.socket)
            .args(["delegate", verb, "fixture", "--json"]);
        if verb == "wait" {
            command.args(["--settle", "0", "--timeout", "2000"]);
        }
        if verb == "send" {
            command.arg("--brief").arg(home.join("brief.md"));
        }
        command.output().unwrap()
    }
    fn assert(&self, verb: &str, code: i32, outcome: &str) {
        let output = self.cli(verb);
        assert_eq!(
            output.status.code(),
            Some(code),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(outcome),
            "{output:?}"
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let blocked = self.dir.path().join("home/profile/projects/fixture");
        if blocked.exists() {
            std::fs::set_permissions(blocked, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}
#[test]
fn wait_follows_terminal_change_and_rerun_uses_same_round() {
    let fixture = Fixture::new(true, true);
    fixture.assert("wait", 0, "DONE: rebound");
    fixture.assert("wait", 0, "DONE: rebound");
}
#[test]
fn unbound_live_delegate_has_distinct_state() {
    Fixture::new(false, false).assert("wait", 12, "session_unbound");
}
#[test]
fn result_rebinds_exact_codex_session() {
    Fixture::new(true, false).assert("result", 0, "DONE: rebound");
}
#[test]
fn result_liveness_follows_identity_when_terminal_changes_after_lookup() {
    Fixture::serve(true, true, true).assert("result", 0, "running");
}

#[test]
fn legacy_same_cwd_cannot_return_another_agents_reply() {
    let fixture = Fixture::scenario(false, false, false, true);
    for verb in ["result", "wait"] {
        fixture.assert(verb, 12, "session_unbound");
        let home = fixture.dir.path().join("home");
        let registry = home
            .join("state")
            .join(support::paths::app_dir_name())
            .join("delegates")
            .join(support::paths::server_key(&fixture.socket));
        let entry: Value =
            serde_json::from_slice(&std::fs::read(registry.join("fixture.json")).unwrap()).unwrap();
        assert!(entry["agent_id"].is_null());
        assert!(entry["session"].is_null());
    }
}

#[test]
fn conflicting_observed_session_preserves_launch_record() {
    for verb in ["result", "wait"] {
        let fixture = Fixture::case(true, false, false, false, SessionCase::ConflictingRecord);
        let registry = fixture
            .dir
            .path()
            .join("home/state")
            .join(support::paths::app_dir_name())
            .join("delegates")
            .join(support::paths::server_key(&fixture.socket))
            .join("fixture.json");
        let before = std::fs::read(&registry).unwrap();
        fixture.assert(verb, 12, "session_unbound");
        assert_eq!(std::fs::read(&registry).unwrap(), before);
    }
}

#[test]
fn wait_conflict_during_terminal_change_preserves_launch_record() {
    let fixture = Fixture::case(true, true, false, false, SessionCase::ConflictingRecord);
    let registry = fixture
        .dir
        .path()
        .join("home/state")
        .join(support::paths::app_dir_name())
        .join("delegates")
        .join(support::paths::server_key(&fixture.socket))
        .join("fixture.json");
    let before = std::fs::read(&registry).unwrap();
    fixture.assert("wait", 12, "session_unbound");
    assert_eq!(std::fs::read(&registry).unwrap(), before);
}

#[test]
fn result_session_change_after_lookup_is_unbound() {
    for verb in ["result", "wait"] {
        Fixture::case(true, false, false, false, SessionCase::ConflictingResult).assert(
            verb,
            12,
            "session_unbound",
        );
    }
}

#[test]
fn recovery_guard_requires_protocol_33_server() {
    Fixture::case(true, false, false, false, SessionCase::Protocol32).assert(
        "result",
        78,
        "server_version_gap",
    );
}

#[test]
fn ambiguous_claude_profile_and_default_session_is_unbound() {
    for verb in ["wait", "result"] {
        Fixture::case(true, false, false, false, SessionCase::AmbiguousClaude).assert(
            verb,
            12,
            "session_unbound",
        );
    }
}

fn assert_mid_grace_recovery(case: SessionCase) {
    let fixture = Fixture::case(true, false, false, false, case);
    let registry = fixture
        .dir
        .path()
        .join("home/state")
        .join(support::paths::app_dir_name())
        .join("delegates")
        .join(support::paths::server_key(&fixture.socket))
        .join("fixture.json");
    let before: Value = serde_json::from_slice(&std::fs::read(&registry).unwrap()).unwrap();
    fixture.assert("wait", 124, "timeout");
    let after: Value = serde_json::from_slice(&std::fs::read(&registry).unwrap()).unwrap();
    for key in ["round", "submitted_at_ms", "cursor", "session"] {
        assert_eq!(after[key], before[key], "recovery changed {key}");
    }
    assert_eq!(after["terminal_id"], "term_new");
    let requests = fixture.requests.lock().unwrap();
    assert!(requests
        .iter()
        .any(|r| r["method"] == "pane.report_agent_session"));
    assert!(
        requests
            .iter()
            .any(|r| r["method"] == "agent.get" && r["params"]["target"] == "term_new"),
        "the replacement must be sampled by settling"
    );
}

#[test]
fn mid_grace_cached_reply_waits_for_working_replacement_to_settle() {
    assert_mid_grace_recovery(SessionCase::CachedGrace);
}

#[test]
fn mid_grace_no_result_does_not_compare_terminal_counters() {
    assert_mid_grace_recovery(SessionCase::EmptyGrace);
}

#[test]
fn mid_grace_turn_check_restarts_on_a_different_terminal() {
    assert_mid_grace_recovery(SessionCase::TurnCheckGrace);
}

#[test]
fn unreadable_claude_store_is_unbound_and_names_the_store() {
    for verb in ["wait", "result"] {
        let fixture = Fixture::case(true, false, false, false, SessionCase::UnreadableClaude);
        let output = fixture.cli(verb);
        assert_eq!(output.status.code(), Some(12), "{output:?}");
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["outcome"], "session_unbound");
        let blocked = fixture.dir.path().join("home/profile/projects/fixture");
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains(blocked.to_str().unwrap()),
            "{response}"
        );
        assert!(!fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r["method"] == "pane.report_agent_session"));
    }
}

#[test]
fn replacement_between_precheck_and_result_must_settle() {
    let fixture = Fixture::case(true, false, false, false, SessionCase::ResultReadRace);
    fixture.assert("wait", 124, "timeout");
}

#[test]
fn reply_grace_checks_identity_once_per_result_read() {
    let fixture = Fixture::case(true, false, false, false, SessionCase::QuietGrace);
    fixture.assert("wait", 124, "timeout");
    let requests = fixture.requests.lock().unwrap();
    let reads: Vec<_> = requests
        .iter()
        .enumerate()
        .filter_map(|(index, request)| (request["method"] == "agent.result").then_some(index))
        .collect();
    assert!(reads.len() >= 2, "the fixture must exercise reply grace");
    for pair in reads.windows(2) {
        let between = &requests[pair[0] + 1..pair[1]];
        assert_eq!(
            between.len(),
            1,
            "redundant requests during grace: {between:?}"
        );
        assert_eq!(between[0]["method"], "agent.get");
        assert_eq!(between[0]["params"]["target"], "agent_fixture.example_2993");
    }
}
