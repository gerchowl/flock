//! Lifecycle removals exercised through real isolated servers and custody stores.
mod support;
use serde_json::{json, Value};
use std::{fs, os::unix::fs::PermissionsExt, path::Path, time::Duration};
use support::fleet::{self, Fleet, Node, NodeSpec};
const DEADLINE: Duration = Duration::from_secs(45);
const PAIR: &[NodeSpec] = &[
    NodeSpec::new("nodea", "tombstone-origin", &["nodeb"]),
    NodeSpec::new("nodeb", "tombstone-owner", &[]),
];
const SINGLE: &[NodeSpec] = &[NodeSpec::new("nodea", "tombstone-local", &[]).with_config(
    "\n[session.restart]\nrestart_grace_secs = 0\nkill_grace_secs = 0\noperator_quiet_ms = 0\nflush_wait_ms = 0\nsettle_ms = 0\nverify_timeout_secs = 20\n",
)];
fn raw(node: &Node, method: &str, params: Value) -> Value {
    serde_json::from_str(
        &node.api(&json!({"id":"tombstone","method":method,"params":params}).to_string()),
    )
    .unwrap()
}
fn api(node: &Node, method: &str, params: Value) -> Value {
    let response = raw(node, method, params);
    assert!(response.get("error").is_none(), "{method}: {response}");
    response["result"].clone()
}
fn start_at(node: &Node, cwd: &Path, executable: &Path) -> Value {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let name = format!(
        "fixture-{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    api(
        node,
        "agent.start",
        json!({"name":name, "cwd":cwd, "argv":[executable]}),
    )["agent"]
        .clone()
}
fn start(node: &Node) -> Value {
    start_at(node, &node.repo, Path::new("/bin/sh"))
}
fn db(node: &Node) -> rusqlite::Connection {
    let connection =
        rusqlite::Connection::open(node.home.join("state/flock-dev/mesh-mail.sqlite")).unwrap();
    connection.busy_timeout(Duration::from_secs(5)).unwrap();
    connection
}
fn removals(node: &Node) -> i64 {
    db(node)
        .query_row("SELECT count(*) FROM agent_tombstones", [], |r| r.get(0))
        .unwrap()
}
fn send(node: &Node, sender: &Value, target: &Value, correlation: &str) -> Value {
    api(
        node,
        "msg.send",
        json!({"to":{"type":"agent", "agent":target["agent_id"]},
        "from_agent":sender["agent_id"], "body":"fixture question", "intent":"needs_reply", "correlation_id":correlation}),
    )
}
fn state(node: &Node, correlation: &str, expected: &str) -> Value {
    fleet::wait_until(expected, DEADLINE, || {
        let result = api(node, "msg.status", json!({"correlation_id":correlation}));
        (result["state"] == expected).then_some(result)
    })
}
fn close(node: &Node, agent: &Value) {
    api(node, "pane.close", json!({"pane_id":agent["pane_id"]}));
}
fn discover(fleet: &Fleet, target: &Value) {
    fleet.wait_route("nodea", "nodeb", true);
    fleet::wait_until("remote recipient", DEADLINE, || {
        api(fleet.node("nodea"), "agent.list", json!({}))["fleet"]
            .as_array()?
            .iter()
            .any(|a| a["agent_id"] == target["agent_id"])
            .then_some(())
    });
}
fn session(node: &Node, agent: &Value) {
    api(
        node,
        "pane.report_agent_session",
        json!({"pane_id":agent["pane_id"],
        "source":"flock:pi", "agent":"pi", "agent_session_id":"retained-session"}),
    );
}
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
fn native_executable(node: &Node) -> std::path::PathBuf {
    let executable = node.home.join("claude");
    let report = format!("printf '%s\\n' '{{\"session_id\":\"retained-session\"}}' | {0} hook claude session\n{0} pane report-agent --source flock:claude --agent claude --state idle --agent-session-id retained-session", quote(env!("CARGO_BIN_EXE_flk")));
    let stop = format!("if [ \"$line\" = fixture-stop ]; then printf '%s\\n' '{{\"hook_event_name\":\"Stop\",\"last_assistant_message\":\"※ recap: Done. Next: wait.\"}}' | {} hook claude stop > {}; touch {}; fi", quote(env!("CARGO_BIN_EXE_flk")), quote(node.home.join("stop-output").to_str().unwrap()), quote(node.home.join("stop-ready").to_str().unwrap()));
    fs::write(&executable, format!("#!/bin/sh\n{report}\nprintf 'Claude Code\\nTask complete.\\n─────────────\\n❯ \\n─────────────\\n'\nwhile IFS= read -r line\ndo\n{report}\n{stop}\ndone\n")).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    executable
}
fn native_agent(node: &Node) -> Value {
    let executable = native_executable(node);
    let agent = start_at(node, &node.repo, &executable);
    fleet::wait_until("native session hook", DEADLINE, || {
        let current = api(node, "agent.get", json!({"target":agent["pane_id"]}))["agent"].clone();
        (current["agent_session"]["value"] == "retained-session").then_some(())
    });
    agent
}

