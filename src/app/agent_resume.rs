use std::time::Instant;

use bytes::Bytes;
use ratatui::{
    layout::Rect,
    widgets::{Block, Borders},
};

use super::App;

struct PendingAgentResumeCandidate {
    pane_id: crate::layout::PaneId,
    terminal_id: crate::terminal::TerminalId,
    cwd: std::path::PathBuf,
    plan: crate::agent_resume::AgentResumePlan,
    rows: u16,
    cols: u16,
}

impl App {
    /// Whether a deferred plan is eligible for automatic launch; failed plans need a user retry.
    pub(crate) fn has_pending_agent_resumes(&self) -> bool {
        self.state
            .terminals
            .values()
            .any(|terminal| terminal.pending_agent_resume_plan.is_some() && !terminal.resume_failed)
    }

    pub(crate) fn sync_pending_agent_resume_deadline(&mut self, now: Instant) {
        if !self.has_pending_agent_resumes() {
            self.pending_agent_resume_deadline = None;
            return;
        }
        if self.pending_agent_resume_candidates().is_empty() {
            self.pending_agent_resume_deadline = None;
            return;
        }
        self.pending_agent_resume_deadline
            .get_or_insert(now + super::PENDING_AGENT_RESUME_THEME_WAIT);
    }

    pub(crate) fn pending_agent_resume_due(&self, now: Instant) -> bool {
        self.pending_agent_resume_deadline
            .is_some_and(|deadline| now >= deadline)
    }

    pub(crate) fn start_pending_agent_resumes(&mut self, allow_empty_theme: bool) -> bool {
        let pending = self.pending_agent_resume_candidates();
        let mut changed = false;
        for PendingAgentResumeCandidate {
            pane_id,
            terminal_id,
            cwd,
            plan,
            rows,
            cols,
        } in pending
        {
            if self.terminal_runtimes.get(&terminal_id).is_some() {
                continue;
            }
            changed |= self.start_pending_agent_resume(
                pane_id,
                terminal_id,
                cwd,
                plan,
                rows,
                cols,
                allow_empty_theme,
            );
        }

        if changed {
            self.schedule_session_save();
        }
        if !self.has_pending_agent_resumes() || self.pending_agent_resume_candidates().is_empty() {
            self.pending_agent_resume_deadline = None;
        }
        changed
    }

