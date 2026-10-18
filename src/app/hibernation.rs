//! Pane hibernation seams (#175 phase 4, C3).
//!
//! Hibernation is a user-initiated (or C3-check-initiated) transition that
//! parks an agent pane: the child process is asked to exit, the resume plan
//! is stashed on the terminal, and the pane is kept. The next focus (or an
//! explicit `agent.resume`) respawns the argv into the same terminal / pane.
//!
//! Guardrails (§8 of the design):
//! - Refuse to hibernate any pane without a valid `agent_resume::plan(..)` —
//!   data loss guard: we would not know how to bring the agent back.
//! - `respawn_shell_on_exit` is disabled so the pane doesn't drop into a
//!   generic shell when the child exits.
//! - `handle_pane_died` special-cases the hibernated case in the App layer
//!   (see `runtime_exit_action`) so no `PaneClosed` fires.
//!
//! This module owns the App-level verbs (`hibernate_pane`,
//! `resume_hibernated_pane`) and the durable event emit; the API handlers
//! (`handle_agent_hibernate`, `handle_agent_resume`) live alongside the
//! other `agent.*` handlers.

use crate::layout::PaneId;

use super::App;

/// Why a hibernate/resume verb refused. Maps 1:1 to the wire-level
/// refusal codes the API handlers surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HibernationError {
    /// The pane's agent has no resumable session (no `agent_resume::plan`)
    /// — hibernating would lose the conversation forever.
    Unsupported {
        reason: String,
    },
    /// `agent.resume` on a pane that isn't hibernated.
    NotHibernated,
    /// `agent.hibernate` on a pane that's already hibernated.
    AlreadyHibernated,
    /// The target pane could not be resolved.
    NotFound,
    RestartPending,
}

impl HibernationError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Unsupported { .. } => "hibernate_unsupported",
            Self::NotHibernated => "not_hibernated",
            Self::AlreadyHibernated => "already_hibernated",
            Self::NotFound => "not_found",
            Self::RestartPending => "restart_pending",
        }
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::Unsupported { reason } => reason.clone(),
            Self::NotHibernated => "pane is not hibernated; nothing to resume".to_string(),
            Self::AlreadyHibernated => {
                "pane is already hibernated; call agent.resume to bring it back".to_string()
            }
            Self::NotFound => "no pane matches that target".to_string(),
            Self::RestartPending => "a restart is stopping this pane; wait for verification".into(),
        }
    }
}

impl App {
    /// Hibernate a pane: stash its resume plan, disable respawn-on-exit,
    /// gracefully release + shut down its runtime so the child exits. The
    /// pane stays alive on the tree — a later focus/resume respawns the
    /// argv back into it.
    pub(crate) fn hibernate_pane(
        &mut self,
        ws_idx: usize,
        pane_id: PaneId,
    ) -> Result<AgentHibernationOutcome, HibernationError> {
        let (terminal_id, session_info) = {
            let ws = self
                .state
                .workspaces
                .get(ws_idx)
                .ok_or(HibernationError::NotFound)?;
            let pane = ws.pane_state(pane_id).ok_or(HibernationError::NotFound)?;
            let terminal_id = pane.attached_terminal_id.clone();
            let terminal = self
                .state
                .terminals
                .get(&terminal_id)
                .ok_or(HibernationError::NotFound)?;
            if terminal.hibernated_resume_plan.is_some() {
                return Err(HibernationError::AlreadyHibernated);
            }
            let info = super::creation::terminal_agent_session_info(terminal).ok_or(
                HibernationError::Unsupported {
                    reason: "agent target has no resumable session — refusing to hibernate \
                             would lose the conversation forever"
                        .to_string(),
                },
            )?;
            (terminal_id, info)
        };
        let session_ref = crate::agent_resume::AgentSessionRef {
            kind: session_info.kind,
            value: session_info.value.clone(),
        };
        let plan =
            crate::agent_resume::plan(&session_info.source, &session_info.agent, &session_ref)
                .ok_or_else(|| HibernationError::Unsupported {
                    reason: format!(
                        "{agent} ({source}) has no resume integration; refusing to hibernate",
                        agent = session_info.agent,
                        source = session_info.source
                    ),
                })?;

        let known_agent = crate::detect::parse_agent_label(&session_info.agent);
        let public_pane_id = self
            .public_pane_id(ws_idx, pane_id)
            .ok_or(HibernationError::NotFound)?;
        let workspace_id = self.public_workspace_id(ws_idx);

        // Read the profile selector while the agent process still exists to
        // read it from (#1010): after the shutdown below nothing on the
        // machine says which account the session belongs to.
        self.record_agent_profile_env(&terminal_id, None);
        if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
            terminal.set_hibernated_resume_plan(Some(plan.clone()));
            terminal.respawn_shell_on_exit = false;
        }
        // Graceful release + shutdown of the runtime — the same seam
        // `agent.pane` teardown uses. When the child exits, `runtime_exit_action`
        // sees the stashed plan and holds the pane instead of closing it.
        if let Some(runtime) = self.terminal_runtimes.get(&terminal_id) {
            if let Some(agent) = known_agent {
                runtime.begin_graceful_release(agent);
            }
        }
        if let Some(runtime) = self.terminal_runtimes.remove(&terminal_id) {
            runtime.shutdown();
        }
        // Saved now rather than after the debounce (#1010): hibernating is
        // what an operator does right before stopping the server, and a stop
        // inside the debounce window would lose the only record of the plan.
        self.save_session_now();

