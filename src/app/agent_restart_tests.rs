//! Requests and native session reports enter through the API. A fixture harness
//! runs in a real PTY, so stop/resume assertions cover the runtime seam too.
use super::*;
use crate::api::schema::{Method, PaneReportAgentSessionParams, Request};
use crate::detect::Agent;
use std::os::unix::fs::PermissionsExt;

struct Rig {
    app: App,
    id: TerminalId,
    pane: String,
    directory: std::path::PathBuf,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let ids = self
            .app
            .terminal_runtimes
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            if let Some(runtime) = self.app.terminal_runtimes.remove(&id) {
                runtime.shutdown();
            }
        }
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// The default fixture records its argv and `RESTART_PROFILE` in `launch-<pid>`.
/// It writes the record under a dot-prefixed temporary name and renames it into
/// place, so a reader polling for `launch-*` sees either no record or the whole
/// one, never a prefix of it (#1165). The rename stays in one directory, which
/// keeps it a single atomic `rename(2)`.
fn rig() -> Rig {
    rig_with_script("#!/bin/sh\n{ printf '%s\\n' \"$@\"; printf '%s\\n' \"${RESTART_PROFILE:-unset}\"; } > \".launch-$$.partial\"\nmv \".launch-$$.partial\" \"launch-$$\"\nwhile IFS= read -r line; do :; done\n")
}

fn rig_with_script(script: &str) -> Rig {
    rig_with_launch_options(script, &["--model", "fixture-model"])
}

fn rig_with_launch_options(script: &str, options: &[&str]) -> Rig {
    rig_with_harness_options("claude", script, options)
}