    fn pending_agent_resume_candidates(&self) -> Vec<PendingAgentResumeCandidate> {
        let terminal_area = self.state.view.terminal_area;
        if terminal_area.width == 0 || terminal_area.height == 0 {
            return Vec::new();
        };

        let mut pending = Vec::new();
        for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
            for (tab_idx, tab) in ws.tabs.iter().enumerate() {
                for info in
                    self.pending_agent_resume_pane_infos(ws_idx, tab_idx, tab, terminal_area)
                {
                    let Some(pane) = tab.panes.get(&info.id) else {
                        continue;
                    };
                    if self
                        .terminal_runtimes
                        .get(&pane.attached_terminal_id)
                        .is_some()
                    {
                        continue;
                    }
                    let Some(terminal) = self.state.terminals.get(&pane.attached_terminal_id)
                    else {
                        continue;
                    };
                    // Failed deferred launches remain resumable, but are not auto-retried.
                    if terminal.resume_failed {
                        continue;
                    }
                    let Some(plan) = terminal.pending_agent_resume_plan.clone() else {
                        continue;
                    };
                    pending.push(PendingAgentResumeCandidate {
                        pane_id: info.id,
                        terminal_id: pane.attached_terminal_id.clone(),
                        cwd: terminal.cwd.clone(),
                        plan,
                        rows: info.inner_rect.height,
                        cols: info.inner_rect.width,
                    });
                }
            }
        }
        pending
    }

    fn pending_agent_resume_pane_infos(
        &self,
        ws_idx: usize,
        tab_idx: usize,
        tab: &crate::workspace::Tab,
        terminal_area: Rect,
    ) -> Vec<crate::layout::PaneInfo> {
        let mut pane_infos = derived_pending_agent_resume_pane_infos(tab, terminal_area);

        if self.state.active == Some(ws_idx)
            && self
                .state
                .workspaces
                .get(ws_idx)
                .is_some_and(|ws| tab_idx == ws.active_tab_index())
        {
            for visible_info in &self.state.view.pane_infos {
                if let Some(info) = pane_infos
                    .iter_mut()
                    .find(|info| info.id == visible_info.id)
                {
                    *info = visible_info.clone();
                } else {
                    pane_infos.push(visible_info.clone());
                }
            }
        }

        pane_infos
    }

    pub(crate) fn start_pending_agent_resume_for_terminal(
        &mut self,
        terminal_id: &crate::terminal::TerminalId,
        rows: u16,
        cols: u16,
        allow_empty_theme: bool,
    ) -> bool {
        if self.terminal_runtimes.get(terminal_id).is_some() {
            return false;
        }
        let Some((pane_id, cwd, plan)) = self.state.workspaces.iter().find_map(|ws| {
            ws.tabs.iter().find_map(|tab| {
                tab.layout.pane_ids().into_iter().find_map(|pane_id| {
                    let pane = tab.panes.get(&pane_id)?;
                    if &pane.attached_terminal_id != terminal_id {
                        return None;
                    }
                    let terminal = self.state.terminals.get(terminal_id)?;
                    Some((
                        pane_id,
                        terminal.cwd.clone(),
                        terminal.pending_agent_resume_plan.clone()?,
                    ))
                })
            })
        }) else {
            return false;
        };

        let changed = self.start_pending_agent_resume(
            pane_id,
            terminal_id.clone(),
            cwd,
            plan,
            rows,
            cols,
            allow_empty_theme,
        );
        if changed {
            self.schedule_session_save();
        }
        if !self.has_pending_agent_resumes() {
            self.pending_agent_resume_deadline = None;
        }
        changed
    }

    fn start_pending_agent_resume(
        &mut self,
        pane_id: crate::layout::PaneId,
        terminal_id: crate::terminal::TerminalId,
        cwd: std::path::PathBuf,
        plan: crate::agent_resume::AgentResumePlan,
        rows: u16,
        cols: u16,
        allow_empty_theme: bool,
    ) -> bool {
        let host_terminal_theme = self.state.host_terminal_theme;
        if host_terminal_theme.is_empty() && !allow_empty_theme {
            return false;
        }

        let Some(resume_command) = shell_command_from_argv(&plan.argv) else {
            // ADR-0028 §1/§3: retain the plan, but suppress automatic retries
            // after a launch failure, including a command that cannot launch.
            if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                terminal.resume_failed = true;
            }
            crate::logging::agent_resume_empty_argv(
                pane_id.raw(),
                &terminal_id.to_string(),
                &plan.agent,
            );
            return false;
        };

        // ADR-0028 §1/§3: retain the prior lifecycle before inserting a
        // runtime, then publish the real transition in the sequenced stream.
        // Detector updates alone cannot recover the prior Offline status.
        let before = self.find_pane(pane_id).and_then(|(ws_idx, _)| {
            self.pane_info(ws_idx, pane_id)
                .map(|pane| (ws_idx, pane.agent_status))
        });

        // #1179: the resumed agent keeps its question policy hint.
        let _questions = self
            .state
            .terminals
            .get(&terminal_id)
            .map(|terminal| crate::integration::set_pending_questions(terminal.questions));
        let runtime = match crate::terminal::TerminalRuntime::spawn(
            pane_id,
            rows,
            cols,
            cwd,
            self.state.pane_scrollback_limit_bytes,
            host_terminal_theme,
            crate::pane::PaneShellConfig::new(&self.state.default_shell, self.state.shell_mode),
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        ) {
            Ok(runtime) => runtime,
            Err(err) => {
                crate::logging::agent_resume_shell_spawn_failed(
                    pane_id.raw(),
                    &terminal_id.to_string(),
                    &plan.agent,
                    &err.to_string(),
                );
                if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                    terminal.resume_failed = true;
                }
                return false;
            }
        };

        // Re-select the account profile the session was launched under
        // (#1010). flk spawns a FRESH login shell here with no selector such
        // as CLAUDE_CONFIG_DIR, so without this the resumed session falls back
        // to the CLI's default account and is orphaned from the profile it
        // belongs to. What the terminal recorded at start or hibernate, and
        // the session record Claude's SessionStart hook wrote, are exported
        // into the pane so the resume (and any later relaunch in this pane)
        // run under the right account. See `App::resume_profile_env`.
        let mut input = String::new();
        let shell = crate::pane::pane_shell(&self.state.default_shell);
        for (key, value) in self.resume_profile_env(&terminal_id, &plan) {
            // In the pane shell's OWN syntax. `export FOO=bar` is a syntax
            // error in fish, so a fish user would get no profile set at all and
            // silently resume under the default account — precisely the failure
            // this restore exists to prevent, just moved to a different shell.
            input.push_str(&env_assignment_for_shell(&shell, &key, &value));
            input.push('\r');
        }
        input.push_str(&resume_command);
        input.push('\r');
        if let Err(err) = runtime.try_send_bytes(Bytes::from(input)) {
            crate::logging::agent_resume_send_command_failed(
                pane_id.raw(),
                &terminal_id.to_string(),
                &plan.agent,
                &err.to_string(),
            );
            runtime.shutdown();
            if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                terminal.resume_failed = true;
            }
            return false;
        }

        self.terminal_runtimes.insert(terminal_id.clone(), runtime);
        if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
            terminal.resume_failed = false;
            terminal.pending_agent_resume_plan = None;
            terminal.respawn_shell_on_exit = false;
        }
        if let Some((ws_idx, previous_status)) = before {
            if let Some(pane) = self.pane_info(ws_idx, pane_id) {
                if previous_status != pane.agent_status {
                    self.emit_event(crate::api::schema::EventEnvelope {
                        event: crate::api::schema::EventKind::PaneAgentStatusChanged,
                        data: crate::api::schema::EventData::PaneAgentStatusChanged {
                            pane_id: pane.pane_id,
                            workspace_id: pane.workspace_id,
                            agent_status: pane.agent_status,
                            agent: pane.agent,
                            title: pane.title,
                            display_agent: pane.display_agent,
                            custom_status: pane.custom_status,
                            state_labels: pane.state_labels,
                        },
                    });
                }
            }
        }
        true
    }
}

