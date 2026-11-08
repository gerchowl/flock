//! Delegate identity survives terminal replacement, with authoritative sessions.
#![allow(clippy::disallowed_methods)]
mod support;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::process::Output;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use support::environment::Command;

struct Fixture {
    dir: support::socket_dir::SocketDir,
    socket: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Fixture {
    fn new(evidence: bool, during_wait: bool) -> Self {
        Self::serve(evidence, during_wait, false)
    }
    fn serve(evidence: bool, during_wait: bool, empty_result: bool) -> Self {
        let dir = support::socket_dir::SocketDir::reserve("d2993");
        let socket = dir.path().join("s");
        let home = dir.path().join("home");
        let registry = home
            .join("state")
            .join(support::paths::app_dir_name())
            .join("delegates")
            .join(support::paths::server_key(&socket));
        std::fs::create_dir_all(&registry).unwrap();
        let session =
            json!({"source":"flock:codex","agent":"codex","kind":"id","value":"session-2993"});
        let entry = json!({"name":"fixture", "terminal_id":"term_old", "pane_id":"w1:p1",
            "root_pane":"w1:p0", "workspace_id":"w1", "mode":"cwd", "worktree":null,
            "branch":null,"harness":"codex","model":null,"round":1,"brief":"brief.md",
            "submitted_at_ms":1,"created_at_ms":1,"cursor":"term_old:0:0:0:i",
            "agent_id":"agent_fixture.example_2993","session":if evidence {session.clone()} else {Value::Null}});
        std::fs::write(registry.join("fixture.json"), entry.to_string()).unwrap();
        let sessions = home.join(".codex/sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("rollout-test-session-2993.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session-2993\"}}\n",
        )
        .unwrap();
        std::fs::write(home.join("brief.md"), "Finish this round.").unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = std::thread::spawn(move || {
            let mut first = true;
            let mut bound = empty_result;
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
                let target = request["params"]["target"].as_str().unwrap_or_default();
                let result = match request["method"].as_str().unwrap() {
                    "agent.get" if target == "term_old" && !first => {
                        json!({"error":{"code":"agent_not_found","message":"old terminal replaced"}})
                    }
                    "agent.get" => {
                        let old = first && during_wait;
                        first = false;
                        json!({"result":{"agent":{"name":"fixture","agent_id":"agent_fixture.example_2993",
                            "terminal_id":if old {"term_old"} else {"term_new"},"pane_id":"w1:p1",
                            "agent_status":if empty_result {"working"} else {"done"},"turn_cursor":if old {"term_old:0:0:0:i"} else {"term_new:0:1:2:i"},
                            "agent_session":if bound {session.clone()} else {Value::Null}}}})
                    }
                    "pane.get" => {
                        json!({"result":{"pane":{"terminal_id":"term_new","pane_id":"w1:p1"}}})
                    }
                    "pane.report_agent_session" => {
                        assert_eq!(request["params"]["agent_session_id"], "session-2993");
                        bound = true;
                        json!({"result":{}})
                    }
                    "agent.result" if empty_result => {
                        json!({"error":{"code":"no_result","message":"turn still running"}})
                    }
                    "agent.result" if bound => json!({"result":{"result":{"finished":true,
                        "at_ms":2,"status":"done","text":"DONE: rebound","session_id":"session-2993"}}}),
                    "ping" => {
                        json!({"result":{"type":"pong","version":"3.3.0","protocol":support::PROTOCOL_VERSION,"capabilities":{"pane_input_write_ack":true}}})
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
fn send_rebinds_before_submission() {
    Fixture::new(true, false).assert("send", 0, "\"round\":2");
}

#[test]
fn result_liveness_follows_identity_when_terminal_changes_after_lookup() {
    Fixture::serve(true, true, true).assert("result", 0, "running");
}