fn native_harness(node: &Node, harness: &str, screen: &str) -> Value {
    let executable = node.home.join(harness);
    let report = format!(
        "{} pane report-agent --source flock:{harness} --agent {harness} --state idle --agent-session-id retained-session",
        quote(env!("CARGO_BIN_EXE_flk"))
    );
    fs::write(
        &executable,
        format!("#!/bin/sh\n{report}\nprintf '\\033[2J\\033[H%s\\n' {}\nwhile IFS= read -r line; do :; done\n", quote(screen)),
    ).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let agent = start_at(node, &node.repo, &executable);
    fleet::wait_until("native harness session", DEADLINE, || {
        let current = api(node, "agent.get", json!({"target":agent["pane_id"]}))["agent"].clone();
        (current["agent_session"]["value"] == "retained-session").then_some(())
    });
    agent
}

fn assert_harness_restart(harness: &str, screen: &str) {
    let fleet = fleet::spawn("ts-harness", SINGLE);
    let node = fleet.node("nodea");
    let target = native_harness(node, harness, screen);
    let sender = start(node);
    send(node, &sender, &target, "harness-restart-mail");
    api(
        node,
        "agent.restart",
        json!({"target":target["pane_id"], "reason":"fixture harness restart"}),
    );
    fleet::wait_until("harness restart verified", DEADLINE, || {
        let log = fs::read_to_string(node.config_home.join("flock-dev/event-log.jsonl")).ok()?;
        log.lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|event| {
                event["envelope"]["event"] == "agent_restart"
                    && event["envelope"]["data"]["phase"] == "restarted"
            })
            .then_some(())
    });
    let current = api(node, "agent.get", json!({"target":target["pane_id"]}))["agent"].clone();
    assert_eq!(current["agent_id"], target["agent_id"]);
    assert_eq!(current["agent_session"]["value"], "retained-session");
    assert_eq!(removals(node), 0);
    state(node, "harness-restart-mail", "delivered");
    let messages = api(node, "msg.read", json!({"pane":target["pane_id"]}));
    let messages = messages["messages"].as_array().unwrap();
    assert!(messages
        .iter()
        .any(|m| m["correlation_id"] == "harness-restart-mail"));
    assert!(messages.iter().any(|m| m["body"]
        .as_str()
        .is_some_and(|body| body.contains("Restart verified:"))));
}

#[test]
fn codex_restart_confirms_startup_composer() {
    assert_harness_restart(
        "codex",
        include_str!("fixtures/codex/startup-passive-banners.txt"),
    );
}

#[test]
fn codex_restart_confirms_model_footer_without_shortcuts() {
    assert_harness_restart(
        "codex",
        "› Ask Codex to do anything\n\n  GPT-6.1-Sol default · /fixture/work\n",
    );
}

#[test]
fn opencode_restart_confirms_composer() {
    assert_harness_restart(
        "opencode",
        "┃\n┃  Ask anything…\n┃\n┃  Build test-model\n╹\ntab agents ctrl+p commands\n",
    );
}