fn derived_pending_agent_resume_pane_infos(
    tab: &crate::workspace::Tab,
    terminal_area: Rect,
) -> Vec<crate::layout::PaneInfo> {
    let multi_pane = tab.layout.pane_count() > 1;
    tab.layout
        .panes(terminal_area)
        .into_iter()
        .map(|mut info| {
            let pane_inner = if multi_pane {
                Block::default().borders(Borders::ALL).inner(info.rect)
            } else {
                terminal_area
            };
            info.inner_rect = stable_terminal_inner_rect(pane_inner);
            info
        })
        .collect()
}

fn stable_terminal_inner_rect(pane_inner: Rect) -> Rect {
    if pane_inner.width <= 4 {
        return pane_inner;
    }

    Rect::new(
        pane_inner.x,
        pane_inner.y,
        pane_inner.width.saturating_sub(1),
        pane_inner.height,
    )
}

fn shell_command_from_argv(argv: &[String]) -> Option<String> {
    let mut parts = argv.iter();
    let first = shell_quote(parts.next()?);
    let mut command = first;
    for part in parts {
        command.push(' ');
        command.push_str(&shell_quote(part));
    }
    Some(command)
}

/// An environment assignment written in `shell`'s own syntax.
///
/// The value is typed into a live pane, so it has to parse in whatever shell is
/// there. POSIX shells take `export NAME=value`; fish takes `set -gx NAME
/// value` and rejects the former outright.
fn env_assignment_for_shell(shell: &str, name: &str, value: &str) -> String {
    if shell_is_fish(shell) {
        format!("set -gx {name} {}", fish_quote(value))
    } else {
        format!("export {name}={}", shell_quote(value))
    }
}

fn shell_is_fish(shell: &str) -> bool {
    std::path::Path::new(shell.trim())
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "fish")
}

