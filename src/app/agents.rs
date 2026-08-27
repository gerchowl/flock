use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::{terminal_targets::TerminalTargetError, App, Mode};
use crate::api::schema::{AgentStartParams, SplitDirection};
use crate::terminal::{TerminalId, TerminalRuntime};

/// How long a freshly started agent gets to still be running before flock
/// reports the start as a success (#178).
///
/// `exec` itself fails synchronously: a binary missing from `PATH`, one that
/// is not executable, an unreadable interpreter all come back as a spawn
/// error and are already refused. A process that execs and THEN exits does
/// not — the API answers `agent_started`, the pane is reaped a moment later,
/// and nothing machine-readable ever names the failure, so a script driving
/// `agent.start`/`agent.fork` cannot tell a running agent from one that died
/// on its own arguments.
///
/// The window is short on purpose. What it catches is loader- and
/// argument-shaped — a CLI rejecting its own flags, a missing dylib, a
/// wrapper script that exits — and those die in single-digit milliseconds;
/// the rest is headroom for a loaded machine getting round to reaping them,
/// not a health check. Every healthy start pays it in full, which is the
/// reason not to make it generous, and a dead one pays none of it — the wait
/// ends the moment the exit lands. An agent that lives past the window and
/// exits later is a different event, and belongs to the pane that reports it.
const START_LIVENESS_WINDOW: Duration = Duration::from_millis(250);

/// How often the liveness window re-reads the child's exit slot.
const START_LIVENESS_POLL: Duration = Duration::from_millis(5);

/// How long a dead child's last words get to reach the terminal.
///
/// The PTY reader is a separate task, so at the instant `wait()` returns the
/// bytes the child printed on its way out may not be parsed yet. Bounded, and
/// a pane that stays blank is simply reported as having printed nothing.
const START_OUTPUT_SETTLE: Duration = Duration::from_millis(50);

/// How much of the pane's last line to quote back in the refusal.
const START_OUTPUT_MAX_CHARS: usize = 200;

