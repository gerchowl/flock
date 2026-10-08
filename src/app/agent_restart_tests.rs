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

fn rig() -> Rig {
    rig_with_script("#!/bin/sh\nprintf '%s\\n' \"$@\" > \"launch-$$\"\nprintf '%s\\n' \"${RESTART_PROFILE:-unset}\" >> \"launch-$$\"\nwhile IFS= read -r line; do :; done\n")
}

fn rig_with_script(script: &str) -> Rig {
    let directory = std::env::temp_dir().join(format!(
        "flock-restart-{}-{}",
        std::process::id(),
        super::super::notifications::mint_notification_id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let executable = directory.join("claude");
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
    let argv = vec![
        executable.to_string_lossy().into_owned(),
        "--model".into(),
        "fixture-model".into(),
    ];
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
    report_session(&mut rig, "restart-session", 1);
    rig
}

fn report_session(rig: &mut Rig, session: &str, seq: u64) {
    rig.app.current_api_peer_pid = rig
        .app
        .terminal_runtimes
        .get(&rig.id)
        .and_then(|r| r.child_pid());
    let response = rig.app.handle_api_request(Request {
        id: "session-report".into(),
        method: Method::PaneReportAgentSession(PaneReportAgentSessionParams {
            pane_id: rig.pane.clone(),
            source: "flock:claude".into(),
            agent: "claude".into(),
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
    let mut rig = rig();
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
    let deadline = Instant::now() + Duration::from_secs(5);
    let expected = rig.directory.join(format!("launch-{}", resumed.unwrap()));
    // Resume uses a shell with a separate harness child, so find the launch
    // record by its resume argv rather than assuming shell pid == harness pid.
    let launch = loop {
        let text = std::fs::read_dir(&rig.directory)
            .unwrap()
            .flatten()
            .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
            .find(|text| text.starts_with("--resume\nrestart-session\n"));
        if let Some(text) = text {
            break text;
        }
        assert!(
            Instant::now() < deadline,
            "resumed fixture did not execute ({expected:?})"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    };
    assert!(launch.contains("--model\nfixture-model\n"));
    assert!(launch.ends_with("fixture-env\n"));
}

#[tokio::test]
async fn restart_api_refuses_fresh_session_and_reports_stuck_without_continuing() {
    let mut rig = rig();
    assert!(request(&mut rig).get("result").is_some());
    idle(&mut rig, Instant::now(), "");
    advance_until_verifying(&mut rig).await;
    report_session(&mut rig, "wrong-session", 1);
    rig.app.tick_agent_restarts(Instant::now() + TICK);
    assert!(has_phase(&rig, "restart_stuck"));
    assert!(!has_phase(&rig, "restarted"));
    assert_eq!(rig.app.mailboxes.wake_count(&rig.pane), 0);
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
async fn restart_api_rate_cap_stops_and_focus_cannot_resume() {
    let mut rig = rig();
    rig.app.state.config.session.restart.max_restarts = 0;
    rig.app.state.config.session.restart.restart_grace_secs = 0;
    assert!(request(&mut rig).get("result").is_some());
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
    assert!(rig.app.event_hub.events_after(0).iter().any(|(_, event)| matches!(&event.data, EventData::AgentRestart { phase, detail, .. } if phase == "restart_stuck" && detail.contains("Unknown startup question"))));
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