fn rig_with_harness_options(agent: &str, script: &str, options: &[&str]) -> Rig {
    let directory = std::env::temp_dir().join(format!(
        "flock-restart-{}-{}",
        std::process::id(),
        super::super::notifications::mint_notification_id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let executable = directory.join(agent);
    std::fs::write(&executable, script).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut config = crate::config::Config::default();
    config.terminal.default_shell = "/bin/sh".into();
    config.session.restart.settle_ms = 0;
    config.session.restart.operator_quiet_ms = 0;
    config.session.restart.flush_wait_ms = 0;
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(&config, true, None, rx, crate::api::EventHub::default());
    app.state
        .workspaces
        .push(crate::workspace::Workspace::test_new("restart-fixture"));
    app.state.ensure_test_terminals();
    let pane_id = app.state.workspaces[0].focused_pane_id().unwrap();
    let id = app.state.workspaces[0]
        .terminal_id(pane_id)
        .unwrap()
        .clone();
    let pane = app.public_pane_id(0, pane_id).unwrap();
    let mut argv = vec![executable.to_string_lossy().into_owned()];
    argv.extend(options.iter().map(|arg| (*arg).to_string()));
    let runtime = crate::terminal::TerminalRuntime::spawn_argv_command(
        pane_id,
        24,
        80,
        directory.clone(),
        &argv,
        1024 * 1024,
        app.state.host_terminal_theme,
        app.event_tx.clone(),
        app.render_notify.clone(),
        app.render_dirty.clone(),
    )
    .unwrap();
    app.terminal_runtimes.insert(id.clone(), runtime);
    let terminal = app.state.terminals.get_mut(&id).unwrap();
    terminal.cwd = directory.clone();
    terminal.launch_argv = Some(argv);
    terminal.launch_env = vec![("RESTART_PROFILE".into(), "fixture-env".into())];
    terminal.set_agent_name("restart-fixture".into());
    let mut rig = Rig {
        app,
        id,
        pane,
        directory,
    };
    report_harness_session(&mut rig, agent, "restart-session", 1);
    rig
}

fn report_session(rig: &mut Rig, session: &str, seq: u64) {
    report_harness_session(rig, "claude", session, seq);
}

fn report_harness_session(rig: &mut Rig, agent: &str, session: &str, seq: u64) {
    rig.app.current_api_peer_pid = rig
        .app
        .terminal_runtimes
        .get(&rig.id)
        .and_then(|r| r.child_pid());
    let response = rig.app.handle_api_request(Request {
        id: "session-report".into(),
        method: Method::PaneReportAgentSession(PaneReportAgentSessionParams {
            pane_id: rig.pane.clone(),
            source: format!("flock:{agent}"),
            agent: agent.into(),
            seq: Some(seq),
            agent_session_id: Some(session.into()),
            agent_session_path: None,
            session_start_source: Some("resume".into()),
        }),
    });
    rig.app.current_api_peer_pid = None;
    assert!(!response.contains("\"error\""), "{response}");
}

fn request(rig: &mut Rig) -> serde_json::Value {
    serde_json::from_str(&rig.app.handle_api_request(Request {
        id: "restart-request".into(),
        method: Method::AgentRestart(AgentRestartParams {
            target: rig.pane.clone(),
            reason: "reload MCP configuration".into(),
            continue_with: Some("finish the assigned task".into()),
            when: "after_turn".into(),
        }),
    }))
    .unwrap()
}

fn idle(rig: &mut Rig, now: Instant, draft: &str) {
    let rule = "─".repeat(40);
    let screen = format!("\x1b[2J\x1b[H{rule}\r\n❯ {draft}\r\n{rule}\r\n");
    rig.app
        .terminal_runtimes
        .get(&rig.id)
        .unwrap()
        .test_process_pty_bytes(screen.as_bytes());
    rig.app
        .state
        .terminals
        .get_mut(&rig.id)
        .unwrap()
        .set_detected_state_with_screen_signals_at(
            Some(Agent::Claude),
            AgentState::Idle,
            false,
            true,
            false,
            false,
            now,
        );
}

fn has_phase(rig: &Rig, phase: &str) -> bool {
    rig.app.event_hub.events_after(0).iter().any(|(_, envelope)| matches!(&envelope.data, EventData::AgentRestart { phase: value, .. } if value == phase))
}

async fn advance_until_verifying(rig: &mut Rig) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !rig
        .app
        .state
        .agent_restarts
        .pending
        .get(&rig.id)
        .is_some_and(|r| matches!(r.phase, Phase::Verifying(_)))
    {
        assert!(Instant::now() < deadline, "resume did not launch");
        // Exercise the real dual-loop ordering: drain runtime events before
        // evaluating timers, keeping the death event on the parked pane.
        while let Ok(event) = rig.app.event_rx.try_recv() {
            rig.app.handle_internal_event(event);
        }
        rig.app.tick_agent_restarts(Instant::now());
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

/// Open the private FIFO before requesting a restart. Keep its writer connected
/// until the fixture opens it, so an unscheduled child cannot look like EOF.
fn launch_report(
    rig: &Rig,
) -> (
    tokio::io::BufReader<tokio::net::unix::pipe::Receiver>,
    tokio::net::unix::pipe::Sender,
) {
    let path = rig.directory.join("resumed");
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: c_path is a live, NUL-terminated path to this rig's private FIFO.
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    let receiver = tokio::net::unix::pipe::OpenOptions::new()
        .open_receiver(&path)
        .unwrap();
    let sender = tokio::net::unix::pipe::OpenOptions::new()
        .open_sender(&path)
        .unwrap();
    (tokio::io::BufReader::new(receiver), sender)
}

/// Consume the complete fixture report, failing on death or a silent producer.
/// The ceiling guards failure only, and never decides whether a launch succeeded.
async fn read_launch_report(
    rig: &mut Rig,
    receiver: &mut tokio::io::BufReader<tokio::net::unix::pipe::Receiver>,
    lines: usize,
) -> String {
    use tokio::io::AsyncBufReadExt;
    let report = tokio::time::timeout(Duration::from_secs(30), async {
        let mut report = Vec::new();
        for _ in 0..lines {
            loop {
                tokio::select! {
                    result = receiver.read_until(b'\n', &mut report) => {
                        assert!(result.unwrap() > 0, "resumed fixture closed its report");
                        break;
                    }
                    event = rig.app.event_rx.recv() => {
                        assert!(event.is_some(), "runtime event channel closed");
                        // A late death event can belong to the original process.
                        // Judge the resumed child itself, not the pane id alone.
                        assert!(!rig.app.restart_runtime_gone(&rig.id),
                            "resumed fixture exited before its report: {}",
                            rig.app.restart_last_output(&rig.id));
                    }
                }
            }
        }
        report
    })
    .await
    .expect("resumed fixture report exceeded the 30-second failure ceiling");
    String::from_utf8(report).unwrap()
}

#[tokio::test]
async fn restart_api_waits_for_turn_boundary_and_does_not_accept_a_draft() {
    let mut rig = rig();
    let original = rig.app.terminal_runtimes.get(&rig.id).unwrap().child_pid();
    rig.app.state.terminals.get_mut(&rig.id).unwrap().state = AgentState::Working;
    assert!(request(&mut rig).get("result").is_some());
    let now = Instant::now();
    rig.app.tick_agent_restarts(now);
    assert_eq!(
        rig.app.terminal_runtimes.get(&rig.id).unwrap().child_pid(),
        original
    );
    idle(&mut rig, now + TICK, "operator draft");
    rig.app.tick_agent_restarts(now + TICK);
    assert_eq!(
        rig.app.terminal_runtimes.get(&rig.id).unwrap().child_pid(),
        original
    );
    assert!(!has_phase(&rig, "restart_stopping"));
    assert!(
        request(&mut rig).get("error").is_some(),
        "duplicate request must not replace the continuation"
    );
}

#[tokio::test]
async fn restart_api_resumes_same_session_flags_env_and_delivers_continuation() {
    let mut rig = rig_with_script("#!/bin/sh\nif [ \"$1\" != --model ]; then printf '%s\\n' \"$@\" \"${RESTART_PROFILE:-unset}\" > resumed; fi\nwhile IFS= read -r line; do :; done\n");
    let (mut receiver, _sender) = launch_report(&rig);
    let original = rig.app.terminal_runtimes.get(&rig.id).unwrap().child_pid();
    assert!(request(&mut rig).get("result").is_some());
    idle(&mut rig, Instant::now(), "");
    advance_until_verifying(&mut rig).await;
    let resumed = rig.app.terminal_runtimes.get(&rig.id).unwrap().child_pid();
    assert_ne!(resumed, original);
    assert_eq!(
        rig.app.mailboxes.wake_count(&rig.pane),
        0,
        "unverified resume must not get a continuation"
    );
    assert!(!rig
        .app
        .state
        .terminals
        .get(&rig.id)
        .unwrap()
        .restart_session_confirmed());
    report_session(&mut rig, "restart-session", 1);
    let now = Instant::now() + TICK;
    idle(&mut rig, now, "");
    rig.app.tick_agent_restarts(now);
    assert!(has_phase(&rig, "restarted"));
    assert_eq!(rig.app.mailboxes.wake_count(&rig.pane), 1);
    assert!(
        !rig.app
            .state
            .terminals
            .get(&rig.id)
            .unwrap()
            .restart_in_progress
    );
    let launch = read_launch_report(&mut rig, &mut receiver, 5).await;
    assert!(
        launch.starts_with("--resume\nrestart-session\n"),
        "{launch}"
    );
    assert!(launch.contains("--model\nfixture-model\n"));
    assert!(launch.ends_with("fixture-env\n"));
}

/// #1179: a restart keeps the question policy on the record, and the resumed
/// process is launched with its hint.
#[tokio::test]
async fn a_restart_keeps_the_question_policy_and_its_hint() {
    // The original launch starts with --model. Every other launch reports its
    // actual arguments, including a restart missing --resume. Reading
    // its report waits for execution rather than assuming spawn means it ran in
    // five seconds (#1283). ADR-0034 §2 requires the record and hint to survive.
    let mut rig = rig_with_script("#!/bin/sh\nif [ \"$1\" != --model ]; then printf '%s\\n' \"$1\" \"$2\" \"${FLOCK_QUESTIONS:-unset}\" > resumed; fi\nwhile IFS= read -r line; do :; done\n");
    let (mut receiver, _sender) = launch_report(&rig);
    rig.app.state.terminals.get_mut(&rig.id).unwrap().questions =
        crate::api::schema::QuestionPolicy::Route;
    assert!(request(&mut rig).get("result").is_some());
    idle(&mut rig, Instant::now(), "");
    advance_until_verifying(&mut rig).await;
    let launch = read_launch_report(&mut rig, &mut receiver, 3).await;
    assert!(
        launch.starts_with("--resume\nrestart-session\n"),
        "{launch}"
    );
    assert!(launch.ends_with("route\n"), "{launch}");
    assert_eq!(
        rig.app.state.terminals[&rig.id].questions,
        crate::api::schema::QuestionPolicy::Route
    );
}

#[tokio::test(flavor = "current_thread")]
async fn restart_api_refuses_fresh_session_and_reports_stuck_without_continuing() {
    let mut rig = rig_with_script("#!/bin/sh\nwhile IFS= read -r line; do exit 0; done\n");
    assert!(request(&mut rig).get("result").is_some());
    idle(&mut rig, Instant::now(), "");
    advance_until_verifying(&mut rig).await;
    report_session(&mut rig, "wrong-session", 1);
    rig.app.tick_agent_restarts(Instant::now() + TICK);
    assert!(has_phase(&rig, "restart_stuck"));
    assert!(!has_phase(&rig, "restarted"));
    assert_eq!(rig.app.mailboxes.wake_count(&rig.pane), 0);
    assert_live_retry(&mut rig);
    assert_eq!(
        rig.app
            .state
            .terminals
            .get(&rig.id)
            .unwrap()
            .persisted_agent_session
            .as_ref()
            .unwrap()
            .session_ref
            .value,
        "wrong-session"
    );
    assert!(rig.app.event_hub.events_after(0).iter().any(|(_, event)| matches!(&event.data, EventData::AgentRestart { phase, detail, .. } if phase == "restart_stuck" && detail.contains("requester's session restart-session"))));
    rig.app
        .terminal_runtimes
        .get(&rig.id)
        .unwrap()
        .try_send_bytes(bytes::Bytes::from_static(b"exit\r"))
        .unwrap();
    // Hold the real watcher event after it publishes the exit slot, then
    // schedule delivery for the next async yield. The old polling/drain path
    // sees a dead child and an empty queue without settling the retry.
    let pane_id = rig.app.state.workspaces[0].focused_pane_id().unwrap();
    let died = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = rig
                .app
                .event_rx
                .recv()
                .await
                .expect("runtime event channel");
            if let crate::events::AppEvent::PaneDied { pane_id: id } = &event {
                assert_eq!(*id, pane_id, "unexpected pane died before resumed child");
                // The original child's watcher can also arrive late. Only
                // a death notification after the resumed child is reaped can
                // settle this retry, as in the production exit handler.
                if rig.app.restart_runtime_gone(&rig.id) {
                    break event;
                }
                continue;
            }
            rig.app.handle_internal_event(event);
        }
    })
    .await
    .expect("resumed child did not report PaneDied within 5 seconds");
    assert!(rig.app.restart_runtime_gone(&rig.id));
    assert!(rig.app.state.terminals[&rig.id].restart_retry.is_some());
    // Isolate delivery from further watcher notifications, so even the
    // failing-before schedule cannot accidentally drain a later death event.
    let (events, delayed_rx) = tokio::sync::mpsc::channel(1);
    let _held_events = std::mem::replace(&mut rig.app.event_rx, delayed_rx);
    let delivery =
        tokio::spawn(async move { events.send(died).await.expect("deferred death event") });
    // ADR-0037 §1: completion is handling PaneDied, not observing the
    // watcher's earlier exit slot or finding its queue temporarily empty.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = rig
                .app
                .event_rx
                .recv()
                .await
                .expect("runtime event channel");
            let died = if let crate::events::AppEvent::PaneDied { pane_id: id } = &event {
                assert_eq!(*id, pane_id, "unexpected pane died while settling retry");
                true
            } else {
                false
            };
            rig.app.handle_internal_event(event);
            if died {
                break;
            }
        }
    })
    .await
    .expect("PaneDied was not handled within the 5-second failure ceiling");
    let terminal = rig.app.state.terminals.get(&rig.id).unwrap();
    assert!(terminal.restart_retry.is_none());
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .unwrap()
            .session_ref
            .value,
        "restart-session"
    );
    assert!(terminal
        .hibernated_resume_plan
        .as_ref()
        .unwrap()
        .argv
        .contains(&"restart-session".into()));
    delivery.await.expect("fixture event delivery task");
    assert_eq!(reported_status(&mut rig), "offline");
    let pane = rig.app.state.workspaces[0].focused_pane_id().unwrap();
    assert!(rig.app.resume_hibernated_pane(0, pane).is_ok());
}