#[test]
fn codex_resume_failed_keeps_mail_without_tombstone() {
    let fleet = fleet::spawn("ts-codex-fail", SINGLE);
    let node = fleet.node("nodea");
    let target = native_harness(
        node,
        "codex",
        "› Ask Codex to do anything\n\n  GPT-6.1-Sol default · /fixture/work\n",
    );
    let sender = start(node);
    send(node, &sender, &target, "codex-failed-resume-mail");
    fs::remove_file(node.home.join("codex")).unwrap();
    api(
        node,
        "agent.restart",
        json!({"target":target["pane_id"],"reason":"fixture failed Codex resume"}),
    );
    fleet::wait_until("Codex offline resume failure", DEADLINE, || {
        let current = api(node, "agent.get", json!({"target":target["pane_id"]}))["agent"].clone();
        assert_eq!(current["agent_id"], target["agent_id"]);
        (current["agent_status"] == "offline" && current["blocked_reason"] == "resume_failed")
            .then_some(())
    });
    assert_eq!(removals(node), 0);
    state(node, "codex-failed-resume-mail", "delivered");
    let messages = api(node, "msg.read", json!({"pane":target["pane_id"]}));
    assert!(messages["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["correlation_id"] == "codex-failed-resume-mail"));
}

fn stop_hook(node: &Node, agent: &Value) -> String {
    let ready = node.home.join("stop-ready");
    let _ = fs::remove_file(&ready);
    api(
        node,
        "pane.send_text",
        json!({"pane_id":agent["pane_id"],"text":"fixture-stop\n"}),
    );
    fleet::wait_until("Stop hook output", DEADLINE, || {
        ready.exists().then_some(())
    });
    fs::read_to_string(node.home.join("stop-output")).unwrap()
}

// The respawn-shell path belongs to imported agent runtimes. Keep the
// replacement sandbox server owned through assertions, including panic cleanup.
struct RestoredServer<'a>(&'a Node);
impl Drop for RestoredServer<'_> {
    fn drop(&mut self) {
        use std::io::Write;
        if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&self.0.api_socket) {
            let _ = stream
                .write_all(b"{\"id\":\"cleanup\",\"method\":\"server.stop\",\"params\":{}}\n");
        }
    }
}
fn restore_server(node: &Node) -> RestoredServer<'_> {
    let guard = RestoredServer(node);
    api(node, "server.live_handoff", json!({}));
    // The replacement API is ready before asynchronous custody recovery finishes.
    fleet::wait_until("mesh store recovered", DEADLINE, || {
        let status = raw(node, "peers.enrollment", json!({}));
        (status.get("error").is_none() && status["result"]["mesh_suspended_reason"].is_null())
            .then_some(())
    });
    guard
}

fn exit_to_shell(node: &Node, agent: &Value) {
    api(
        node,
        "pane.send_text",
        json!({"pane_id":agent["pane_id"],"text":"exit\n"}),
    );
    fleet::wait_until("agent exited to shell", DEADLINE, || {
        // A successful pane lookup plus absent agent proves shell respawn, not close.
        if raw(node, "pane.get", json!({"pane_id":agent["pane_id"]}))
            .get("error")
            .is_some()
        {
            return None;
        }
        raw(node, "agent.get", json!({"target":agent["pane_id"]}))
            .get("error")
            .map(|_| ())
    });
}
fn replacement_in_same_pane(node: &Node, previous: &Value) -> Value {
    let imported = api(node, "agent.get", json!({"target":previous["pane_id"]}))["agent"].clone();
    assert_eq!(imported["agent_id"], previous["agent_id"]);
    exit_to_shell(node, previous);
    let executable = native_executable(node);
    node.run_in_pane(
        previous["pane_id"].as_str().unwrap(),
        &format!("exec {}", quote(executable.to_str().unwrap())),
    );
    let replacement = fleet::wait_until("replacement agent", DEADLINE, || {
        let result = raw(node, "agent.get", json!({"target":previous["pane_id"]}));
        let agent = &result["result"]["agent"];
        (agent["agent_session"]["value"] == "retained-session").then(|| agent.clone())
    });
    assert_eq!(replacement["pane_id"], previous["pane_id"]);
    assert_eq!(replacement["terminal_id"], imported["terminal_id"]);
    assert_ne!(replacement["agent_id"], previous["agent_id"]);
    replacement
}