/// How far up the dead pane to look for that line.
const START_OUTPUT_TAIL_LINES: usize = 40;

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

    /// Where an agent starts, given an explicit `--cwd` and the placement it
    /// was aimed at (#365).
    ///
    /// `agent start` used to consult `--cwd`, then `current_dir()` — the
    /// SERVER's cwd, which for a daemon launched from a login shell is
    /// `$HOME`. So `--workspace <id>` named a destination for the pane and
    /// none at all for the process: the agent landed in the home directory of
    /// a machine it had just been pointed away from. When that agent is Claude
    /// Code the first thing the operator sees is a trust prompt asking for
    /// read, edit and execute over all of `$HOME` — flock steered it there,
    /// and accepting is the natural reflex.
    ///
    /// Every other pane-creating verb already follows its target:
    /// `pane.split` and `tab.create` both read the target pane's cwd. This is
    /// that, for agents, with flock's own record of the checkout preferred
    /// over wherever a shell inside it has since wandered — an agent wants the
    /// worktree root, which is also where `worktree create` puts its own root
    /// pane.
    ///
    /// Deliberately NOT routed through `resolve_new_terminal_cwd`. The
    /// `[terminal] new_cwd` policy answers "where does a new interactive pane
    /// go when nobody said", and `--workspace` / `--tab` / `--split` IS
    /// somebody saying. Honouring a `home` policy here would rebuild this bug
    /// out of a config key.
    ///
    /// Candidates are checked for existence, so a membership whose checkout
    /// has been removed degrades to the old fallback instead of turning
    /// `agent start` into an ENOENT from the PTY layer.
    fn agent_start_cwd(
        &self,
        explicit: Option<PathBuf>,
        placement: Option<(usize, crate::layout::PaneId)>,
    ) -> PathBuf {
        explicit
            .or_else(|| {
                let (ws_idx, target_pane) = placement?;
                let ws = self.state.workspaces.get(ws_idx)?;
                let checkout = ws
                    .worktree_space
                    .as_ref()
                    .map(|space| space.checkout_path.clone());
                let pane_cwd = ws.find_tab_index_for_pane(target_pane).and_then(|tab_idx| {
                    ws.tabs.get(tab_idx)?.cwd_for_pane(
                        target_pane,
                        &self.state.terminals,
                        &self.terminal_runtimes,
                    )
                });
                checkout
                    .into_iter()
                    .chain(pane_cwd)
                    .find(|path| path.is_dir())
            })
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/"))
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

        let explicit_cwd = params.cwd.map(PathBuf::from);
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
            let cwd = self.agent_start_cwd(explicit_cwd, Some((ws_idx, target_pane)));
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
            let cwd = self.agent_start_cwd(explicit_cwd, Some((ws_idx, target_pane)));
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
                let cwd = self.agent_start_cwd(explicit_cwd, None);
                self.spawn_agent_workspace(cwd, rows, cols, &argv, focus)?
            } else {
                let tab_idx = self.state.workspaces[ws_idx].active_tab;
                let target_pane = self.state.workspaces[ws_idx].tabs[tab_idx].layout.focused();
                let cwd = self.agent_start_cwd(explicit_cwd, Some((ws_idx, target_pane)));
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
            let cwd = self.agent_start_cwd(explicit_cwd, None);
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
            AgentStartError::ExitedAtStart(exit) => crate::api::schema::ErrorBody {
                code: "agent_exited_at_start".into(),
                message: exit.message(),
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
        let terminal_id = terminal.id.clone();
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
        if let Some(exit) = self.agent_exited_at_start(&terminal_id) {
            self.discard_agent_pane_that_never_ran(pane_id);
            return Err(AgentStartError::ExitedAtStart(exit));
        }
        Ok((ws_idx, 0, pane_id))
    }

    /// Did the agent flock just started die before it could do anything
    /// (#178)? `None` means it was still running when the window closed,
    /// which is as much as a start can honestly claim.
    ///
    /// Polls the child's exit slot rather than waiting on `PaneDied`: the
    /// event is delivered through the app loop, and this runs INSIDE a
    /// handler on that loop, so nothing would ever drain it here.
    fn agent_exited_at_start(&self, terminal_id: &TerminalId) -> Option<AgentExitedAtStart> {
        let runtime = self.terminal_runtimes.get(terminal_id)?;
        // A pane with no watcher — one adopted over a live handoff — has
        // nothing to observe, so it is reported as alive rather than waited on.
        let exit = runtime.child_exit()?;
        let deadline = Instant::now() + START_LIVENESS_WINDOW;
        while !exit.is_reaped() {
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(START_LIVENESS_POLL);
        }
        Some(AgentExitedAtStart {
            exit_code: exit.exit_code(),
            last_output: last_pane_output(runtime),
        })
    }

    /// Undo a start whose agent was already gone (#178).
    ///
    /// The watcher's own `PaneDied` reaps this pane a moment later anyway,
    /// but "a moment later" is after the caller has been told the start
    /// failed — and a caller told that must not be able to list the
    /// half-built workspace in between. Reaping it here makes the refusal and
    /// the state agree.
    fn discard_agent_pane_that_never_ran(&mut self, pane_id: crate::layout::PaneId) {
        self.state
            .handle_app_event(crate::events::AppEvent::PaneDied { pane_id });
        self.shutdown_detached_terminal_runtimes();
        self.schedule_session_save();
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
        let terminal_id = result.1.terminal.id.clone();
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
        if let Some(exit) = self.agent_exited_at_start(&terminal_id) {
            self.discard_agent_pane_that_never_ran(result.1.pane_id);
            return Err(AgentStartError::ExitedAtStart(exit));
        }
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
    /// #178: the child exec'd and was already gone when flock looked. A
    /// successful spawn is not a started agent.
    ExitedAtStart(AgentExitedAtStart),
    /// #359: the agent profile the child would run under could not be
    /// established from the caller.
    ProfileUnresolved(crate::spawn::env::ProfileUnresolved),
    DuplicateName {
        name: String,
        candidates: Vec<crate::api::schema::AgentInfo>,
    },
}

/// An agent that exec'd cleanly and then exited inside the liveness window
/// (#178).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AgentExitedAtStart {
    /// `None` when the wait itself failed, which is not the same thing as
    /// "exited 0" and must not read like it.
    pub(super) exit_code: Option<u32>,
    /// The last thing the pane printed, which is usually the only
    /// explanation there is: `claude --resume` against a transcript that is
    /// not on disk prints `No conversation found with session ID: <id>` and
    /// exits 0.
    pub(super) last_output: Option<String>,
}