#[tokio::test]
async fn restart_api_grace_expiry_forces_a_busy_agent_and_reports_reason() {
    let mut rig = rig();
    rig.app.state.config.session.restart.restart_grace_secs = 0;
    rig.app.state.terminals.get_mut(&rig.id).unwrap().state = AgentState::Working;
    assert!(request(&mut rig).get("result").is_some());
    advance_until_verifying(&mut rig).await;
    report_session(&mut rig, "restart-session", 1);
    let now = Instant::now() + TICK;
    idle(&mut rig, now, "");
    rig.app.tick_agent_restarts(now);
    assert!(rig.app.event_hub.events_after(0).iter().any(|(_, event)| matches!(&event.data, EventData::AgentRestart { phase, forced: true, reason, .. } if phase == "restarted" && reason.contains("restart_grace"))));
}

#[tokio::test]
async fn restart_hard_limit_rate_cap_stops_and_focus_cannot_resume() {
    let mut rig = rig();
    rig.app.state.config.session.restart.max_restarts = 0;
    let params = AgentRestartParams {
        target: rig.pane.clone(),
        reason: "hard_rss_cap".into(),
        continue_with: None,
        when: "after_turn".into(),
    };
    let response = rig
        .app
        .queue_agent_restart_inner(params, Instant::now(), Some("hard_rss_cap".into()))
        .unwrap();
    assert!(response.2, "forced rate cap must report stop_only");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !has_phase(&rig, "restart_stopped") {
        rig.app.tick_agent_restarts(Instant::now());
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let pane_id = rig.app.state.workspaces[0].focused_pane_id().unwrap();
    assert_eq!(
        rig.app
            .resume_hibernated_pane(0, pane_id)
            .unwrap_err()
            .code(),
        "restart_pending"
    );
    assert!(rig.app.terminal_runtimes.get(&rig.id).is_none());
    assert_eq!(rig.app.mailboxes.wake_count(&rig.pane), 0);
}

#[tokio::test]
async fn restart_api_off_switch_and_missing_self_ancestry_refuse_without_stopping() {
    let mut rig = rig();
    let pid = rig.app.terminal_runtimes.get(&rig.id).unwrap().child_pid();
    rig.app.state.config.session.restart.enabled = false;
    assert_eq!(request(&mut rig)["error"]["code"], "restart_disabled");
    rig.app.state.config.session.restart.enabled = true;
    let result = rig
        .app
        .queue_agent_restart(
            AgentRestartParams {
                target: "self".into(),
                reason: "reload".into(),
                continue_with: None,
                when: "after_turn".into(),
            },
            Instant::now(),
        )
        .unwrap_err();
    assert_eq!(result.code, "no_caller_pane");
    assert_eq!(
        rig.app.terminal_runtimes.get(&rig.id).unwrap().child_pid(),
        pid
    );
}

#[tokio::test]
async fn restart_api_requires_idle_evidence_after_the_request() {
    let mut rig = rig();
    let original = rig.app.terminal_runtimes.get(&rig.id).unwrap().child_pid();
    assert!(request(&mut rig).get("result").is_some());
    let now = Instant::now();
    idle(&mut rig, now - Duration::from_secs(1), "");
    rig.app.tick_agent_restarts(now);
    assert_eq!(
        rig.app.terminal_runtimes.get(&rig.id).unwrap().child_pid(),
        original
    );
    assert!(!has_phase(&rig, "restart_stopping"));
}

#[tokio::test]
async fn restart_automatic_rss_limit_stops_without_waiting_for_idle() {
    let mut rig = rig();
    rig.app.state.config.session.restart.hard_rss_cap = 1;
    rig.app.state.terminals.get_mut(&rig.id).unwrap().state = AgentState::Working;
    advance_until_verifying(&mut rig).await;
    assert!(rig.app.event_hub.events_after(0).iter().any(|(_, event)| matches!(&event.data, EventData::AgentRestart { phase, forced: true, reason, .. } if phase == "restart_stopping" && reason.contains("hard_rss_cap"))));
}

#[tokio::test]
async fn restart_api_unknown_startup_dialog_times_out_without_continuing() {
    let mut rig = rig();
    rig.app.state.config.session.restart.verify_timeout_secs = 0;
    assert!(request(&mut rig).get("result").is_some());
    idle(&mut rig, Instant::now(), "");
    advance_until_verifying(&mut rig).await;
    report_session(&mut rig, "restart-session", 1);
    rig.app
        .terminal_runtimes
        .get(&rig.id)
        .unwrap()
        .test_process_pty_bytes(
            b"\x1b[2J\x1b[HUnknown startup question: requires the operator\r\n",
        );
    rig.app.tick_agent_restarts(Instant::now() + TICK);
    assert!(has_phase(&rig, "restart_stuck"));
    assert_eq!(rig.app.mailboxes.wake_count(&rig.pane), 0);
    assert!(rig.app.event_hub.events_after(0).iter().any(|(_, event)| matches!(&event.data, EventData::AgentRestart { phase, detail, .. } if phase == "restart_stuck" && detail.contains("Unknown startup question") && detail.contains("unknown_composer"))));
}

#[tokio::test]
async fn restart_shutdown_kills_term_resistant_detached_children() {
    let mut rig = rig_with_script("#!/bin/sh\npython3 -c 'import os, signal, time; os.setsid(); signal.signal(signal.SIGTERM, signal.SIG_IGN); open(\"child.pid\", \"w\").write(str(os.getpid())); time.sleep(100)' &\nwhile IFS= read -r line; do :; done\n");
    rig.app.state.config.session.restart.kill_grace_secs = 0;
    let deadline = Instant::now() + Duration::from_secs(5);
    let child = loop {
        if let Ok(text) = std::fs::read_to_string(rig.directory.join("child.pid")) {
            if let Ok(pid) = text.parse::<u32>() {
                break pid;
            }
        }
        assert!(Instant::now() < deadline, "detached child did not start");
        tokio::time::sleep(Duration::from_millis(30)).await;
    };
    assert!(crate::platform::process_exists(child));
    assert!(request(&mut rig).get("result").is_some());
    idle(&mut rig, Instant::now(), "");
    advance_until_verifying(&mut rig).await;
    assert!(
        !crate::platform::process_exists(child),
        "detached child survived restart"
    );
}

#[tokio::test]
async fn restart_explicit_rate_cap_refuses_without_stopping() {
    let mut rig = rig();
    let now = Instant::now();
    let agent = rig
        .app
        .state
        .terminals
        .get(&rig.id)
        .unwrap()
        .agent_id
        .to_string();
    rig.app.state.config.session.restart.max_restarts = 1;
    rig.app
        .state
        .agent_restarts
        .history
        .insert(agent, vec![now]);
    let original = rig.app.terminal_runtimes.get(&rig.id).unwrap().child_pid();
    let result = request(&mut rig);
    assert_eq!(result["error"]["code"], "restart_rate_limited");
    assert!(result["error"]["message"]
        .as_str()
        .unwrap()
        .contains("retry_after_secs="));
    assert!(rig.app.state.agent_restarts.pending.is_empty());
    assert!(
        !rig.app
            .state
            .terminals
            .get(&rig.id)
            .unwrap()
            .restart_in_progress
    );
    assert_eq!(
        rig.app.terminal_runtimes.get(&rig.id).unwrap().child_pid(),
        original
    );
}

#[tokio::test]
async fn restart_stop_failure_unlocks_and_keeps_retry_plan() {
    let mut rig = rig();
    assert!(request(&mut rig).get("result").is_some());
    let mut request = rig
        .app
        .state
        .agent_restarts
        .pending
        .remove(&rig.id)
        .unwrap();
    request.phase = Phase::Stopping;
    rig.app
        .terminal_runtimes
        .remove(&rig.id)
        .unwrap()
        .shutdown();
    let (tx, rx) = tokio::sync::oneshot::channel();
    rig.app.restarts.shutdowns.insert(rig.id.clone(), rx);
    tx.send(false).unwrap();
    assert!(!rig
        .app
        .advance_restart(&rig.id, &mut request, Instant::now()));
    let terminal = rig.app.state.terminals.get(&rig.id).unwrap();
    assert!(!terminal.restart_in_progress);
    assert_eq!(
        terminal.hibernated_resume_plan.as_ref(),
        Some(&request.plan)
    );
    assert!(has_phase(&rig, "restart_stuck"));
    let pane = rig.app.state.workspaces[0].focused_pane_id().unwrap();
    assert!(rig.app.resume_hibernated_pane(0, pane).is_ok());
}

async fn wait_for_dead_runtime(rig: &Rig) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !rig.app.restart_runtime_gone(&rig.id) {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn restart_waiting_agent_exit_cancels_immediately_and_allows_retry() {
    let mut rig = rig_with_script("#!/bin/sh\nwhile IFS= read -r line; do exit 0; done\n");
    assert!(request(&mut rig).get("result").is_some());
    rig.app
        .terminal_runtimes
        .get(&rig.id)
        .unwrap()
        .try_send_bytes(bytes::Bytes::from_static(b"exit\r"))
        .unwrap();
    // The watcher publishes its exit slot before enqueueing PaneDied. Await
    // the event that actually cancels the restart, including a delayed watcher.
    let pane_id = rig.app.state.workspaces[0].focused_pane_id().unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let event = rig
                .app
                .event_rx
                .recv()
                .await
                .expect("runtime event channel");
            let died =
                matches!(&event, crate::events::AppEvent::PaneDied { pane_id: id } if *id == pane_id);
            rig.app.handle_internal_event(event);
            if died {
                break;
            }
        }
    })
    .await
    .expect("PaneDied did not arrive within the 30-second failure ceiling");
    assert!(has_phase(&rig, "restart_cancelled"));
    assert!(rig.app.state.agent_restarts.pending.is_empty());
    let terminal = rig.app.state.terminals.get(&rig.id).unwrap();
    assert!(!terminal.restart_in_progress);
    assert!(terminal.hibernated_resume_plan.is_some());
    let pane = rig.app.state.workspaces[0].focused_pane_id().unwrap();
    assert!(rig.app.resume_hibernated_pane(0, pane).is_ok());
}