/// Fish quoting is NOT POSIX quoting. Inside single quotes fish treats only
/// `\\` and `'` as special, and it has no `'\''` idiom — writing the POSIX
/// escape would end the string and leave stray characters on the command line.
fn fish_quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    if value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'_' | b'-' | b'.' | b'/' | b':' | b'@' | b'%' | b'+' | b'='
            )
    }) {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {

    /// `export FOO=bar` is a syntax error in fish. Emitting it would leave the
    /// resumed session with no profile set at all — silently the default
    /// account, which is the exact bug this restore exists to prevent.
    #[test]
    fn fish_gets_set_gx_and_posix_shells_get_export() {
        assert_eq!(
            env_assignment_for_shell("/opt/homebrew/bin/fish", "CLAUDE_CONFIG_DIR", "/p/work"),
            "set -gx CLAUDE_CONFIG_DIR '/p/work'"
        );
        // Bare names and a store path rather than FHS literals: only the
        // basename is read, and the hermetic gate rightly objects to test data
        // that looks like it depends on a distribution's layout.
        for shell in ["zsh", "bash", "sh", "/nix/store/abc-bash-5.2/bin/bash", ""] {
            assert_eq!(
                env_assignment_for_shell(shell, "CLAUDE_CONFIG_DIR", "/p/work"),
                "export CLAUDE_CONFIG_DIR=/p/work",
                "{shell} should use POSIX syntax"
            );
        }
    }

    /// Fish quoting is not POSIX quoting: the POSIX `'\''` idiom ends the
    /// string in fish and leaves stray characters on the command line, so a
    /// profile path containing a quote would corrupt the typed input.
    #[test]
    fn fish_quoting_escapes_rather_than_reopening_the_string() {
        let quoted = fish_quote("/p/it's/work");
        assert_eq!(quoted, r"'/p/it\'s/work'");
        assert!(
            !quoted.contains("'\\''"),
            "POSIX escape leaked into fish output: {quoted}"
        );
        assert_eq!(fish_quote(r"/p/back\slash"), r"'/p/back\\slash'");
    }

    /// Only the shell's basename decides, so a versioned or wrapped path still
    /// resolves correctly, and a shell merely NAMED like fish does not.
    #[test]
    fn fish_is_detected_by_basename_only() {
        assert!(shell_is_fish("fish"));
        assert!(shell_is_fish("  /nix/store/abc-fish-3.7.1/bin/fish  "));
        assert!(!shell_is_fish("fishy"));
        assert!(!shell_is_fish("/nix/store/abc-selfish-1.0/bin/selfish"));
        assert!(!shell_is_fish(""));
    }
    use super::*;

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

    #[tokio::test]
    async fn resume_emits_one_live_status_and_orders_explicit_hibernation_events() {
        use crate::api::schema::{AgentStatus, EventData};
        for (explicit, hibernated) in [(false, false), (true, false), (true, true)] {
            let mut app = test_app();
            let log =
                crate::test_support::unique_temp_path("1650-resume-event").with_extension("jsonl");
            app.event_hub = crate::api::EventHub::with_persistence(log.clone());
            app.state.default_shell = "/bin/sh".into();
            app.state.shell_mode = crate::config::ShellModeConfig::NonLogin;
            app.state
                .workspaces
                .push(crate::workspace::Workspace::test_new("resume-event"));
            app.state.active = Some(0);
            app.state.ensure_test_terminals();
            app.state.view.terminal_area = Rect::new(0, 0, 80, 24);
            let pane = app.state.workspaces[0].focused_pane_id().unwrap();
            let id = app.state.workspaces[0].terminal_id(pane).unwrap().clone();
            let public_pane = app.public_pane_id(0, pane).unwrap();
            let public_workspace = app.public_workspace_id(0);
            let terminal = app.state.terminals.get_mut(&id).unwrap();
            terminal.set_detected_state(
                Some(crate::detect::Agent::OpenCode),
                crate::detect::AgentState::Idle,
            );
            let plan = crate::agent_resume::AgentResumePlan {
                agent: "opencode".into(),
                argv: vec!["/bin/sh".into(), "-c".into(), "read -r fixture".into()],
                dedupe_key: "resume-event-fixture".into(),
            };
            if hibernated {
                terminal.set_hibernated_resume_plan(Some(plan));
            } else {
                terminal.pending_agent_resume_plan = Some(plan);
            }
            assert_eq!(
                app.pane_info(0, pane).unwrap().agent_status,
                if hibernated {
                    AgentStatus::Hibernated
                } else {
                    AgentStatus::Offline
                }
            );
            let sequence = app.event_hub.current_sequence();
            // Theme deferral cannot claim that a live execution exists.
            assert!(!app.start_pending_agent_resumes(false));
            assert!(app.event_hub.ring_snapshot_after(sequence).is_empty());
            let launched = if explicit {
                app.resume_hibernated_pane(0, pane).is_ok()
            } else {
                app.start_pending_agent_resumes(true)
            };
            let live_status = app.pane_info(0, pane).unwrap().agent_status;
            let events = app.event_hub.ring_snapshot_after(sequence);
            let repeated = app.start_pending_agent_resumes(true);
            let repeated_events = app.event_hub.ring_snapshot_after(sequence);
            // Reap the owned fake shell before any assertions can panic.
            for (_, runtime) in app.terminal_runtimes.drain() {
                runtime.shutdown();
            }
            drop(app);
            let reopened = crate::api::EventHub::with_persistence(log.clone());
            let replayed = reopened.ring_snapshot_after(sequence);
            drop(reopened);
            std::fs::remove_file(log).unwrap();
            assert_eq!(
                serde_json::to_value(&events).unwrap(),
                serde_json::to_value(replayed).unwrap()
            );
            assert!(launched, "explicit={explicit}");
            assert_eq!(live_status, AgentStatus::Idle);
            let changes: Vec<_> = events
                .iter()
                .filter_map(|(_, event)| match &event.data {
                    EventData::PaneAgentStatusChanged {
                        pane_id,
                        workspace_id,
                        agent_status,
                        ..
                    } => Some((pane_id, workspace_id, *agent_status)),
                    _ => None,
                })
                .collect();
            assert_eq!(
                changes,
                vec![(&public_pane, &public_workspace, AgentStatus::Idle)],
                "explicit={explicit}: {events:?}"
            );
            if hibernated {
                assert_eq!(
                    events
                        .iter()
                        .map(|(_, event)| &event.event)
                        .collect::<Vec<_>>(),
                    vec![
                        &crate::api::schema::EventKind::PaneAgentStatusChanged,
                        &crate::api::schema::EventKind::AgentResumedFromHibernation
                    ],
                    "live status must precede the hibernation resume fact: {events:?}"
                );
            } else {
                assert_eq!(
                    events
                        .iter()
                        .map(|(_, event)| event.event)
                        .collect::<Vec<_>>(),
                    vec![crate::api::schema::EventKind::PaneAgentStatusChanged],
                    "a deferred resume must emit only live status: {events:?}"
                );
            }
            assert!(!repeated);
            assert_eq!(events.len(), repeated_events.len());
        }
    }

    #[tokio::test]
    async fn pending_agent_resume_waits_for_host_theme_before_launch() {
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        let pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.view.pane_infos = pane_infos;
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist");
        terminal.pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf '%s' 'restored agent: shell quoted | marker'; sleep 5".into(),
            ],
            dedupe_key: "flock:codex\0codex\0Id\0codex-session".into(),
        });

        assert!(!app.start_pending_agent_resumes(false));
        assert!(app.terminal_runtimes.get(&terminal_id).is_none());
        assert!(!app.state.terminals[&terminal_id].resume_failed);
        assert!(app.has_pending_agent_resumes());

        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
        };

        assert!(app.start_pending_agent_resumes(false));
        assert!(app.terminal_runtimes.get(&terminal_id).is_some());
        let terminal = app
            .state
            .terminals
            .get(&terminal_id)
            .expect("terminal should survive launch");
        assert!(terminal.pending_agent_resume_plan.is_none());
        assert!(!terminal.respawn_shell_on_exit);

        let runtime = app
            .terminal_runtimes
            .get(&terminal_id)
            .expect("pending resume should leave a shell runtime");
        let marker = "restored agent: shell quoted | marker";
        for _ in 0..20 {
            if runtime
                .snapshot_history()
                .is_some_and(|text| text.contains(marker))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            runtime
                .snapshot_history()
                .expect("runtime should expose terminal history")
                .contains(marker),
            "deferred restore should inject the resume argv into the restored shell"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[tokio::test]
    async fn empty_resume_argv_fails_once_and_stops_automatic_retries() {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("empty-resume")];
        app.state.ensure_test_terminals();
        app.state.view.terminal_area = Rect::new(0, 0, 80, 24);
        let terminal_id = app.state.workspaces[0]
            .terminal_id(app.state.workspaces[0].tabs[0].root_pane)
            .unwrap()
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: Vec::new(),
            dedupe_key: "empty-resume".into(),
        });
        let now = Instant::now();
        app.sync_pending_agent_resume_deadline(now);
        assert!(app.pending_agent_resume_deadline.is_some());
        let logs = crate::logging::capture_logs(|| {
            assert!(!app.start_pending_agent_resumes(true));
        });
        assert!(logs.contains("reason=\"empty_argv\""), "{logs}");
        let logs = crate::logging::capture_logs(|| {
            assert!(!app.start_pending_agent_resumes(true));
        });
        assert!(
            !logs.contains("empty_argv"),
            "an invalid deferred plan must not be retried on the next tick: {logs}"
        );
        assert!(app.state.terminals[&terminal_id].resume_failed);
        assert!(
            app.state.terminals[&terminal_id]
                .pending_agent_resume_plan
                .is_some(),
            "keep the saved plan for explicit recovery"
        );
        assert!(!app.has_pending_agent_resumes());
        app.sync_pending_agent_resume_deadline(now);
        assert!(app.pending_agent_resume_deadline.is_none());
        assert_eq!(app.terminal_runtimes.len(), 0);
    }

    #[tokio::test]
    async fn failed_deferred_resume_cold_restore_allows_one_automatic_attempt() {
        // #661: failed resume must retain the identity and retry plan for pending mail.
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        app.state.view.pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
        };
        {
            let terminal = app
                .state
                .terminals
                .get_mut(&terminal_id)
                .expect("test terminal should exist");
            terminal.set_agent_name("reviewer".into());
            terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
                source: "flock:codex".into(),
                agent: "codex".into(),
                session_ref: crate::agent_resume::AgentSessionRef::id("codex-session").unwrap(),
            });
            terminal.pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
                agent: "codex".into(),
                argv: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
                dedupe_key: "flock:codex\0codex\0Id\0codex-session".into(),
            });
        }
        assert!(
            app.agent_info_for_target("reviewer").is_ok(),
            "the name resolves before the resume is attempted"
        );

        // A shell that does not exist, so `TerminalRuntime::spawn` fails and
        // the failure arm runs. Derived from `temp_dir` rather than a literal
        // path, and never created.
        app.state.default_shell = std::env::temp_dir()
            .join(format!("flock-no-such-shell-{}", std::process::id()))
            .to_string_lossy()
            .into_owned();

        let sequence = app.event_hub.current_sequence();
        assert!(!app.start_pending_agent_resumes(false));
        assert!(
            app.event_hub
                .ring_snapshot_after(sequence)
                .iter()
                .all(|(_, event)| !matches!(
                    event.data,
                    crate::api::schema::EventData::PaneAgentStatusChanged { .. }
                )),
            "a failed spawn cannot announce a live status"
        );

        assert!(
            app.state.workspaces[0].terminal_id(pane_id).is_some(),
            "the pane is still there — only the agent identity was cleared"
        );
        let terminal = app
            .state
            .terminals
            .get(&terminal_id)
            .expect("terminal should survive a failed resume");
        assert_eq!(terminal.agent_name.as_deref(), Some("reviewer"));
        assert!(terminal.resume_failed);
        assert!(terminal.pending_agent_resume_plan.is_some());
        assert!(terminal.hibernated_resume_plan.is_none());
        assert!(terminal.is_agent_terminal());
        let info = app.agent_info_for_target("reviewer").unwrap();
        assert_eq!(info.agent_status, crate::api::schema::AgentStatus::Offline);
        assert_eq!(info.blocked_reason.as_deref(), Some("resume_failed"));
        let agent_id = terminal.agent_id.clone();
        let session = terminal.persisted_agent_session.clone();
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
        // Round-trip the actual persisted representation, with no live runtime transfer.
        let snapshot = serde_json::from_slice(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
        let mut cold = test_app();
        cold.state.default_shell = app.state.default_shell.clone();
        cold.state.shell_mode = crate::config::ShellModeConfig::NonLogin;
        let (workspaces, terminals, runtimes) = crate::persist::restore(
            &snapshot,
            None,
            24,
            80,
            0,
            &cold.state.default_shell,
            cold.state.shell_mode,
            true,
            cold.event_tx.clone(),
            cold.render_notify.clone(),
            cold.render_dirty.clone(),
        );
        assert!(
            runtimes.is_empty(),
            "cold restore must defer the native session"
        );
        cold.state.workspaces = workspaces;
        cold.state.terminals = terminals;
        cold.terminal_runtimes = runtimes.into();
        cold.state.view.terminal_area = Rect::new(0, 0, 80, 24);
        let terminal = cold.state.terminals.values().next().unwrap();
        assert_eq!(terminal.agent_id, agent_id);
        assert_eq!(terminal.persisted_agent_session, session);
        assert!(!terminal.resume_failed);
        assert!(terminal.pending_agent_resume_plan.is_some());
        assert!(terminal.hibernated_resume_plan.is_none());
        assert!(cold.has_pending_agent_resumes());

        // The same missing shell makes the one fresh automatic attempt observable
        // without ever launching a provider CLI or reading a developer's profile.
        let logs = crate::logging::capture_logs(|| {
            assert!(!cold.start_pending_agent_resumes(true));
        });
        assert!(logs.contains("event=\"agent_resume.spawn\""), "{logs}");
        assert!(cold.state.terminals.values().next().unwrap().resume_failed);
        assert!(!cold.has_pending_agent_resumes());
        let logs = crate::logging::capture_logs(|| {
            assert!(!cold.start_pending_agent_resumes(true));
        });
        assert!(
            !logs.contains("agent_resume.spawn"),
            "must not retry: {logs}"
        );
        cold.sync_pending_agent_resume_deadline(Instant::now());
        assert!(cold.pending_agent_resume_deadline.is_none());
    }

    #[tokio::test]
    async fn pending_agent_resume_can_launch_after_theme_wait_expires() {
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        app.state.view.pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
            dedupe_key: "flock:codex\0codex\0Id\0codex-session".into(),
        });

        app.sync_pending_agent_resume_deadline(std::time::Instant::now());
        assert!(!app.start_pending_agent_resumes(false));
        assert!(app.start_pending_agent_resumes(true));
        assert!(app.terminal_runtimes.get(&terminal_id).is_some());

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[tokio::test]
    async fn pending_agent_resume_launches_hidden_panes_with_current_terminal_area() {
        let mut app = test_app();
        let active_workspace = crate::workspace::Workspace::test_new("active");
        let active_pane = active_workspace.tabs[0].root_pane;
        let active_terminal = active_workspace.terminal_id(active_pane).cloned().unwrap();
        let hidden_workspace = crate::workspace::Workspace::test_new("hidden");
        let hidden_pane = hidden_workspace.tabs[0].root_pane;
        let hidden_terminal = hidden_workspace.terminal_id(hidden_pane).cloned().unwrap();
        app.state.view.pane_infos = active_workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![active_workspace, hidden_workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
        };
        for terminal_id in [&active_terminal, &hidden_terminal] {
            app.state
                .terminals
                .get_mut(terminal_id)
                .expect("test terminal should exist")
                .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
                agent: "codex".into(),
                argv: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
                dedupe_key: format!("flock:codex\0codex\0Id\0{terminal_id}"),
            });
        }
        app.pending_agent_resume_deadline =
            Some(std::time::Instant::now() - std::time::Duration::from_millis(1));

        assert!(app.start_pending_agent_resumes(false));
        assert!(app.terminal_runtimes.get(&active_terminal).is_some());
        assert!(app.terminal_runtimes.get(&hidden_terminal).is_some());
        assert!(
            app.pending_agent_resume_deadline.is_none(),
            "launched pending resumes should clear the wakeup deadline"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[tokio::test]
    async fn pending_agent_resume_launches_inactive_tab_panes_with_current_terminal_area() {
        let mut app = test_app();
        let mut workspace = crate::workspace::Workspace::test_new("tabs");
        let active_pane = workspace.tabs[0].root_pane;
        let inactive_tab = workspace.test_add_tab(Some("agents"));
        let inactive_pane = workspace.tabs[inactive_tab].root_pane;
        let inactive_terminal = workspace.tabs[inactive_tab]
            .terminal_id(inactive_pane)
            .cloned()
            .unwrap();
        app.state.view.pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        assert!(app
            .state
            .workspaces
            .first()
            .and_then(|ws| ws.tabs[0].terminal_id(active_pane))
            .is_some());
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
        };
        app.state
            .terminals
            .get_mut(&inactive_terminal)
            .expect("inactive tab terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
            dedupe_key: "flock:codex\0codex\0Id\0inactive-tab-session".into(),
        });

        assert!(app.start_pending_agent_resumes(false));
        assert!(app.terminal_runtimes.get(&inactive_terminal).is_some());
        assert!(
            app.state
                .terminals
                .get(&inactive_terminal)
                .expect("inactive tab terminal should still exist")
                .pending_agent_resume_plan
                .is_none(),
            "inactive tab restored panes should not wait for tab focus"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[tokio::test]
    async fn pending_agent_resume_launches_zoom_hidden_active_tab_panes() {
        let mut app = test_app();
        let mut workspace = crate::workspace::Workspace::test_new("zoomed");
        let hidden_pane = workspace.tabs[0].root_pane;
        let visible_pane = workspace.test_split(ratatui::layout::Direction::Horizontal);
        workspace.tabs[0].zoomed = true;
        let hidden_terminal = workspace.terminal_id(hidden_pane).cloned().unwrap();
        app.state.view.pane_infos = vec![crate::layout::PaneInfo {
            id: visible_pane,
            rect: ratatui::layout::Rect::new(0, 0, 100, 30),
            inner_rect: ratatui::layout::Rect::new(1, 1, 98, 28),
            scrollbar_rect: None,
            header_rect: None,
            is_focused: true,
        }];
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
        };
        app.state
            .terminals
            .get_mut(&hidden_terminal)
            .expect("hidden zoom pane terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
            dedupe_key: "flock:codex\0codex\0Id\0zoom-hidden-session".into(),
        });

        assert!(app.start_pending_agent_resumes(false));
        assert!(app.terminal_runtimes.get(&hidden_terminal).is_some());
        assert!(
            app.state
                .terminals
                .get(&hidden_terminal)
                .expect("hidden zoom pane terminal should still exist")
                .pending_agent_resume_plan
                .is_none(),
            "zoom-hidden restored panes should not wait for pane focus"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[tokio::test]
    async fn pending_agent_resume_uses_current_terminal_area_for_background_panes() {
        let mut app = test_app();
        let previous_workspace = crate::workspace::Workspace::test_new("previous");
        let previous_pane = previous_workspace.tabs[0].root_pane;
        let previous_terminal = previous_workspace
            .terminal_id(previous_pane)
            .cloned()
            .unwrap();
        let current_workspace = crate::workspace::Workspace::test_new("current");
        app.state.view.pane_infos = previous_workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 80, 24);
        app.state.workspaces = vec![previous_workspace, current_workspace];
        app.state.active = Some(1);
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
        };
        app.state
            .terminals
            .get_mut(&previous_terminal)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
            dedupe_key: "flock:codex\0codex\0Id\0codex-session".into(),
        });

        app.sync_pending_agent_resume_deadline(std::time::Instant::now());
        assert!(app.pending_agent_resume_deadline.is_some());
        assert!(app.start_pending_agent_resumes(false));
        assert!(app.terminal_runtimes.get(&previous_terminal).is_some());
        assert!(
            app.state
                .terminals
                .get(&previous_terminal)
                .expect("previous terminal should still exist")
                .pending_agent_resume_plan
                .is_none(),
            "background restored panes should not wait for focus once terminal area is known"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[tokio::test]
    async fn pending_agent_resume_launches_with_inner_rect_size() {
        let mut app = test_app();
        let mut workspace = crate::workspace::Workspace::test_new("split");
        let pane_id = workspace.test_split(ratatui::layout::Direction::Horizontal);
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        app.state.view.pane_infos = vec![crate::layout::PaneInfo {
            id: pane_id,
            rect: ratatui::layout::Rect::new(0, 0, 100, 30),
            inner_rect: ratatui::layout::Rect::new(1, 1, 98, 28),
            scrollbar_rect: None,
            header_rect: None,
            is_focused: true,
        }];
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
        };
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
            dedupe_key: "flock:codex\0codex\0Id\0codex-session".into(),
        });

        assert!(app.start_pending_agent_resumes(false));
        assert_eq!(
            app.terminal_runtimes
                .get(&terminal_id)
                .expect("pending resume should launch")
                .current_size(),
            (28, 98)
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[test]
    fn shell_command_from_argv_quotes_resume_arguments() {
        let argv = vec![
            "claude".to_string(),
            "--resume".to_string(),
            "session with ' quote".to_string(),
        ];

        assert_eq!(
            shell_command_from_argv(&argv).as_deref(),
            Some("claude --resume 'session with '\\'' quote'")
        );
        assert_eq!(shell_command_from_argv(&[]), None);
    }
}