impl AgentExitedAtStart {
    pub(super) fn message(&self) -> String {
        let exit = match self.exit_code {
            Some(code) => format!("exit code {code}"),
            None => "an unreadable exit status".to_string(),
        };
        match self.last_output.as_deref() {
            Some(output) => format!(
                "the agent exited immediately after starting ({exit}); its last output was: {output}"
            ),
            None => format!(
                "the agent exited immediately after starting ({exit}) without printing anything"
            ),
        }
    }
}

/// The last thing a dead pane printed, trimmed to one quotable line (#178).
///
/// Read after the exit, not before: the child's bytes reach the terminal
/// through the PTY reader task, so at the instant `wait()` returns they may
/// not be parsed yet. The settle is bounded — a pane that stays blank is
/// reported as having printed nothing.
fn last_pane_output(runtime: &TerminalRuntime) -> Option<String> {
    let deadline = Instant::now() + START_OUTPUT_SETTLE;
    loop {
        if let Some(line) = last_non_empty_line(&runtime.recent_text(START_OUTPUT_TAIL_LINES)) {
            return Some(line);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(START_LIVENESS_POLL);
    }
}

/// The bottom-most line with anything on it, capped so a pane full of escape
/// noise cannot turn an error message into a screen dump.
fn last_non_empty_line(text: &str) -> Option<String> {
    let line = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let mut capped: String = line.chars().take(START_OUTPUT_MAX_CHARS).collect();
    if capped.chars().count() < line.chars().count() {
        capped.push('…');
    }
    Some(capped)
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

    /// A real directory for a fixture checkout. The new cwd resolution gates
    /// its candidates on `is_dir()`, so a made-up path would silently
    /// exercise the fallback rather than the branch under test.
    fn fixture_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "flock-agent-start-cwd-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("fixture checkout directory");
        std::fs::canonicalize(&dir).expect("canonical fixture path")
    }

    fn worktree_membership(
        checkout: &std::path::Path,
    ) -> crate::workspace::WorktreeSpaceMembership {
        crate::workspace::WorktreeSpaceMembership {
            key: "fixture-repo".into(),
            label: "fixture".into(),
            repo_root: checkout.to_path_buf(),
            checkout_path: checkout.to_path_buf(),
            is_linked_worktree: true,
        }
    }

    /// One workspace, optionally standing at a checkout, active and ready to
    /// be named by `--workspace`.
    fn app_with_workspace(
        name: &str,
        membership: Option<crate::workspace::WorktreeSpaceMembership>,
    ) -> (crate::app::App, String) {
        let mut app = test_app();
        let mut workspace = crate::workspace::Workspace::test_new(name);
        workspace.worktree_space = membership;
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        let workspace_id = app.public_workspace_id(0);
        (app, workspace_id)
    }

    fn targeted_start_request(workspace_id: &str, cwd: Option<String>) -> Request {
        Request {
            id: "req_agent_start_cwd".into(),
            method: Method::AgentStart(AgentStartParams {
                name: "worker".into(),
                cwd,
                workspace_id: Some(workspace_id.to_string()),
                tab_id: None,
                split: None,
                focus: false,
                argv: vec![crate::test_support::no_op_program()],
            }),
        }
    }

    fn shutdown(app: &mut crate::app::App) {
        let runtimes: Vec<_> = app.terminal_runtimes.drain().collect();
        for (_terminal_id, runtime) in runtimes {
            runtime.shutdown();
        }
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

    /// Tear down whatever panes the test left running. A pty child outlives
    /// the test process otherwise.
    fn shutdown(mut app: crate::app::App) {
        let runtimes: Vec<_> = app.terminal_runtimes.drain().collect();
        for (_terminal_id, runtime) in runtimes {
            runtime.shutdown();
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
        let raw = app.handle_api_request(start_request(&crate::test_support::live_program()));
        let response: serde_json::Value = serde_json::from_str(&raw).expect("json");

        assert_ne!(
            response["error"]["code"], "agent_profile_unresolved",
            "only agents with a shell-supplied selector are gated"
        );

        shutdown(app);
    }

    /// A program that stays up is still a successful start — the liveness
    /// window (#178) must not turn every spawn into a refusal.
    #[tokio::test]
    async fn a_program_that_keeps_running_starts_normally() {
        let mut app = test_app();
        let raw = app.handle_api_request(start_request(&crate::test_support::live_program()));
        let response: serde_json::Value = serde_json::from_str(&raw).expect("json");

        assert_eq!(
            response["result"]["type"], "agent_started",
            "unexpected response: {response}"
        );
        assert_eq!(app.state.workspaces.len(), 1);

        shutdown(app);
    }

    /// #178: the failure a successful `spawn` cannot see. The program execs
    /// cleanly and is gone microseconds later, so flock used to answer
    /// `agent_started`, reap the pane a moment afterwards, and leave the
    /// caller holding an agent id for a process that never existed.
    #[tokio::test]
    async fn an_agent_that_exits_the_instant_it_starts_is_not_reported_as_started() {
        let mut app = test_app();
        let raw = app.handle_api_request(start_request(&crate::test_support::no_op_program()));
        let response: serde_json::Value = serde_json::from_str(&raw).expect("json");

        assert_eq!(
            response["error"]["code"], "agent_exited_at_start",
            "unexpected response: {response}"
        );
        assert!(
            app.state.workspaces.is_empty(),
            "a start that died must not leave its workspace behind"
        );

        shutdown(app);
    }

    /// The other half of the same claim, and the one #178 opened on: a binary
    /// that is not on `PATH` at all. This one never reaches the liveness
    /// window — `exec` failure is reported synchronously by the pty spawn, so
    /// the refusal is `agent_start_failed` and no pane is ever built. Asserted
    /// rather than assumed, because the issue's premise was that it was NOT.
    #[tokio::test]
    async fn a_missing_agent_binary_is_refused_before_a_pane_exists() {
        let mut app = test_app();
        let raw = app.handle_api_request(start_request("flock-178-no-such-binary"));
        let response: serde_json::Value = serde_json::from_str(&raw).expect("json");

        assert_eq!(
            response["error"]["code"], "agent_start_failed",
            "unexpected response: {response}"
        );
        assert!(app.state.workspaces.is_empty());

        shutdown(app);
    }

    #[test]
    fn the_quoted_line_is_the_last_one_with_anything_on_it() {
        assert_eq!(
            super::last_non_empty_line(
                "No conversation found with session ID: abc

  
"
            ),
            Some("No conversation found with session ID: abc".to_string())
        );
        assert_eq!(
            super::last_non_empty_line(
                "
   
"
            ),
            None
        );
    }

    /// A pane full of escape noise must not turn one error message into a
    /// screen dump on the socket.
    #[test]
    fn a_very_long_line_is_capped_and_marked() {
        let line = "x".repeat(super::START_OUTPUT_MAX_CHARS + 50);
        let quoted = super::last_non_empty_line(&line).expect("a line");
        assert_eq!(quoted.chars().count(), super::START_OUTPUT_MAX_CHARS + 1);
        assert!(quoted.ends_with('…'));
    }

    /// An unreadable wait is not "exited 0", and the message must not read
    /// like it is.
    #[test]
    fn an_unreadable_exit_is_not_reported_as_a_clean_one() {
        let message = super::AgentExitedAtStart {
            exit_code: None,
            last_output: None,
        }
        .message();
        assert!(message.contains("unreadable exit status"), "{message}");

        let message = super::AgentExitedAtStart {
            exit_code: Some(0),
            last_output: Some("No conversation found".into()),
        }
        .message();
        assert!(message.contains("exit code 0"), "{message}");
        assert!(message.contains("No conversation found"), "{message}");
    }

    /// #365: `--workspace <id>` named where the PANE goes and nothing about
    /// where the PROCESS starts, so the agent opened in the server's own cwd —
    /// `$HOME` for a daemon started from a login shell. Claude Code then asks
    /// the operator to trust the entire home directory, and the reflex is to
    /// accept.
    #[tokio::test]
    async fn a_workspace_targeted_agent_starts_in_that_workspaces_checkout() {
        let checkout = fixture_dir("checkout");
        let (mut app, workspace_id) =
            app_with_workspace("issue-365", Some(worktree_membership(&checkout)));

        let raw = app.handle_api_request(targeted_start_request(&workspace_id, None));
        let response: serde_json::Value = serde_json::from_str(&raw).expect("json");

        assert_eq!(response["result"]["type"], "agent_started");
        assert_eq!(
            response["result"]["agent"]["cwd"],
            checkout.to_string_lossy().as_ref(),
            "the agent belongs in the checkout its workspace stands in"
        );

        shutdown(&mut app);
        let _ = std::fs::remove_dir_all(&checkout);
    }

    /// A workspace that is not a flock-managed worktree still names a place.
    /// Following the target pane is what `pane.split` and `tab.create` already
    /// do; `agent.start` was the one pane-creating verb that did not.
    #[tokio::test]
    async fn a_workspace_targeted_agent_follows_the_target_pane_without_a_worktree() {
        let elsewhere = fixture_dir("pane");
        let (mut app, workspace_id) = app_with_workspace("no-worktree", None);
        for terminal in app.state.terminals.values_mut() {
            terminal.cwd = elsewhere.clone();
        }

        let raw = app.handle_api_request(targeted_start_request(&workspace_id, None));
        let response: serde_json::Value = serde_json::from_str(&raw).expect("json");

        assert_eq!(response["result"]["type"], "agent_started");
        assert_eq!(
            response["result"]["agent"]["cwd"],
            elsewhere.to_string_lossy().as_ref(),
            "with no checkout to prefer, the agent lands where the pane it joins stands"
        );

        shutdown(&mut app);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    /// `--cwd` is still the last word. It was the documented workaround for
    /// this bug, and scripts carrying it must not start behaving differently
    /// once the default improves.
    #[tokio::test]
    async fn an_explicit_cwd_still_outranks_the_workspace_target() {
        let checkout = fixture_dir("outranked-checkout");
        let asked_for = fixture_dir("asked-for");
        let (mut app, workspace_id) =
            app_with_workspace("explicit", Some(worktree_membership(&checkout)));

        let raw = app.handle_api_request(targeted_start_request(
            &workspace_id,
            Some(asked_for.to_string_lossy().into_owned()),
        ));
        let response: serde_json::Value = serde_json::from_str(&raw).expect("json");

        assert_eq!(response["result"]["type"], "agent_started");
        assert_eq!(
            response["result"]["agent"]["cwd"],
            asked_for.to_string_lossy().as_ref()
        );

        shutdown(&mut app);
        let _ = std::fs::remove_dir_all(&checkout);
        let _ = std::fs::remove_dir_all(&asked_for);
    }

    /// A membership whose checkout has been deleted must not turn a working
    /// verb into an ENOENT from the PTY layer. It degrades to the old
    /// fallback, which is a wrong directory rather than no agent at all.
    #[tokio::test]
    async fn a_vanished_checkout_falls_back_instead_of_failing_the_start() {
        let gone = std::env::temp_dir().join(format!(
            "flock-agent-start-cwd-{}-vanished",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&gone);
        let (mut app, workspace_id) =
            app_with_workspace("vanished", Some(worktree_membership(&gone)));

        let raw = app.handle_api_request(targeted_start_request(&workspace_id, None));
        let response: serde_json::Value = serde_json::from_str(&raw).expect("json");

        assert_eq!(response["result"]["type"], "agent_started");
        assert_ne!(
            response["result"]["agent"]["cwd"],
            gone.to_string_lossy().as_ref(),
            "a checkout that is not there cannot be started in"
        );

        shutdown(&mut app);
    }
}