#[tokio::test]
async fn restart_resumed_agent_exit_retains_output_and_retry_plan() {
    let mut rig = rig_with_script("#!/bin/sh\nif [ \"$1\" = --resume ]; then printf 'No conversation found\\n'; exit 1; fi\nwhile IFS= read -r line; do :; done\n");
    assert!(request(&mut rig).get("result").is_some());
    idle(&mut rig, Instant::now(), "");
    advance_until_verifying(&mut rig).await;
    wait_for_dead_runtime(&rig).await;
    while let Ok(event) = rig.app.event_rx.try_recv() {
        rig.app.handle_internal_event(event);
    }
    assert!(rig.app.state.agent_restarts.pending.is_empty());
    let terminal = rig.app.state.terminals.get(&rig.id).unwrap();
    assert!(!terminal.restart_in_progress);
    assert!(terminal.hibernated_resume_plan.is_some());
    assert!(rig.app.event_hub.events_after(0).iter().any(|(_, event)| matches!(&event.data, EventData::AgentRestart { phase, detail, .. } if phase == "restart_stuck" && detail.contains("No conversation found") && detail.contains("flk agent resume"))));
    let pane = rig.app.state.workspaces[0].focused_pane_id().unwrap();
    assert!(rig.app.resume_hibernated_pane(0, pane).is_ok());
}