#[test]
fn replacement_after_exit_accepts_mail_with_fresh_identity() {
    let fleet = fleet::spawn("ts-reuse", SINGLE);
    let node = fleet.node("nodea");
    let old = start(node);
    let sender = start(node);
    let _restored = restore_server(node);
    send(node, &sender, &old, "removed-unread-mail");
    state(node, "removed-unread-mail", "delivered");
    let replacement = replacement_in_same_pane(node, &old);
    assert_eq!(removals(node), 1);
    state(node, "removed-unread-mail", "recipient_gone");
    assert_eq!(
        api(node, "msg.wake", json!({"pane":replacement["pane_id"]}))["count"],
        0
    );
    assert!(!stop_hook(node, &replacement).contains("unread message"));
    send(node, &sender, &replacement, "replacement-mail");
    state(node, "replacement-mail", "delivered");
    assert!(stop_hook(node, &replacement).contains("1 unread message"));
    let messages = api(node, "msg.read", json!({"pane":replacement["pane_id"]}));
    assert!(messages["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["correlation_id"] == "replacement-mail"));
    assert!(messages["messages"]
        .as_array()
        .unwrap()
        .iter()
        .all(|m| m["correlation_id"] != "removed-unread-mail"));
    state(node, "removed-unread-mail", "recipient_gone");
    let refused = raw(
        node,
        "msg.send",
        json!({"to":{"type":"agent","agent":old["agent_id"]},"body":"old address"}),
    );
    assert_eq!(refused["error"]["code"], "recipient_gone");
    // A second import proves the new identity is persisted by the normal snapshot.
    api(node, "server.live_handoff", json!({}));
    let restored = api(node, "agent.get", json!({"target":replacement["pane_id"]}));
    assert_eq!(restored["agent"]["agent_id"], replacement["agent_id"]);
}

#[test]
fn explicit_kill_terminates_pending_mail_as_recipient_gone_at_origin() {
    let fleet = fleet::spawn("ts-kill", PAIR);
    let owner = fleet.node("nodeb");
    let checkout = owner.home.join("kill-checkout");
    let commit = support::environment::Command::new("git")
        .arg("-C")
        .arg(&owner.repo)
        .args([
            "-c",
            "user.name=Flock Fixture",
            "-c",
            "user.email=flock@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "fixture base",
        ])
        .output()
        .unwrap();
    assert!(
        commit.status.success(),
        "{}",
        String::from_utf8_lossy(&commit.stderr)
    );
    let workspace = api(
        owner,
        "worktree.create",
        json!({"cwd":owner.repo, "path":checkout, "branch":"tombstone-fixture"}),
    );
    let recipient = api(owner, "agent.start", json!({"name":"kill-fixture", "workspace_id":workspace["workspace"]["workspace_id"], "argv":["/bin/sh"]}))["agent"].clone();
    session(owner, &recipient);
    let sender = start(fleet.node("nodea"));
    discover(&fleet, &recipient);
    send(fleet.node("nodea"), &sender, &recipient, "kill-mail");
    state(fleet.node("nodea"), "kill-mail", "delivered");
    api(
        owner,
        "worktree.kill",
        json!({"workspace_id":recipient["workspace_id"],"force":true,"dry_run":false}),
    );
    state(fleet.node("nodea"), "kill-mail", "recipient_gone");
    let reason: String = db(owner)
        .query_row(
            "SELECT reason FROM agent_tombstones WHERE agent_id=?1",
            [recipient["agent_id"].as_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reason, "killed");
}

#[test]
fn stale_directory_send_gets_recipient_gone_from_owner() {
    let fleet = fleet::spawn("ts-stale", PAIR);
    let sender = start(fleet.node("nodea"));
    let target = start(fleet.node("nodeb"));
    discover(&fleet, &target);
    close(fleet.node("nodeb"), &target);
    let result = send(fleet.node("nodea"), &sender, &target, "stale-mail");
    assert_eq!(result["state"], "recipient_gone");
    state(fleet.node("nodea"), "stale-mail", "recipient_gone");
    let local = raw(
        fleet.node("nodeb"),
        "msg.send",
        json!({"to":{"type":"agent", "agent":target["agent_id"]},"body":"late"}),
    );
    assert_eq!(local["error"]["code"], "recipient_gone");
}

#[test]
fn hibernation_does_not_tombstone() {
    let fleet = fleet::spawn("ts-hibernate", SINGLE);
    let node = fleet.node("nodea");
    let target = start(node);
    session(node, &target);
    let sender = start(node);
    send(node, &sender, &target, "hibernate-mail");
    api(node, "agent.hibernate", json!({"target":target["pane_id"]}));
    fleet::wait_until("hibernated", DEADLINE, || {
        (api(node, "agent.get", json!({"target":target["pane_id"]}))["agent"]["agent_status"]
            == "hibernated")
            .then_some(())
    });
    assert_eq!(removals(node), 0);
    state(node, "hibernate-mail", "delivered");
}

#[test]
fn restart_resume_582_keeps_identity_and_delivers_without_tombstone() {
    let fleet = fleet::spawn("ts-restart", SINGLE);
    let node = fleet.node("nodea");
    let target = native_agent(node);
    let sender = start(node);
    send(node, &sender, &target, "restart-mail");
    api(
        node,
        "agent.restart",
        json!({"target":target["pane_id"], "reason":"fixture restart"}),
    );
    fleet::wait_until("restart completed", DEADLINE, || {
        let log = fs::read_to_string(node.config_home.join("flock-dev/event-log.jsonl")).ok()?;
        log.lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .any(|e| {
                e["envelope"]["event"] == "agent_restart"
                    && e["envelope"]["data"]["phase"] == "restarted"
            })
            .then_some(())
    });
    let current = api(node, "agent.get", json!({"target":target["pane_id"]}))["agent"].clone();
    assert_eq!(current["agent_id"], target["agent_id"]);
    assert_eq!(current["agent_session"]["value"], "retained-session");
    assert_eq!(removals(node), 0);
    state(node, "restart-mail", "delivered");
    let messages = api(node, "msg.read", json!({"pane":target["pane_id"]}));
    assert!(messages["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["correlation_id"] == "restart-mail"));
}

#[test]
fn resume_failure_reports_offline_not_removed() {
    let fleet = fleet::spawn("ts-failure", SINGLE);
    let node = fleet.node("nodea");
    let target = native_agent(node);
    let sender = start(node);
    send(node, &sender, &target, "failed-resume-mail");
    // Removing the fixture executable makes #582's direct resume spawn fail.
    fs::remove_file(node.home.join("claude")).unwrap();
    api(
        node,
        "agent.restart",
        json!({"target":target["pane_id"],"reason":"fixture failed resume"}),
    );
    fleet::wait_until("offline resume failure", DEADLINE, || {
        let current = api(node, "agent.get", json!({"target":target["pane_id"]}))["agent"].clone();
        (current["agent_status"] == "offline" && current["blocked_reason"] == "resume_failed")
            .then_some(())
    });
    assert_eq!(removals(node), 0);
    state(node, "failed-resume-mail", "delivered");
}

#[test]
fn reply_to_removed_sender_lands_in_origin_status_not_a_replacement_inbox() {
    let fleet = fleet::spawn("ts-reply", PAIR);
    let origin = fleet.node("nodea");
    let owner = fleet.node("nodeb");
    let sender = start(origin);
    let target = start(owner);
    discover(&fleet, &target);
    send(origin, &sender, &target, "removed-sender");
    api(owner, "msg.read", json!({"pane":target["pane_id"]}));
    let _restored = restore_server(origin);
    let replacement = replacement_in_same_pane(origin, &sender);
    assert_ne!(sender["agent_id"], replacement["agent_id"]);
    api(
        owner,
        "msg.reply",
        json!({"correlation_id":"removed-sender","body":"retained answer"}),
    );
    fleet::wait_until("answer in origin status", DEADLINE, || {
        let status = api(
            origin,
            "msg.status",
            json!({"correlation_id":"removed-sender"}),
        );
        (status["reply"]["body"] == "retained answer").then_some(())
    });
    assert_eq!(
        api(origin, "msg.read", json!({"pane":replacement["pane_id"]}))["messages"],
        json!([])
    );
}

#[test]
fn tombstone_write_failure_does_not_block_pane_close() {
    let fleet = fleet::spawn("ts-write", SINGLE);
    let node = fleet.node("nodea");
    let target = start(node);
    let sender = start(node);
    send(node, &sender, &target, "write-failure");
    db(node).execute_batch("CREATE TRIGGER deny_removal BEFORE INSERT ON agent_tombstones BEGIN SELECT RAISE(ABORT, 'fixture write failure'); END;").unwrap();
    close(node, &target);
    assert!(raw(node, "agent.get", json!({"target":target["pane_id"]}))
        .get("error")
        .is_some());
    assert_eq!(removals(node), 0);
    db(node).execute_batch("DROP TRIGGER deny_removal").unwrap();
    fleet::wait_until("retried tombstone", DEADLINE, || {
        (removals(node) == 1).then_some(())
    });
    state(node, "write-failure", "recipient_gone");
}

#[test]
fn confirmed_exit_only_removes_agents_without_resumable_sessions() {
    let fleet = fleet::spawn("ts-exit", SINGLE);
    let node = fleet.node("nodea");
    let sender = start(node);
    let targets = [start(node), start(node)];
    session(node, &targets[1]);
    let _restored = restore_server(node);
    for (retained, target) in [false, true].into_iter().zip(targets) {
        let correlation = if retained {
            "retained-exit"
        } else {
            "permanent-exit"
        };
        send(node, &sender, &target, correlation);
        exit_to_shell(node, &target);
        if retained {
            state(node, correlation, "delivered");
        } else {
            state(node, correlation, "recipient_gone");
        }
    }
    assert_eq!(removals(node), 1);
}