        self.event_hub.push(crate::api::schema::EventEnvelope {
            event: crate::api::schema::EventKind::AgentHibernated,
            data: crate::api::schema::EventData::AgentHibernated {
                pane_id: public_pane_id.clone(),
                workspace_id: workspace_id.clone(),
                agent: session_info.agent.clone(),
                session: Some(session_info.value.clone()),
            },
        });
        Ok(AgentHibernationOutcome {
            public_pane_id,
            workspace_id,
            agent: session_info.agent,
        })
    }

    /// Resume a hibernated or deferred pane in the same terminal / pane.
    /// Clears the plan on success; only a hibernated plan emits the hibernation event.
    pub(crate) fn resume_hibernated_pane(
        &mut self,
        ws_idx: usize,
        pane_id: PaneId,
    ) -> Result<AgentHibernationOutcome, HibernationError> {
        let (terminal_id, plan, was_hibernated) = {
            let ws = self
                .state
                .workspaces
                .get(ws_idx)
                .ok_or(HibernationError::NotFound)?;
            let pane = ws.pane_state(pane_id).ok_or(HibernationError::NotFound)?;
            let terminal_id = pane.attached_terminal_id.clone();
            let terminal = self
                .state
                .terminals
                .get(&terminal_id)
                .ok_or(HibernationError::NotFound)?;
            if terminal.restart_in_progress {
                return Err(HibernationError::RestartPending);
            }
            // A restored agent still waiting for its deferred resume (#1010)
            // is as parked as a hibernated one: it has a plan and no process.
            // Answering `not_hibernated` there told an operator after a cold
            // start that their agent could not come back when it could.
            let deferred = terminal
                .pending_agent_resume_plan
                .clone()
                .filter(|_| self.terminal_runtimes.get(&terminal_id).is_none());
            let plan = terminal
                .hibernated_resume_plan
                .clone()
                .or(deferred)
                .ok_or(HibernationError::NotHibernated)?;
            let was_hibernated = terminal.hibernated_resume_plan.is_some();
            (terminal_id, plan, was_hibernated)
        };

        let public_pane_id = self
            .public_pane_id(ws_idx, pane_id)
            .ok_or(HibernationError::NotFound)?;
        let workspace_id = self.public_workspace_id(ws_idx);
        let agent = plan.agent.clone();

        // Move the plan through the `pending_agent_resume_plan` seam so we
        // reuse the existing spawn-into-same-pane machinery; if that path
        // refuses (e.g. no runtime area yet) the plan stays stashed for the
        // next focus.
        if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
            terminal.pending_agent_resume_plan = Some(plan.clone());
            terminal.set_hibernated_resume_plan(None);
        }
        let (rows, cols) = self.state.estimate_pane_size();
        let launched = self.start_pending_agent_resume_for_terminal(&terminal_id, rows, cols, true);
        if !launched {
            // ADR-0028 §1: a failed launch keeps its original lifecycle and
            // persisted plan slot, never turning a deferred restore into hibernation.
            if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                terminal.pending_agent_resume_plan = None;
                if was_hibernated {
                    terminal.set_hibernated_resume_plan(Some(plan));
                } else {
                    terminal.set_hibernated_resume_plan(None);
                    terminal.pending_agent_resume_plan = Some(plan);
                }
            }
            return Err(HibernationError::Unsupported {
                reason: "runtime area is not ready; try again after the pane is visible".into(),
            });
        }

        // ADR-0028 §1 and #1680: deferred and hibernated are distinct lifecycle facts.
        if was_hibernated {
            self.event_hub.push(crate::api::schema::EventEnvelope {
                event: crate::api::schema::EventKind::AgentResumedFromHibernation,
                data: crate::api::schema::EventData::AgentResumedFromHibernation {
                    pane_id: public_pane_id.clone(),
                    workspace_id: workspace_id.clone(),
                    agent: agent.clone(),
                },
            });
        }
        Ok(AgentHibernationOutcome {
            public_pane_id,
            workspace_id,
            agent,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_resume::{AgentSessionRef, PersistedAgentSession};

    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    fn app_with_agent_pane() -> (App, usize, PaneId) {
        let mut app = test_app();
        let ws = crate::workspace::Workspace::test_new("main");
        let pane_id = ws.tabs[0].root_pane;
        app.state.workspaces.push(ws);
        app.state.ensure_test_terminals();
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .cloned()
            .unwrap();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.persisted_agent_session = Some(PersistedAgentSession {
            source: "flock:claude".into(),
            agent: "claude".into(),
            session_ref: AgentSessionRef::id("test-session").unwrap(),
        });
        // Mark the terminal as an agent-owned pane so `agent_info` returns
        // Some — the resume plan derives from persisted_agent_session, but
        // is_agent_terminal reads from these fields.
        terminal.launch_argv = Some(vec!["claude".into()]);
        terminal.set_agent_name("claude".into());
        (app, 0, pane_id)
    }

    #[test]
    fn hibernate_refuses_pane_without_resume_plan() {
        let mut app = test_app();
        let ws = crate::workspace::Workspace::test_new("main");
        let pane_id = ws.tabs[0].root_pane;
        app.state.workspaces.push(ws);
        app.state.ensure_test_terminals();
        let err = app.hibernate_pane(0, pane_id).unwrap_err();
        assert!(matches!(err, HibernationError::Unsupported { .. }));
        assert_eq!(err.code(), "hibernate_unsupported");
    }

    #[test]
    fn hibernate_stashes_plan_and_disables_respawn() {
        let (mut app, ws_idx, pane_id) = app_with_agent_pane();
        let terminal_id = app.state.workspaces[ws_idx]
            .terminal_id(pane_id)
            .cloned()
            .unwrap();
        {
            let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
            terminal.respawn_shell_on_exit = true;
        }
        app.hibernate_pane(ws_idx, pane_id).unwrap();
        let terminal = app.state.terminals.get(&terminal_id).unwrap();
        assert!(terminal.hibernated_resume_plan.is_some());
        assert!(!terminal.respawn_shell_on_exit);
    }

    #[test]
    fn hibernate_refuses_double_hibernate() {
        let (mut app, ws_idx, pane_id) = app_with_agent_pane();
        app.hibernate_pane(ws_idx, pane_id).unwrap();
        let err = app.hibernate_pane(ws_idx, pane_id).unwrap_err();
        assert_eq!(err.code(), "already_hibernated");
    }

    #[tokio::test]
    async fn deferred_1680_resume_does_not_claim_hibernation() {
        let mut app = test_app();
        app.state.default_shell = "/bin/sh".into();
        app.state.shell_mode = crate::config::ShellModeConfig::NonLogin;
        let ws = crate::workspace::Workspace::test_new("deferred-resume");
        let pane_id = ws.tabs[0].root_pane;
        let terminal_id = ws.terminal_id(pane_id).cloned().unwrap();
        app.state.workspaces.push(ws);
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 80, 24);
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_detected_state(
            Some(crate::detect::Agent::OpenCode),
            crate::detect::AgentState::Idle,
        );
        terminal.pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "opencode".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), "read -r fixture".into()],
            dedupe_key: "deferred-resume-fixture".into(),
        });
        assert!(terminal.hibernated_resume_plan.is_none());
        let sequence = app.event_hub.current_sequence();
        let result = app.resume_hibernated_pane(0, pane_id);
        let events = app.event_hub.events_after(sequence);
        // Reap the owned fake shell before any assertion can panic.
        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            events
                .iter()
                .map(|(_, envelope)| envelope.event)
                .collect::<Vec<_>>(),
            vec![crate::api::schema::EventKind::PaneAgentStatusChanged],
            "a deferred resume must emit only its live status: {events:?}"
        );
    }

    #[tokio::test]
    async fn failed_1680_resume_preserves_plan_origin_through_persistence_and_retry() {
        use crate::api::schema::{AgentStatus, EventKind};
        use std::os::unix::fs::PermissionsExt;
        for (automatic, hibernated) in [(false, false), (false, true), (true, false)] {
            let root =
                crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
            let fake = root.path().join("opencode");
            std::fs::write(&fake, "#!/bin/sh\nread -r fixture\n").unwrap();
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
            let mut app = test_app();
            app.state.default_shell = root.path().join("missing-shell").display().to_string();
            app.state.shell_mode = crate::config::ShellModeConfig::NonLogin;
            let ws = crate::workspace::Workspace::test_new("failed-resume");
            let pane_id = ws.tabs[0].root_pane;
            let id = ws.terminal_id(pane_id).cloned().unwrap();
            app.state.workspaces.push(ws);
            app.state.active = Some(0);
            app.state.ensure_test_terminals();
            app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 80, 24);
            let terminal = app.state.terminals.get_mut(&id).unwrap();
            terminal.set_agent_name("opencode".into());
            terminal.set_detected_state(
                Some(crate::detect::Agent::OpenCode),
                crate::detect::AgentState::Idle,
            );
            terminal.launch_argv = Some(vec![fake.display().to_string()]);
            terminal.persisted_agent_session = Some(crate::agent_resume::PersistedAgentSession {
                source: "flock:opencode".into(),
                agent: "opencode".into(),
                session_ref: crate::agent_resume::AgentSessionRef::id("failed-resume-fixture")
                    .unwrap(),
            });
            let plan = crate::agent_resume::AgentResumePlan {
                agent: "opencode".into(),
                argv: vec![
                    fake.display().to_string(),
                    "--session".into(),
                    "failed-resume-fixture".into(),
                ],
                dedupe_key: "failed-resume-fixture".into(),
            };
            if hibernated {
                terminal.set_hibernated_resume_plan(Some(plan));
            } else {
                terminal.pending_agent_resume_plan = Some(plan);
            }
            let sequence = app.event_hub.current_sequence();
            let failed = if automatic {
                !app.start_pending_agent_resumes(true)
            } else {
                app.resume_hibernated_pane(0, pane_id).is_err()
            };
            let terminal = &app.state.terminals[&id];
            let failed_slots = (
                terminal.pending_agent_resume_plan.is_some(),
                terminal.hibernated_resume_plan.is_some(),
            );
            let failed_status = app.pane_info(0, pane_id).unwrap().agent_status;
            let failed_events = app.event_hub.events_after(sequence);
            let snapshot = crate::persist::capture(
                crate::persist::RegistryCapture {
                    registry: &app.state.structured_agents,
                    now_ms: Some(42),
                },
                &app.state.workspaces,
                &app.state.terminals,
                &app.terminal_runtimes,
                app.state.active,
                app.state.selected,
                app.state.panel_scopes(),
                app.state.sidebar_width,
                app.state.sidebar_section_split,
                app.state.collapsed_space_keys.clone(),
                Default::default(),
            );
            let path = root.path().join("session.json");
            serde_json::to_writer(
                crate::platform::structured::create_private_file(&path).unwrap(),
                &snapshot,
            )
            .unwrap();
            let saved: crate::persist::SessionSnapshot =
                serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            let saved_hibernated = saved.workspaces[0].tabs[0].panes[&pane_id.raw()]
                .hibernated
                .is_some();
            let (_, restored, mut restored_runtimes) = crate::persist::restore(
                &saved,
                None,
                24,
                80,
                app.state.pane_scrollback_limit_bytes,
                "/bin/sh",
                crate::config::ShellModeConfig::NonLogin,
                true,
                app.event_tx.clone(),
                app.render_notify.clone(),
                app.render_dirty.clone(),
            );
            let restored_terminal = restored.values().next().unwrap();
            let restored_slots = (
                restored_terminal.pending_agent_resume_plan.is_some(),
                restored_terminal.hibernated_resume_plan.is_some(),
            );
            let restore_spawned = !restored_runtimes.is_empty();
            for (_, runtime) in restored_runtimes.drain() {
                runtime.shutdown();
            }
            app.state.default_shell = "/bin/sh".into();
            let automatic_retry = app.start_pending_agent_resumes(true);
            let automatic_spawned = app.terminal_runtimes.len() != 0;
            let retried = app.resume_hibernated_pane(0, pane_id);
            let events = app.event_hub.events_after(sequence);
            for (_, runtime) in app.terminal_runtimes.drain() {
                runtime.shutdown();
            }
            let expected_slots = (!hibernated, hibernated);
            assert!(failed && retried.is_ok(), "failed={failed}; {retried:?}");
            assert!(
                !automatic_retry && !automatic_spawned,
                "failed resumes require an explicit retry"
            );
            assert!(failed_events.is_empty());
            assert_eq!(failed_status, AgentStatus::Offline);
            assert_eq!(failed_slots, expected_slots, "retry events: {events:?}");
            assert_eq!(saved_hibernated, hibernated);
            assert_eq!(restored_slots, expected_slots);
            assert!(!restore_spawned);
            let mut expected = vec![EventKind::PaneAgentStatusChanged];
            if hibernated {
                expected.push(EventKind::AgentResumedFromHibernation);
            }
            assert_eq!(
                events
                    .iter()
                    .map(|(_, event)| event.event)
                    .collect::<Vec<_>>(),
                expected
            );
            let digest =
                crate::digest::categorize(events.iter().map(|(seq, event)| (*seq, 0, event)));
            assert_eq!(
                digest
                    .green
                    .iter()
                    .filter(|row| row.kind == "agent_resumed_from_hibernation")
                    .count(),
                usize::from(hibernated)
            );
        }
    }

    #[test]
    fn resume_refuses_unhibernated_pane() {
        let (mut app, ws_idx, pane_id) = app_with_agent_pane();
        let err = app.resume_hibernated_pane(ws_idx, pane_id).unwrap_err();
        assert_eq!(err.code(), "not_hibernated");
    }

    #[test]
    fn hibernation_status_derives_hibernated_variant() {
        let (mut app, ws_idx, pane_id) = app_with_agent_pane();
        app.hibernate_pane(ws_idx, pane_id).unwrap();
        let info = app.agent_info(ws_idx, pane_id).unwrap();
        assert_eq!(
            info.agent_status,
            crate::api::schema::AgentStatus::Hibernated
        );
    }

    #[test]
    fn hibernate_emits_durable_event() {
        let (mut app, ws_idx, pane_id) = app_with_agent_pane();
        app.hibernate_pane(ws_idx, pane_id).unwrap();
        let events = app.event_hub.events_after(0);
        let has_event = events.iter().any(|(_, envelope)| {
            matches!(
                &envelope.data,
                crate::api::schema::EventData::AgentHibernated { agent, .. } if agent == "claude"
            )
        });
        assert!(has_event, "expected AgentHibernated event on the hub");
    }
}

/// Successful hibernate/resume outcome — the API handlers re-derive the
/// pane's public `AgentInfo` after the mutation lands, so the outcome
/// only needs to signal "it worked". Kept as a struct so future callers
/// can enrich the result without a signature change.
#[derive(Debug, Clone)]
pub(crate) struct AgentHibernationOutcome {
    #[allow(
        dead_code,
        reason = "surface reserved for future consumers; API handlers use agent_info() instead"
    )]
    pub public_pane_id: String,
    #[allow(
        dead_code,
        reason = "surface reserved for future consumers; API handlers use agent_info() instead"
    )]
    pub workspace_id: String,
    #[allow(
        dead_code,
        reason = "surface reserved for future consumers; API handlers use agent_info() instead"
    )]
    pub agent: String,
}