#[tokio::test]
async fn restart_grace_expiry_waits_for_operator_quiet() {
    let mut rig = rig();
    rig.app.state.config.session.restart.restart_grace_secs = 0;
    rig.app.state.config.session.restart.operator_quiet_ms = 1000;
    assert!(request(&mut rig).get("result").is_some());
    let now = Instant::now();
    rig.app
        .terminal_runtimes
        .get(&rig.id)
        .unwrap()
        .test_stamp_operator_input_at(now);
    idle(&mut rig, now, "human draft");
    rig.app.tick_agent_restarts(now);
    assert!(!has_phase(&rig, "restart_stopping"));
    rig.app.tick_agent_restarts(now + Duration::from_secs(1));
    assert!(has_phase(&rig, "restart_stopping"));
}

#[tokio::test]
async fn restart_handoff_cancels_pending_and_verifying_requests() {
    for verifying in [false, true] {
        let mut rig = rig();
        assert!(request(&mut rig).get("result").is_some());
        if verifying {
            idle(&mut rig, Instant::now(), "");
            advance_until_verifying(&mut rig).await;
        }
        rig.app.cancel_agent_restarts_for_handoff().unwrap();
        assert!(rig.app.state.agent_restarts.pending.is_empty());
        assert!(
            !rig.app
                .state
                .terminals
                .get(&rig.id)
                .unwrap()
                .restart_in_progress
        );
        assert!(has_phase(&rig, "restart_cancelled"));
        let events = rig.app.event_hub.events_after(0);
        let mut notifications = super::super::notifications::NotificationLog::default();
        notifications.seed_from_events(events.iter().map(|(_, event)| event));
        assert!(notifications
            .newest_first()
            .any(|entry| entry.pane_id.as_deref() == Some(&rig.pane)
                && entry
                    .body
                    .as_ref()
                    .is_some_and(|body| body.contains("restart_cancelled"))));
        let mut inbox = super::super::mailboxes::MailboxRegistry::default();
        inbox.seed_from_events(events.iter().map(|(_, event)| event));
        assert!(inbox.pop_next(&rig.pane).unwrap().body.contains("handoff"));
        assert!(rig
            .app
            .mailboxes
            .pop_next(&rig.pane)
            .unwrap()
            .body
            .contains("handoff"));
        assert!(rig
            .app
            .state
            .terminals
            .get(&rig.id)
            .unwrap()
            .prompt_history
            .iter()
            .any(|entry| entry.text.contains("restart_cancelled")));
        assert!(rig.app.terminal_runtimes.get(&rig.id).is_some());
    }
}

