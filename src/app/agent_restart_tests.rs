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
    wait_for_dead_runtime(&rig).await;
    while let Ok(event) = rig.app.event_rx.try_recv() {
        rig.app.handle_internal_event(event);
    }
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
    assert_eq!(reported_status(&mut rig), "hibernated");
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
    wait_for_dead_runtime(&rig).await;
    while let Ok(event) = rig.app.event_rx.try_recv() {
        rig.app.handle_internal_event(event);
    }
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
    assert_eq!(reported_status(&mut rig), "hibernated");
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
