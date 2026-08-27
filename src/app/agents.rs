use std::path::PathBuf;

use super::{terminal_targets::TerminalTargetError, App, Mode};
use crate::api::schema::{AgentStartParams, SplitDirection};

impl App {
    pub(super) fn collect_agent_infos(&self) -> Vec<crate::api::schema::AgentInfo> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(ws_idx, ws)| {
                ws.tabs.iter().flat_map(move |tab| {
                    tab.layout
                        .pane_ids()
                        .into_iter()
                        .filter_map(move |pane_id| self.agent_info(ws_idx, pane_id))
                })
            })
            .collect()
    }

    pub(super) fn agent_info_for_target(
        &self,
        target: &str,
    ) -> Result<crate::api::schema::AgentInfo, TerminalTargetError> {
        let resolved = self.resolve_terminal_target(target)?;
        self.agent_info(resolved.ws_idx, resolved.pane_id)
            .ok_or_else(|| TerminalTargetError::NotFound {
                target: target.to_string(),
            })
    }

    pub(super) fn focus_agent_target(
        &mut self,
        target: &str,
    ) -> Result<crate::api::schema::AgentInfo, TerminalTargetError> {
        let resolved = self.resolve_terminal_target(target)?;
        self.state
            .focus_pane_in_workspace(resolved.ws_idx, resolved.pane_id);
        self.state.mode = Mode::Terminal;
        // #175 C3: focusing a hibernated pane wakes it up — best-effort so a
        // pane that isn't ready to spawn (e.g. no terminal area yet) just
        // stays hibernated until the next focus.
        if self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|ws| ws.pane_state(resolved.pane_id))
            .and_then(|pane| self.state.terminals.get(&pane.attached_terminal_id))
            .is_some_and(|terminal| terminal.hibernated_resume_plan.is_some())
        {
            let _ = self.resume_hibernated_pane(resolved.ws_idx, resolved.pane_id);
        }
        self.agent_info(resolved.ws_idx, resolved.pane_id)
            .ok_or_else(|| TerminalTargetError::NotFound {
                target: target.to_string(),
            })
    }

    pub(super) fn rename_agent_target(
        &mut self,
        target: &str,
        name: Option<String>,
    ) -> Result<crate::api::schema::AgentInfo, AgentRenameError> {
        let resolved = self
            .resolve_terminal_target(target)
            .map_err(AgentRenameError::Target)?;
        let normalized_name = name.and_then(|name| {
            let trimmed = name.trim().to_string();
            (!trimmed.is_empty()).then_some(trimmed)
        });

        if let Some(name) = normalized_name.as_deref() {
            let conflicts = self.agent_name_conflicts(name, &resolved.terminal_id);
            if !conflicts.is_empty() {
                return Err(AgentRenameError::DuplicateName {
                    name: name.to_string(),
                    candidates: conflicts,
                });
            }
        }

        let Some(terminal) = self
            .state
            .terminals
            .values_mut()
            .find(|terminal| terminal.id.to_string() == resolved.terminal_id)
        else {
            return Err(AgentRenameError::Target(TerminalTargetError::NotFound {
                target: target.to_string(),
            }));
        };
        match normalized_name {
            Some(name) => {
                terminal.set_agent_name(name.clone());
                terminal.set_manual_label(name);
            }
            None => terminal.clear_agent_name(),
        }
        self.state.mark_session_dirty();
        self.agent_info(resolved.ws_idx, resolved.pane_id)
            .ok_or_else(|| {
                AgentRenameError::Target(TerminalTargetError::NotFound {
                    target: target.to_string(),
                })
            })
    }

    pub(super) fn start_agent(
        &mut self,
        params: AgentStartParams,
    ) -> Result<(crate::api::schema::AgentInfo, Vec<String>), AgentStartError> {
        let name = params.name.trim().to_string();
        if name.is_empty() {
            return Err(AgentStartError::InvalidName);
        }
        if params.argv.is_empty() {
            return Err(AgentStartError::EmptyArgv);
        }
        let conflicts = self.agent_name_conflicts(&name, "");
        if !conflicts.is_empty() {
            return Err(AgentStartError::DuplicateName {
                name,
                candidates: conflicts,
            });
        }

        let cwd = params
            .cwd
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/"));
        let argv = params.argv;
        let focus = params.focus;
        let (rows, cols) = self.state.estimate_pane_size();

        // #359: `flk agent start` execs argv directly, so the profile selector
        // the operator's shell exported never reaches the child unless it is
        // carried here. Resolved from the API peer — the `flk` process the
        // operator ran, or the agent that asked — and refused rather than
        // guessed. Held across every placement branch below, because all of
        // them end in an argv exec.
        let spawn_env = self
            .resolve_spawn_env(&argv, self.current_api_peer_pid, None)
            .map_err(AgentStartError::ProfileUnresolved)?;
        let _spawn_env_guard = crate::integration::set_pending_spawn_env(spawn_env);

        let (ws_idx, tab_idx, pane_id) = if let Some(tab_id) = params.tab_id {
            let (ws_idx, tab_idx) =
                self.parse_tab_id(&tab_id)
                    .ok_or_else(|| AgentStartError::TargetNotFound {
                        target: tab_id.clone(),
                    })?;
            if let Some(workspace_id) = params.workspace_id.as_deref() {
                let requested_ws_idx = self.parse_workspace_id(workspace_id).ok_or_else(|| {
                    AgentStartError::TargetNotFound {
                        target: workspace_id.to_string(),
                    }
                })?;
                if requested_ws_idx != ws_idx {
                    return Err(AgentStartError::PlacementConflict);
                }
            }
            let target_pane = self.state.workspaces[ws_idx].tabs[tab_idx].layout.focused();
            self.spawn_agent_split(
                ws_idx,
                target_pane,
                params.split.unwrap_or(SplitDirection::Right),
                cwd,
                &argv,
                focus,
            )?
        } else if let Some(workspace_id) = params.workspace_id {
            let ws_idx = self.parse_workspace_id(&workspace_id).ok_or_else(|| {
                AgentStartError::TargetNotFound {
                    target: workspace_id.clone(),
                }
            })?;
            let tab_idx = self.state.workspaces[ws_idx].active_tab;
            let target_pane = self.state.workspaces[ws_idx].tabs[tab_idx].layout.focused();
            self.spawn_agent_split(
                ws_idx,
                target_pane,
                params.split.unwrap_or(SplitDirection::Right),
                cwd,
                &argv,
                focus,
            )?
        } else if let Some(split) = params.split {
            // No placement target, but the caller ASKED to split: honour it
            // against whatever is active. `--split` is the only way to say
            // "put this beside what I am looking at" without naming a target.
            let ws_idx = self.state.active.unwrap_or(0);
            if self.state.workspaces.is_empty() {
                self.spawn_agent_workspace(cwd, rows, cols, &argv, focus)?
            } else {
                let tab_idx = self.state.workspaces[ws_idx].active_tab;
                let target_pane = self.state.workspaces[ws_idx].tabs[tab_idx].layout.focused();
                self.spawn_agent_split(ws_idx, target_pane, split, cwd, &argv, focus)?
            }
        } else {
            // An agent asked for with no placement at all gets its OWN space,
            // as the tab's only pane.
            //
            // This used to split the ACTIVE workspace, which put the new agent
            // beside whatever the operator happened to be looking at — so
            // `flk agent start` from inside a pane landed the agent in that
            // pane's tab, and the space it should have had never existed. The
            // agent then shares a tab with an unrelated shell, both compete for
            // the same width, and closing "the space" closes two things.
            //
            // Splitting is still reachable, but only by asking: name a
            // `workspace_id`/`tab_id`, or pass `split`.
            self.spawn_agent_workspace(cwd, rows, cols, &argv, focus)?
        };

        let terminal_id = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.terminal_id(pane_id))
            .cloned()
            .ok_or_else(|| AgentStartError::SpawnFailed("terminal disappeared".into()))?;
        let Some(terminal) = self.state.terminals.get_mut(&terminal_id) else {
            return Err(AgentStartError::SpawnFailed("terminal disappeared".into()));
        };
        terminal.set_agent_name(name.clone());
        terminal.set_manual_label(name);
        self.state.mark_session_dirty();

        let agent = self
            .agent_info(ws_idx, pane_id)
            .ok_or_else(|| AgentStartError::SpawnFailed("agent disappeared".into()))?;
        debug_assert_eq!(agent.tab_id, self.public_tab_id(ws_idx, tab_idx).unwrap());
        Ok((agent, argv))
    }

    pub(super) fn agent_start_error_body(
        &self,
        err: AgentStartError,
    ) -> crate::api::schema::ErrorBody {
        match err {
            AgentStartError::InvalidName => crate::api::schema::ErrorBody {
                code: "invalid_agent_name".into(),
                message: "agent name must not be empty".into(),
            },
            AgentStartError::EmptyArgv => crate::api::schema::ErrorBody {
                code: "invalid_agent_argv".into(),
                message: "agent start argv must not be empty".into(),
            },
            AgentStartError::TargetNotFound { target } => crate::api::schema::ErrorBody {
                code: "agent_placement_not_found".into(),
                message: format!("agent placement target {target} not found"),
            },
            AgentStartError::PlacementConflict => crate::api::schema::ErrorBody {
                code: "agent_placement_conflict".into(),
                message: "--tab must belong to --workspace".into(),
            },
            AgentStartError::SpawnFailed(message) => crate::api::schema::ErrorBody {
                code: "agent_start_failed".into(),
                message,
            },
            AgentStartError::ProfileUnresolved(unresolved) => crate::api::schema::ErrorBody {
                code: unresolved.code().into(),
                message: unresolved.message(),
            },
            AgentStartError::DuplicateName { name, candidates } => crate::api::schema::ErrorBody {
                code: "agent_name_taken".into(),
                message: format!(
                    "agent name {name} is already used; candidates: {}",
                    candidates
                        .into_iter()
                        .map(|candidate| format!(
                            "terminal_id={} pane_id={} workspace_id={} tab_id={} cwd={} status={:?}",
                            candidate.terminal_id,
                            candidate.pane_id,
                            candidate.workspace_id,
                            candidate.tab_id,
                            candidate.cwd.unwrap_or_else(|| "unknown".into()),
                            candidate.agent_status,
                        ))
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
            },
        }
    }

    pub(super) fn agent_target_error_body(
        &self,
        err: TerminalTargetError,
    ) -> crate::api::schema::ErrorBody {
        match err {
            TerminalTargetError::NotFound { target } => crate::api::schema::ErrorBody {
                code: "agent_not_found".into(),
                message: format!("agent target {target} not found"),
            },
            TerminalTargetError::Ambiguous { target, candidates } => {
                crate::api::schema::ErrorBody {
                    code: "agent_target_ambiguous".into(),
                    message: format!(
                        "agent target {target} is ambiguous; candidates: {}",
                        candidates
                            .into_iter()
                            .map(|candidate| format!(
                                "terminal_id={} pane_id={} workspace_id={} tab_id={} cwd={} status={:?}",
                                candidate.terminal_id,
                                candidate.pane_id,
                                candidate.workspace_id,
                                candidate.tab_id,
                                candidate.cwd.unwrap_or_else(|| "unknown".into()),
                                candidate.agent_status,
                            ))
                            .collect::<Vec<_>>()
                            .join("; ")
                    ),
                }
            }
        }
    }

    pub(super) fn agent_rename_error_body(
        &self,
        err: AgentRenameError,
    ) -> crate::api::schema::ErrorBody {
        match err {
            AgentRenameError::Target(err) => self.agent_target_error_body(err),
            AgentRenameError::DuplicateName { name, candidates } => crate::api::schema::ErrorBody {
                code: "agent_name_taken".into(),
                message: format!(
                    "agent name {name} is already used; candidates: {}",
                    candidates
                        .into_iter()
                        .map(|candidate| format!(
                            "terminal_id={} pane_id={} workspace_id={} tab_id={} cwd={} status={:?}",
                            candidate.terminal_id,
                            candidate.pane_id,
                            candidate.workspace_id,
                            candidate.tab_id,
                            candidate.cwd.unwrap_or_else(|| "unknown".into()),
                            candidate.agent_status,
                        ))
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
            },
        }
    }

    /// The shell-supplied environment an argv-spawned agent must carry so it
    /// lands on the same agent profile as whoever asked for it (#359).
    ///
    /// `requester_pid` is the process that ASKED: the API peer for
    /// `agent.spawn` and `agent.start`, the forked pane's own child for either
    /// fork path. Reading it there rather than from the server is not a
    /// preference — the server's environment has no profile selector in it at
    /// all, so there is nothing at this end to allow-list.
    ///
    /// `recorded` is the profile flock already knows this child belongs on,
    /// from the `session_id -> config_dir` records Claude's `SessionStart` hook
    /// writes (`agent_resume::claude_config_dir_for_session`). It is a
    /// FALLBACK, not an override: a live requester is the current truth, since
    /// an operator can switch a pane's profile after the session started. The
    /// record is what answers for a pane with no live process — a hibernated
    /// agent is forkable and has no child pid at all.
    ///
    /// Shared by all four spawn doors so a refusal, and a successful carry,
    /// mean the same thing whichever one the caller came in through.
    pub(crate) fn resolve_spawn_env(
        &self,
        argv: &[String],
        requester_pid: Option<u32>,
        recorded: Option<String>,
    ) -> Result<Vec<(String, String)>, crate::spawn::env::ProfileUnresolved> {
        let agent = crate::spawn::env::agent_for_argv(argv);
        let attested = requester_pid
            .and_then(|pid| crate::spawn::env::read_requester_env(pid, std::process::id()))
            .or_else(|| recorded.map(crate::spawn::env::recorded_claude_profile));
        let requester_env = match (attested, requester_pid) {
            (Some(env), _) => crate::spawn::env::RequesterEnv::Attested(env),
            // A pid we could not read is the case worth refusing over: a
            // process IS there, so a selector may well be there with it.
            (None, Some(_)) => crate::spawn::env::RequesterEnv::Unreadable,
            // No requester and no record. Nothing was ever there to read.
            (None, None) => crate::spawn::env::RequesterEnv::Absent,
        };
        crate::spawn::env::resolve(agent, &requester_env, |dir| {
            std::path::Path::new(dir).is_dir()
        })
        .map_err(|unresolved| match unresolved {
            // The pure resolver has no pid to name; fill it in here so the
            // refusal points an operator at a process they can inspect.
            crate::spawn::env::ProfileUnresolved::RequesterUnreadable { .. } => {
                crate::spawn::env::ProfileUnresolved::RequesterUnreadable { pid: requester_pid }
            }
            other => other,
        })
    }

    pub(super) fn spawn_agent_workspace(
        &mut self,
        cwd: PathBuf,
        rows: u16,
        cols: u16,
        argv: &[String],
        focus: bool,
    ) -> Result<(usize, usize, crate::layout::PaneId), AgentStartError> {
        let (ws, terminal, runtime) = crate::workspace::Workspace::new_argv_command(
            cwd,
            rows,
            cols,
            argv,
            self.state.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        )
        .map_err(|err| AgentStartError::SpawnFailed(err.to_string()))?;
        self.terminal_runtimes.insert(terminal.id.clone(), runtime);
        self.state.terminals.insert(terminal.id.clone(), terminal);
        self.state.workspaces.push(ws);
        let ws_idx = self.state.workspaces.len() - 1;
        self.state
            .remove_alias_shadowed_by_new_pane(self.state.workspaces[ws_idx].tabs[0].root_pane);
        if focus || self.state.active.is_none() {
            self.state.switch_workspace(ws_idx);
            self.state.mode = Mode::Terminal;
        }
        self.schedule_session_save();
        let pane_id = self.state.workspaces[ws_idx].tabs[0].root_pane;
        Ok((ws_idx, 0, pane_id))
    }

    fn spawn_agent_split(
        &mut self,
        ws_idx: usize,
        target_pane: crate::layout::PaneId,
        split: SplitDirection,
        cwd: PathBuf,
        argv: &[String],
        focus: bool,
    ) -> Result<(usize, usize, crate::layout::PaneId), AgentStartError> {
        let (rows, cols) = self.state.estimate_pane_size();
        let previous_focus = self.state.current_pane_focus_target();
        let direction = match split {
            SplitDirection::Right => ratatui::layout::Direction::Horizontal,
            SplitDirection::Down => ratatui::layout::Direction::Vertical,
        };
        let result = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .and_then(|ws| {
                ws.split_pane_argv_command(
                    target_pane,
                    direction,
                    rows,
                    cols,
                    Some(cwd),
                    argv,
                    self.state.pane_scrollback_limit_bytes,
                    self.state.host_terminal_theme,
                    focus,
                )
            })
            .ok_or_else(|| AgentStartError::TargetNotFound {
                target: target_pane.raw().to_string(),
            })?
            .map_err(|err| AgentStartError::SpawnFailed(err.to_string()))?;
        self.terminal_runtimes
            .insert(result.1.terminal.id.clone(), result.1.runtime);
        self.state
            .remove_alias_shadowed_by_new_pane(result.1.pane_id);
        self.state
            .terminals
            .insert(result.1.terminal.id.clone(), result.1.terminal);
        if focus {
            self.state.switch_workspace_tab(ws_idx, result.0);
            self.state
                .record_pane_focus_change(previous_focus, ws_idx, result.1.pane_id);
            self.state.mode = Mode::Terminal;
        }
        self.schedule_session_save();
        Ok((ws_idx, result.0, result.1.pane_id))
    }

    pub(super) fn agent_info(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<crate::api::schema::AgentInfo> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let pane_state = ws.pane_state(pane_id)?;
        let terminal = self.state.terminals.get(&pane_state.attached_terminal_id)?;
        if !terminal.is_agent_terminal() {
            return None;
        }
        let pane = self.pane_info(ws_idx, pane_id)?;
        Some(crate::api::schema::AgentInfo {
            agent_id: terminal.agent_id.to_string(),
            terminal_id: pane.terminal_id,
            name: terminal.agent_name.clone(),
            agent: pane.agent,
            title: pane.title,
            display_agent: pane.display_agent,
            agent_status: pane.agent_status,
            custom_status: pane.custom_status,
            state_labels: pane.state_labels,
            agent_session: pane.agent_session,
            workspace_id: pane.workspace_id,
            tab_id: pane.tab_id,
            pane_id: pane.pane_id,
            focused: pane.focused,
            cwd: pane.cwd,
            foreground_cwd: pane.foreground_cwd,
            seen: pane.seen,
            status_age_secs: pane.status_age_secs,
            run_id: terminal.run_id.clone(),
            revision: pane.revision,
        })
    }

    fn agent_name_conflicts(
        &self,
        name: &str,
        except_terminal_id: &str,
    ) -> Vec<crate::api::schema::AgentInfo> {
        self.collect_agent_infos()
            .into_iter()
            .filter(|agent| {
                agent.name.as_deref() == Some(name) && agent.terminal_id != except_terminal_id
            })
            .collect()
    }
}

pub(super) enum AgentStartError {
    InvalidName,
    EmptyArgv,
    TargetNotFound {
        target: String,
    },
    PlacementConflict,
    SpawnFailed(String),
    /// #359: the agent profile the child would run under could not be
    /// established from the caller.
    ProfileUnresolved(crate::spawn::env::ProfileUnresolved),
    DuplicateName {
        name: String,
        candidates: Vec<crate::api::schema::AgentInfo>,
    },
}

pub(super) enum AgentRenameError {
    Target(TerminalTargetError),
    DuplicateName {
        name: String,
        candidates: Vec<crate::api::schema::AgentInfo>,
    },
}

#[cfg(test)]
mod tests {
    use crate::api::schema::{AgentStartParams, Method, Request};

    fn test_app() -> crate::app::App {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        crate::app::App::new(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        )
    }

    fn start_request(program: &str) -> Request {
        Request {
            id: "req_agent_start_profile".into(),
            method: Method::AgentStart(AgentStartParams {
                name: "worker".into(),
                cwd: Some("/".into()),
                workspace_id: None,
                tab_id: None,
                split: None,
                focus: false,
                argv: vec![program.to_string()],
            }),
        }
    }

    /// A pid that named a process and no longer does. Reaped before use, so
    /// the read attests nothing on any platform.
    #[cfg(unix)]
    #[allow(clippy::disallowed_methods)]
    fn unreadable_requester_pid() -> u32 {
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("exit 0")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn a probe child");
        let pid = child.id();
        let _ = child.wait();
        pid
    }

    /// #359: `flk agent start` execs argv with no shell in between, so the
    /// Claude profile selector a login shell exports never reaches the child.
    /// A caller flock cannot read attests nothing, and the guess that would
    /// license is the default profile — which may be a DIFFERENT authenticated
    /// account. Refuse at spawn time instead, so the failure is a typed error
    /// rather than a child parked at a login prompt in a pane nobody is
    /// watching.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_claude_start_refuses_when_the_callers_profile_cannot_be_read() {
        let mut app = test_app();
        let before = app.state.workspaces.len();
        app.current_api_peer_pid = Some(unreadable_requester_pid());
        let raw = app.handle_api_request(start_request("claude"));
        let response: serde_json::Value = serde_json::from_str(&raw).expect("json");

        assert_eq!(response["error"]["code"], "agent_profile_unresolved");
        assert_eq!(
            app.state.workspaces.len(),
            before,
            "a refused start must create nothing"
        );
    }

    /// The refusal is scoped to agents flock knows to have shell-supplied
    /// state. Anything else must start exactly as it did before — a global
    /// gate here would break every non-claude `agent start` on a host where
    /// process environments are unreadable.
    #[tokio::test]
    async fn a_non_agent_program_is_not_gated_on_a_profile() {
        let mut app = test_app();
        let raw = app.handle_api_request(start_request(&crate::test_support::no_op_program()));
        let response: serde_json::Value = serde_json::from_str(&raw).expect("json");

        assert_ne!(
            response["error"]["code"], "agent_profile_unresolved",
            "only agents with a shell-supplied selector are gated"
        );

        let runtimes: Vec<_> = app.terminal_runtimes.drain().collect();
        for (_terminal_id, runtime) in runtimes {
            runtime.shutdown();
        }
    }
}