#[tokio::test]
async fn restart_handoff_refuses_during_process_teardown() {
    let mut rig = rig();
    assert!(request(&mut rig).get("result").is_some());
    rig.app
        .state
        .agent_restarts
        .pending
        .get_mut(&rig.id)
        .unwrap()
        .phase = Phase::Stopping;
    assert_eq!(
        rig.app
            .cancel_agent_restarts_for_handoff()
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(
        rig.app
            .state
            .terminals
            .get(&rig.id)
            .unwrap()
            .restart_in_progress
    );
    assert!(!has_phase(&rig, "restart_cancelled"));
}

#[tokio::test]
async fn restart_refuses_harness_without_confirmed_session_hooks() {
    let mut rig = rig();
    rig.app
        .state
        .terminals
        .get_mut(&rig.id)
        .unwrap()
        .prepare_restart_resume();
    let original = rig.app.terminal_runtimes.get(&rig.id).unwrap().child_pid();
    assert_eq!(request(&mut rig)["error"]["code"], "restart_unsupported");
    assert_eq!(
        rig.app.terminal_runtimes.get(&rig.id).unwrap().child_pid(),
        original
    );
    assert!(
        !rig.app
            .state
            .terminals
            .get(&rig.id)
            .unwrap()
            .restart_in_progress
    );
}

#[tokio::test]
async fn restart_grace_after_completed_turn_reports_idle_grace() {
    let mut rig = rig();
    rig.app.state.config.session.restart.restart_grace_secs = 0;
    assert!(request(&mut rig).get("result").is_some());
    idle(&mut rig, Instant::now(), "");
    advance_until_verifying(&mut rig).await;
    report_session(&mut rig, "restart-session", 1);
    let now = Instant::now() + TICK;
    idle(&mut rig, now, "");
    rig.app.tick_agent_restarts(now);
    let message = rig.app.mailboxes.pop_next(&rig.pane).unwrap();
    assert!(message
        .body
        .contains("force-restarted after the idle grace"));
    assert!(!message.body.contains("force-restarted mid-turn"));
}

#[tokio::test]
async fn restart_lost_process_report_excludes_harness_and_mcp_subtrees() {
    let mut rig = rig_with_script("#!/bin/sh\npython3 -c 'import os,time;open(\"mcp.pid\",\"w\").write(str(os.getpid()));time.sleep(100)' mcp &\n/bin/sh -c 'echo $$ > background.pid; sleep 100' &\nwhile IFS= read -r line; do :; done\n");
    let deadline = Instant::now() + Duration::from_secs(5);
    let (mcp, background) = loop {
        if let (Ok(mcp), Ok(background)) = (
            std::fs::read_to_string(rig.directory.join("mcp.pid")),
            std::fs::read_to_string(rig.directory.join("background.pid")),
        ) {
            if let (Ok(mcp), Ok(background)) =
                (mcp.trim().parse::<u32>(), background.trim().parse::<u32>())
            {
                break (mcp, background);
            }
        }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    rig.app.sample_restart_vitals(Instant::now());
    let vitals = rig.app.restarts.samples.get(&rig.id).unwrap();
    let root = rig
        .app
        .terminal_runtimes
        .get(&rig.id)
        .unwrap()
        .child_pid()
        .unwrap();
    assert!(
        vitals.pids.contains(&root)
            && vitals.pids.contains(&mcp)
            && vitals.pids.contains(&background),
        "filtering loss reports must not shrink kill scope"
    );
    let losses = vitals.processes.join(", ");
    assert!(!losses.contains(&format!("pid {root})")));
    assert!(!losses.contains(&format!("pid {mcp})")));
    assert!(losses.contains(&format!("pid {background})")));
    assert!(request(&mut rig).get("result").is_some());
    let mut request = rig
        .app
        .state
        .agent_restarts
        .pending
        .remove(&rig.id)
        .unwrap();
    request.lost = rig
        .app
        .restarts
        .samples
        .get(&rig.id)
        .unwrap()
        .processes
        .clone();
    request.lost.push("fixture background watcher".into());
    rig.app
        .report_restart(&rig.id, &request, "restarted", "verified".into());
    rig.app.drain_pending_ui_events();
    assert!(rig.app.event_hub.events_after(0).iter().any(|(_, event)| matches!(&event.data, EventData::NotificationFiled { body: Some(body), .. } if body.contains("Lost background") && body.contains("fixture background watcher") && body.contains(&format!("pid {background})")))));
}

#[tokio::test]
async fn restart_failed_live_verification_can_retry_after_process_exits() {
    let mut rig = rig_with_script("#!/bin/sh\nwhile IFS= read -r line; do exit 0; done\n");
    rig.app.state.config.session.restart.verify_timeout_secs = 0;
    assert!(request(&mut rig).get("result").is_some());
    idle(&mut rig, Instant::now(), "");
    advance_until_verifying(&mut rig).await;
    rig.app.tick_agent_restarts(Instant::now() + TICK);
    assert!(has_phase(&rig, "restart_stuck"));
    assert_live_retry(&mut rig);
    rig.app
        .terminal_runtimes
        .get(&rig.id)
        .unwrap()
        .try_send_bytes(bytes::Bytes::from_static(b"exit\r"))
        .unwrap();
    wait_for_dead_runtime(&rig).await;
    while let Ok(event) = rig.app.event_rx.try_recv() {
        rig.app.handle_internal_event(event);
    }
    assert!(rig.app.terminal_runtimes.get(&rig.id).is_none());
    assert!(rig
        .app
        .state
        .terminals
        .get(&rig.id)
        .unwrap()
        .restart_retry
        .is_none());
    assert_eq!(reported_status(&mut rig), "offline");
    let pane = rig.app.state.workspaces[0].focused_pane_id().unwrap();
    assert!(rig.app.resume_hibernated_pane(0, pane).is_ok());
}

fn reported_status(rig: &mut Rig) -> serde_json::Value {
    let response = rig.app.handle_api_request(Request {
        id: "restart-status".into(),
        method: Method::AgentGet(crate::api::schema::AgentTarget {
            target: rig.pane.clone(),
        }),
    });
    let value: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert!(value.get("error").is_none(), "{value}");
    let status = value["result"]["agent"]["agent_status"].clone();
    assert!(status.is_string(), "missing agent status: {value}");
    status
}

fn assert_live_retry(rig: &mut Rig) {
    assert_ne!(reported_status(rig), "hibernated");
    let terminal = rig.app.state.terminals.get(&rig.id).unwrap();
    assert!(!terminal.restart_in_progress);
    assert!(terminal.hibernated_resume_plan.is_none());
    assert!(terminal.restart_retry.is_some());
    assert!(!rig.app.restart_runtime_gone(&rig.id));
    let response = rig.app.handle_api_request(Request {
        id: "resume-live-retry".into(),
        method: Method::AgentResume(crate::api::schema::AgentTarget {
            target: rig.pane.clone(),
        }),
    });
    let value: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(value["error"]["code"], "not_hibernated");
}

#[tokio::test]
async fn restart_live_retry_does_not_override_deliberate_hibernation() {
    let mut rig = rig();
    assert!(request(&mut rig).get("result").is_some());
    idle(&mut rig, Instant::now(), "");
    advance_until_verifying(&mut rig).await;
    report_session(&mut rig, "wrong-session", 1);
    rig.app.tick_agent_restarts(Instant::now() + TICK);
    assert_live_retry(&mut rig);
    let pane = rig.app.state.workspaces[0].focused_pane_id().unwrap();
    rig.app.hibernate_pane(0, pane).unwrap();
    while let Ok(event) = rig.app.event_rx.try_recv() {
        rig.app.handle_internal_event(event);
    }
    let terminal = rig.app.state.terminals.get(&rig.id).unwrap();
    assert!(terminal.restart_retry.is_none());
    assert!(
        terminal
            .hibernated_resume_plan
            .as_ref()
            .unwrap()
            .argv
            .contains(&"wrong-session".into()),
        "an operator's deliberate park supersedes the deferred restart retry"
    );
}

#[tokio::test]
async fn restart_messages_detach_leftover_relays_before_the_next_request() {
    let store = crate::mesh::runtime_store::TestStore::new();
    let (_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &crate::config::Config::default(),
        true,
        None,
        api_rx,
        crate::api::EventHub::default(),
    );
    app.message_relays.pending = Some(crate::app::message_relay::RelaySend {
        mesh: store.delivery(),
        id: "detached".into(),
        peer: crate::config::PeerConfig::default(),
        // Refused locally, without dialing a peer.
        to_agent: "invalid recipient".into(),
        host: "nodeb".into(),
        direct: true,
        from_agent: "agent_nodea_sender".into(),
        correlation_id: "detached".into(),
        intent: MsgIntent::Fyi,
        respond_to: None,
    });
    app.restart_message_with_intent(
        "agent_nodea_gone".into(),
        "restart notice".into(),
        MsgIntent::Fyi,
    );
    assert!(app.message_relays.pending.is_none());
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), app.event_rx.recv())
        .await
        .unwrap()
        .expect("detached relay completes");
    let crate::events::AppEvent::MsgRelayCompleted(completion) = &event else {
        panic!("expected relay completion");
    };
    assert_eq!(completion.send.id, "detached");
    assert!(completion.send.respond_to.is_none());
    app.handle_internal_event(event);
}

#[tokio::test]
async fn restart_composer_rejects_unknown_harness_drafts_and_dialogs() {
    let mut rig = rig();
    for (agent, screen, reason) in [
        ("pi", "─────────────\n❯ \n─────────────\n", "unknown_composer"),
        ("codex", "› operator draft\n  ? for shortcuts\n", "composer_not_empty"),
        ("opencode", "┃\n┃ operator draft\n┃\n╹\ntab agents ctrl+p commands\n", "composer_not_empty"),
        ("codex", include_str!("../../tests/fixtures/codex/startup-update-dialog.txt"), "unknown_composer"),
        ("opencode", "┃\n┃ Ask anything…\n┃\n╹\ntab agents ctrl+p commands\n↑↓ select enter confirm esc dismiss\n", "unknown_composer"),
    ] {
        rig.app.state.terminals.get_mut(&rig.id).unwrap().persisted_agent_session.as_mut().unwrap().agent = agent.into();
        let screen = format!("\x1b[2J\x1b[H{}", screen.replace('\n', "\r\n"));
        rig.app.terminal_runtimes.get(&rig.id).unwrap().test_process_pty_bytes(screen.as_bytes());
        assert_eq!(rig.app.restart_composer_ready(&rig.id), Err(reason), "{agent}: {screen}");
    }
}

#[tokio::test]
async fn restart_opencode_requires_fresh_observation_and_empty_composer_without_idle_hook() {
    let mut rig = rig();
    let now = Instant::now();
    let terminal = rig.app.state.terminals.get_mut(&rig.id).unwrap();
    terminal.prepare_restart_resume();
    terminal.persisted_agent_session.as_mut().unwrap().agent = "opencode".into();
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::OpenCode),
        AgentState::Idle,
        false,
        false,
        false,
        false,
        now,
    );
    rig.app
        .terminal_runtimes
        .get(&rig.id)
        .unwrap()
        .test_process_pty_bytes(
            "\x1b[2J\x1b[H┃\r\n┃ Ask anything…\r\n┃\r\n╹\r\ntab agents ctrl+p commands\r\n"
                .as_bytes(),
        );
    let policy = rig.app.restart_policy(&rig.id);
    assert!(rig.app.restart_ready(&rig.id, now, &policy, now));
    assert!(!rig.app.restart_ready(&rig.id, now, &policy, now + TICK));
    assert!(!rig
        .app
        .restart_ready(&rig.id, now + Duration::from_secs(10), &policy, now));
}

async fn claude_verifying_rig() -> Rig {
    let mut rig = rig();
    assert!(request(&mut rig).get("result").is_some());
    idle(&mut rig, Instant::now(), "");
    advance_until_verifying(&mut rig).await;
    report_session(&mut rig, "restart-session", 1);
    rig
}

#[tokio::test]
async fn restart_claude_empty_composer_verifies_and_delivers_continuation() {
    let mut rig = claude_verifying_rig().await;
    let now = Instant::now() + TICK;
    idle(&mut rig, now, "");
    assert_eq!(rig.app.restart_composer_ready(&rig.id), Ok(()));
    rig.app.tick_agent_restarts(now);
    assert!(has_phase(&rig, "restarted"));
    assert!(!has_phase(&rig, "restart_stuck"));
    assert_eq!(rig.app.mailboxes.wake_count(&rig.pane), 1);
}

#[tokio::test]
async fn restart_claude_draft_reports_composer_not_empty_without_continuing() {
    let mut rig = claude_verifying_rig().await;
    rig.app.state.config.session.restart.verify_timeout_secs = 0;
    let now = Instant::now() + TICK;
    idle(&mut rig, now, "operator draft");
    assert_eq!(
        rig.app.restart_composer_ready(&rig.id),
        Err("composer_not_empty")
    );
    rig.app.tick_agent_restarts(now);
    assert!(!has_phase(&rig, "restarted"));
    assert_eq!(rig.app.mailboxes.wake_count(&rig.pane), 0);
    assert!(rig.app.event_hub.events_after(0).iter().any(|(_, event)| matches!(
        &event.data, EventData::AgentRestart { phase, detail, .. }
        if phase == "restart_stuck" && detail.contains("verification timed out (composer_not_empty)")
    )));
}

#[tokio::test]
async fn restart_claude_empty_composer_timeout_reports_missing_idle_observation() {
    let mut rig = claude_verifying_rig().await;
    rig.app.state.config.session.restart.verify_timeout_secs = 0;
    let Phase::Verifying(started) = rig.app.state.agent_restarts.pending[&rig.id].phase else {
        panic!("expected verifying phase");
    };
    idle(&mut rig, started - TICK, "");
    assert_eq!(rig.app.restart_composer_ready(&rig.id), Ok(()));
    rig.app.tick_agent_restarts(Instant::now() + TICK);
    assert!(!has_phase(&rig, "restarted"));
    assert_eq!(rig.app.mailboxes.wake_count(&rig.pane), 0);
    assert!(rig.app.event_hub.events_after(0).iter().any(|(_, event)| matches!(
        &event.data, EventData::AgentRestart { phase, detail, .. }
        if phase == "restart_stuck" && detail.contains("verification timed out (idle_observation_missing)")
    )));
}

/// The native report binds the session before the API reconstructs launch options.
#[tokio::test]
async fn restart_optional_1109_reports_drops_and_launches_same_session() {
    let options = [
        "--model",
        "fixture-model",
        "--prompt-suggestions",
        "false",
        "--tools",
        "Read",
        "Edit",
        "--file",
        "file_one:one.txt",
        "file_two:two.txt",
        "--worktree",
        "fixture-tree",
        "-w=another-tree",
        "--from-pr=42",
        "-r",
        "old-session",
    ];
    let mut rig = rig_with_launch_options("#!/bin/sh\nif [ \"$1\" = --resume ]; then { printf '%s\\n' \"$@\" \"${RESTART_PROFILE:-unset}\"; pwd -P; } > resumed; fi\nwhile IFS= read -r line; do :; done\n", &options);
    let cwd = rig.app.state.terminals[&rig.id].cwd.clone();
    let (mut receiver, _sender) = launch_report(&rig);
    let response = request(&mut rig);
    assert!(response.get("result").is_some(), "{response}");
    let dropped = response["result"]["dropped_flags"].as_array().unwrap();
    assert_eq!(
        dropped
            .iter()
            .map(|entry| entry["flag"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["--worktree", "-w", "--from-pr"]
    );
    assert!(dropped
        .iter()
        .all(|entry| !entry["reason"].as_str().unwrap().is_empty()));
    idle(&mut rig, Instant::now(), "");
    advance_until_verifying(&mut rig).await;
    let report = read_launch_report(&mut rig, &mut receiver, 14).await;
    assert_eq!(report, format!("--resume\nrestart-session\n--model\nfixture-model\n--prompt-suggestions\nfalse\n--tools\nRead\nEdit\n--file\nfile_one:one.txt\nfile_two:two.txt\nfixture-env\n{}\n", rig.directory.canonicalize().unwrap().display()));
    assert_eq!(rig.app.state.terminals[&rig.id].cwd, cwd);
    report_session(&mut rig, "restart-session", 1);
    let now = Instant::now() + TICK;
    idle(&mut rig, now, "");
    rig.app.tick_agent_restarts(now);
    assert!(has_phase(&rig, "restarted"));
    assert!(rig.app.event_hub.events_after(0).iter().any(|(_, event)| matches!(
        &event.data, EventData::AgentRestart { phase, detail, .. }
        if phase == "restarted" && detail.contains("--worktree") && detail.contains("--from-pr") && detail.contains("existing cwd")
    )));
}

#[tokio::test]
async fn restart_optional_1109_refuses_open_or_remote_flags_without_stopping() {
    for option in [
        "--remote-control=fixture",
        "--prompt-suggestions=maybe",
        "--cloud=fixture",
        "--teleport=fixture",
    ] {
        let mut rig = rig_with_launch_options(
            "#!/bin/sh\nwhile IFS= read -r line; do :; done\n",
            &["--model", "fixture-model", option],
        );
        let pid = rig.app.terminal_runtimes.get(&rig.id).unwrap().child_pid();
        let response = request(&mut rig);
        assert_eq!(
            response["error"]["code"], "restart_unsupported",
            "{option}: {response}"
        );
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains(option.split('=').next().unwrap()));
        assert_eq!(
            rig.app.terminal_runtimes.get(&rig.id).unwrap().child_pid(),
            pid
        );
        assert!(rig.app.state.agent_restarts.is_empty());
    }
}

#[tokio::test]
async fn restart_inline_1109_drops_the_original_prompt_in_the_native_launch() {
    let mut rig = rig_with_launch_options("#!/bin/sh\nif [ \"$1\" = --resume ]; then { printf '%s\\n' \"$@\" \"${RESTART_PROFILE:-unset}\"; pwd -P; } > resumed; fi\nwhile IFS= read -r line; do :; done\n", &["--model", "fixture-model", "--tools=Read", "implement the fixture"]);
    let (mut receiver, _sender) = launch_report(&rig);
    let response = request(&mut rig);
    assert!(response.get("result").is_some(), "{response}");
    idle(&mut rig, Instant::now(), "");
    advance_until_verifying(&mut rig).await;
    let report = read_launch_report(&mut rig, &mut receiver, 7).await;
    assert_eq!(
        report,
        format!(
            "--resume\nrestart-session\n--model\nfixture-model\n--tools=Read\nfixture-env\n{}\n",
            rig.directory.canonicalize().unwrap().display()
        )
    );
}

/// The isolated fake PTY and native report feed the real restart API planner.
#[tokio::test]
async fn restart_1659_plugin_directory_does_not_replay_the_opening_prompt() {
    let mut rig = rig_with_launch_options(
        "#!/bin/sh\nwhile IFS= read -r line; do :; done\n",
        &["--plugin-dir", "fixture-plugin", "opening prompt"],
    );
    let response = request(&mut rig);
    assert!(response.get("result").is_some(), "{response}");
    let plan = &rig.app.state.agent_restarts.pending[&rig.id].plan.argv;
    assert_eq!(
        &plan[1..],
        [
            "--resume",
            "restart-session",
            "--plugin-dir",
            "fixture-plugin"
        ]
    );
}

#[tokio::test]
async fn restart_1659_claude_print_is_not_a_value_taking_option() {
    let mut rig = rig_with_launch_options(
        "#!/bin/sh\nwhile IFS= read -r line; do :; done\n",
        &["-p", "--model", "fixture-model", "opening prompt"],
    );
    let response = request(&mut rig);
    assert!(response.get("result").is_some(), "{response}");
    let plan = &rig.app.state.agent_restarts.pending[&rig.id].plan.argv;
    assert_eq!(
        &plan[1..],
        [
            "--resume",
            "restart-session",
            "-p",
            "--model",
            "fixture-model"
        ]
    );
}

#[tokio::test]
async fn restart_refusal_envelope_omits_attached_values_without_stopping() {
    for (option, flag, omitted) in [
        (
            "-pfixture-sensitive-profile",
            "-p",
            "fixture-sensitive-profile",
        ),
        (
            "-mfixture-sensitive-profile",
            "-m",
            "fixture-sensitive-profile",
        ),
        ("-abc", "-a", "bc"),
        (
            "--unverified=fixture-sensitive-profile",
            "--unverified",
            "fixture-sensitive-profile",
        ),
    ] {
        let mut rig = rig_with_launch_options(
            "#!/bin/sh\nwhile IFS= read -r line; do :; done\n",
            &[option],
        );
        let pid = rig.app.terminal_runtimes.get(&rig.id).unwrap().child_pid();
        let response = request(&mut rig);
        assert_eq!(
            response["error"]["code"], "restart_unsupported",
            "{response}"
        );
        assert!(
            !response.to_string().contains(omitted),
            "value in error envelope: {response}"
        );
        let message = response["error"]["message"].as_str().unwrap();
        assert!(
            message.contains(flag) && message.contains("(attached value omitted)"),
            "{response}"
        );
        assert_eq!(
            rig.app.terminal_runtimes.get(&rig.id).unwrap().child_pid(),
            pid
        );
        assert!(rig.app.state.agent_restarts.is_empty());
    }
}

/// Pin Codex's concatenated config override through the complete API envelope.
#[tokio::test]
async fn restart_codex_concatenated_config_refusal_omits_key_and_value() {
    let mut rig = rig_with_harness_options(
        "codex",
        "#!/bin/sh\nwhile IFS= read -r line; do :; done\n",
        &["-cfixture-sensitive-key=fixture-sensitive-value"],
    );
    let pid = rig.app.terminal_runtimes.get(&rig.id).unwrap().child_pid();
    let response = request(&mut rig);
    assert_eq!(
        response["error"]["code"], "restart_unsupported",
        "{response}"
    );
    let message = response["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("-c (attached value omitted)"),
        "{response}"
    );
    // Serialize the entire response, including optional error.data.
    let envelope = response.to_string();
    for sensitive in ["fixture-sensitive-key", "fixture-sensitive-value"] {
        assert!(
            !envelope.contains(sensitive),
            "value in error envelope: {response}"
        );
    }
    assert!(pid.is_some(), "fixture must have a live child");
    assert_eq!(
        rig.app.terminal_runtimes.get(&rig.id).unwrap().child_pid(),
        pid
    );
    assert!(rig.app.state.agent_restarts.is_empty());
}
