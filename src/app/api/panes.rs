use bytes::Bytes;

use crate::api::schema::{
    EventData, EventEnvelope, EventKind, PaneClearAgentAuthorityParams, PaneListParams,
    PaneMoveDestination, PaneMoveParams, PaneMoveReason, PaneMoveResult, PaneReadParams,
    PaneReadResult, PaneReleaseAgentParams, PaneRenameParams, PaneReportAgentParams,
    PaneReportAgentSessionParams, PaneReportMetadataParams, PaneSendInputParams,
    PaneSendKeysParams, PaneSendTextParams, PaneSplitParams, PaneTarget, ReadFormat, ReadSource,
    ResponseResult,
};
use crate::app::{App, Mode};

use super::super::api_helpers::{
    detect_state_from_api, encode_api_keys, encode_api_text, normalize_custom_status,
    normalize_reported_agent_label, sanitize_reported_prompt,
};
use super::responses::{encode_error, encode_success};

impl App {
    pub(super) fn handle_pane_split(&mut self, id: String, params: PaneSplitParams) -> String {
        let Some((ws_idx, target_pane_id)) = self.parse_pane_id(&params.target_pane_id) else {
            return pane_not_found(id, &params.target_pane_id);
        };
        let (rows, cols) = self.state.estimate_pane_size();
        let split_cwd = params.cwd.map(std::path::PathBuf::from).or_else(|| {
            let follow_cwd = self.state.workspaces.get(ws_idx).and_then(|ws| {
                let tab_idx = ws.find_tab_index_for_pane(target_pane_id)?;
                ws.tabs.get(tab_idx)?.cwd_for_pane(
                    target_pane_id,
                    &self.state.terminals,
                    &self.terminal_runtimes,
                )
            });
            Some(self.resolve_new_terminal_cwd(follow_cwd))
        });
        let default_shell = self.state.default_shell.clone();
        let scrollback_limit_bytes = self.state.pane_scrollback_limit_bytes;
        let host_terminal_theme = self.state.host_terminal_theme;
        let previous_focus = self.state.current_pane_focus_target();
        let Some(ws) = self.state.workspaces.get_mut(ws_idx) else {
            return pane_not_found(id, &params.target_pane_id);
        };
        let direction = match params.direction {
            crate::api::schema::SplitDirection::Right => ratatui::layout::Direction::Horizontal,
            crate::api::schema::SplitDirection::Down => ratatui::layout::Direction::Vertical,
        };
        let (target_tab_idx, new_pane) = match ws.split_pane(
            target_pane_id,
            direction,
            rows,
            cols,
            split_cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            crate::pane::PaneShellConfig::new(&default_shell, self.state.shell_mode),
            params.focus,
        ) {
            Some(Ok(result)) => result,
            Some(Err(err)) => return encode_error(id, "pane_split_failed", err.to_string()),
            None => return pane_not_found(id, &params.target_pane_id),
        };
        if params.focus {
            self.state.switch_workspace_tab(ws_idx, target_tab_idx);
            self.state
                .record_pane_focus_change(previous_focus, ws_idx, new_pane.pane_id);
            self.state.mode = Mode::Terminal;
        }
        self.terminal_runtimes
            .insert(new_pane.terminal.id.clone(), new_pane.runtime);
        self.state
            .remove_alias_shadowed_by_new_pane(new_pane.pane_id);
        self.state
            .terminals
            .insert(new_pane.terminal.id.clone(), new_pane.terminal);
        self.schedule_session_save();
        let pane = self.pane_info(ws_idx, new_pane.pane_id).unwrap();
        self.emit_event(EventEnvelope {
            event: EventKind::PaneCreated,
            data: EventData::PaneCreated { pane: pane.clone() },
        });

        encode_success(id, ResponseResult::PaneInfo { pane })
    }

    /// Relocate a live pane (with its still-running PTY) into another existing
    /// tab, a brand-new tab, or a brand-new workspace. The pane's terminal
    /// runtime stays registered under the same terminal id at the App level —
    /// only its containing tab/workspace changes, plus the pane's
    /// workspace-scoped public id when the destination is a different
    /// workspace (this is consistent with the fork's per-workspace stable
    /// numbering: closed numbers are never reused, so a stale hook script
    /// cannot retarget a different live pane). Federation/peer summaries
    /// operate at workspace level, so a pane move that does not strand an
    /// empty workspace is transparent to peers; when the source workspace
    /// becomes empty and is closed, that is reported via the existing
    /// `WorkspaceClosed` event, which peer summaries already react to.
    pub(super) fn handle_pane_move(&mut self, id: String, params: PaneMoveParams) -> String {
        let PaneMoveParams {
            pane_id,
            destination,
            focus,
        } = params;
        let Some((source_ws_idx, source_pane_id)) = self.parse_pane_id(&pane_id) else {
            return pane_not_found(id, &pane_id);
        };
        let Some(source_tab_idx) =
            self.state.workspaces[source_ws_idx].find_tab_index_for_pane(source_pane_id)
        else {
            return pane_not_found(id, &pane_id);
        };
        let previous_pane_id = self
            .public_pane_id(source_ws_idx, source_pane_id)
            .unwrap_or_else(|| pane_id.clone());
        let previous_workspace_id = self.public_workspace_id(source_ws_idx);
        let Some(previous_tab_id) = self.public_tab_id(source_ws_idx, source_tab_idx) else {
            return encode_error(id, "tab_not_found", "source tab not found");
        };
        let Some(source_terminal_id) = self
            .state
            .workspaces
            .get(source_ws_idx)
            .and_then(|ws| ws.tabs.get(source_tab_idx))
            .and_then(|tab| tab.terminal_id(source_pane_id))
            .cloned()
        else {
            return pane_not_found(id, &pane_id);
        };

        let resolved = match destination {
            PaneMoveDestination::Tab {
                tab_id,
                target_pane_id,
                split,
                ratio,
            } => {
                let Some((target_ws_idx, target_tab_idx)) = self.parse_tab_id(&tab_id) else {
                    return encode_error(id, "tab_not_found", format!("tab {tab_id} not found"));
                };
                if source_ws_idx == target_ws_idx && source_tab_idx == target_tab_idx {
                    let Some(pane) = self.pane_info(source_ws_idx, source_pane_id) else {
                        return pane_not_found(id, &pane_id);
                    };
                    let focused_pane_id = self
                        .public_pane_id(
                            source_ws_idx,
                            self.state.workspaces[source_ws_idx].tabs[source_tab_idx]
                                .layout
                                .focused(),
                        )
                        .unwrap_or_else(|| previous_pane_id.clone());
                    return encode_success(
                        id,
                        ResponseResult::PaneMove {
                            move_result: PaneMoveResult {
                                changed: false,
                                reason: Some(PaneMoveReason::SameTab),
                                previous_pane_id,
                                previous_workspace_id,
                                previous_tab_id,
                                pane: Box::new(pane),
                                created_workspace: None,
                                created_tab: None,
                                closed_workspace_id: None,
                                closed_tab_id: None,
                                focused_pane_id,
                            },
                        },
                    );
                }
                let target_pane_id = match target_pane_id {
                    Some(raw) => {
                        let Some((pane_ws_idx, pane_id)) = self.parse_pane_id(&raw) else {
                            return encode_error(
                                id,
                                "target_pane_not_found",
                                format!("target pane {raw} not found"),
                            );
                        };
                        let pane_tab_idx =
                            self.state.workspaces[pane_ws_idx].find_tab_index_for_pane(pane_id);
                        if pane_ws_idx != target_ws_idx || pane_tab_idx != Some(target_tab_idx) {
                            return encode_error(
                                id,
                                "target_pane_not_found",
                                format!("target pane {raw} is not in tab {tab_id}"),
                            );
                        }
                        pane_id
                    }
                    None => self.state.workspaces[target_ws_idx].tabs[target_tab_idx]
                        .layout
                        .focused(),
                };
                let Some(target_tab_id) = self.public_tab_id(target_ws_idx, target_tab_idx) else {
                    return encode_error(id, "tab_not_found", format!("tab {tab_id} not found"));
                };
                ResolvedPaneMoveDestination::ExistingTab {
                    tab_id: target_tab_id,
                    target_pane_id,
                    split,
                    ratio: ratio.unwrap_or(0.5),
                    cross_workspace: source_ws_idx != target_ws_idx,
                }
            }
            PaneMoveDestination::NewTab {
                workspace_id,
                label,
            } => {
                let target_workspace_id = if let Some(workspace_id) = workspace_id {
                    let Some(ws_idx) = self.parse_workspace_id(&workspace_id) else {
                        return encode_error(
                            id,
                            "workspace_not_found",
                            format!("workspace {workspace_id} not found"),
                        );
                    };
                    self.public_workspace_id(ws_idx)
                } else {
                    previous_workspace_id.clone()
                };
                ResolvedPaneMoveDestination::NewTab {
                    workspace_id: target_workspace_id,
                    label,
                }
            }
            PaneMoveDestination::NewWorkspace { label, tab_label } => {
                ResolvedPaneMoveDestination::NewWorkspace { label, tab_label }
            }
        };

        let previous_focus = self.state.current_pane_focus_target();
        let recovery_context = PaneMoveRecoveryContext {
            source_ws_idx,
            previous_workspace_id: previous_workspace_id.clone(),
            previous_workspace_label: self.state.workspaces[source_ws_idx].custom_name.clone(),
            previous_tab_label: self.state.workspaces[source_ws_idx].tabs[source_tab_idx]
                .custom_name
                .clone(),
            previous_worktree_space: self.state.workspaces[source_ws_idx].worktree_space.clone(),
            identity_cwd: self.state.workspaces[source_ws_idx].identity_cwd.clone(),
        };

        let taken = match self
            .state
            .workspaces
            .get_mut(source_ws_idx)
            .and_then(|ws| ws.take_pane_for_move(source_pane_id))
        {
            Some(taken) => taken,
            None => return encode_error(id, "pane_move_failed", "source pane could not be moved"),
        };
        let source_removed_tab_id = taken.removed_tab_idx.map(|_| previous_tab_id.clone());
        let source_workspace_empty = taken.workspace_empty;
        let moved = taken.moved;
        let cross_workspace = match &resolved {
            ResolvedPaneMoveDestination::ExistingTab {
                cross_workspace, ..
            } => *cross_workspace,
            ResolvedPaneMoveDestination::NewTab { workspace_id, .. } => {
                workspace_id != &previous_workspace_id
            }
            ResolvedPaneMoveDestination::NewWorkspace { .. } => true,
        };
        if cross_workspace {
            if let Some(ws) = self.state.workspaces.get_mut(source_ws_idx) {
                ws.unregister_moved_pane(source_pane_id);
            }
        }

        let mut closed_workspace_id = None;
        if source_workspace_empty && cross_workspace {
            self.state.workspaces.remove(source_ws_idx);
            closed_workspace_id = Some(previous_workspace_id.clone());
            if self.state.workspaces.is_empty() {
                self.state.active = None;
                self.state.selected = 0;
            } else {
                if let Some(active) = self.state.active {
                    if active == source_ws_idx {
                        self.state.active =
                            Some(source_ws_idx.min(self.state.workspaces.len() - 1));
                    } else if active > source_ws_idx {
                        self.state.active = Some(active - 1);
                    }
                }
                if self.state.selected == source_ws_idx {
                    self.state.selected = source_ws_idx.min(self.state.workspaces.len() - 1);
                } else if self.state.selected > source_ws_idx {
                    self.state.selected -= 1;
                }
            }
        }

        let mut created_workspace_flag = false;
        let mut created_tab_flag = false;
        let (target_ws_idx, target_tab_idx, moved_pane_id) = match resolved {
            ResolvedPaneMoveDestination::ExistingTab {
                tab_id,
                target_pane_id,
                split,
                ratio,
                cross_workspace: _,
            } => {
                let Some((target_ws_idx, target_tab_idx)) = self.parse_tab_id(&tab_id) else {
                    self.recover_failed_pane_move(recovery_context, moved);
                    return encode_error(id, "pane_move_failed", "target tab disappeared");
                };
                let previous_target_focus = self.state.workspaces[target_ws_idx].tabs
                    [target_tab_idx]
                    .layout
                    .focused();
                let direction = split_direction_to_layout(split);
                let moved_pane_id = match self.state.workspaces[target_ws_idx]
                    .insert_moved_pane_into_tab(
                        target_tab_idx,
                        target_pane_id,
                        moved,
                        direction,
                        ratio,
                    ) {
                    Ok(pane_id) => pane_id,
                    Err(moved) => {
                        self.recover_failed_pane_move(recovery_context, moved);
                        return encode_error(
                            id,
                            "pane_move_failed",
                            "target pane could not be split",
                        );
                    }
                };
                if !focus {
                    self.state.workspaces[target_ws_idx].tabs[target_tab_idx]
                        .layout
                        .focus_pane(previous_target_focus);
                }
                (target_ws_idx, target_tab_idx, moved_pane_id)
            }
            ResolvedPaneMoveDestination::NewTab {
                workspace_id,
                label,
            } => {
                let Some(target_ws_idx) = self.parse_workspace_id(&workspace_id) else {
                    self.recover_failed_pane_move(recovery_context, moved);
                    return encode_error(id, "pane_move_failed", "target workspace disappeared");
                };
                let moved_pane_id = moved.pane_id;
                let target_tab_idx = self.state.workspaces[target_ws_idx]
                    .create_tab_from_existing_pane(
                        moved,
                        label,
                        self.event_tx.clone(),
                        self.render_notify.clone(),
                        self.render_dirty.clone(),
                    );
                created_tab_flag = true;
                (target_ws_idx, target_tab_idx, moved_pane_id)
            }
            ResolvedPaneMoveDestination::NewWorkspace { label, tab_label } => {
                let identity_cwd = self
                    .state
                    .terminals
                    .get(&source_terminal_id)
                    .map(|terminal| terminal.cwd.clone())
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| "/".into()));
                let moved_pane_id = moved.pane_id;
                let workspace = crate::workspace::Workspace::from_existing_pane(
                    label,
                    tab_label,
                    identity_cwd,
                    moved,
                    self.event_tx.clone(),
                    self.render_notify.clone(),
                    self.render_dirty.clone(),
                );
                self.state.workspaces.push(workspace);
                let target_ws_idx = self.state.workspaces.len() - 1;
                created_workspace_flag = true;
                created_tab_flag = true;
                (target_ws_idx, 0, moved_pane_id)
            }
        };

        if focus || self.state.active.is_none() {
            self.state
                .switch_workspace_tab(target_ws_idx, target_tab_idx);
            self.state
                .record_pane_focus_change(previous_focus, target_ws_idx, moved_pane_id);
            self.state.mode = crate::app::Mode::Terminal;
        }
        let created_workspace = created_workspace_flag.then(|| self.workspace_info(target_ws_idx));
        let created_tab = if created_tab_flag {
            self.tab_info(target_ws_idx, target_tab_idx)
        } else {
            None
        };

        self.state.remove_alias_shadowed_by_new_pane(moved_pane_id);
        self.state.mark_session_dirty();
        self.schedule_session_save();
        let Some(pane) = self.pane_info(target_ws_idx, moved_pane_id) else {
            return encode_error(id, "pane_move_failed", "moved pane is unavailable");
        };
        let focused_pane_id = self
            .public_pane_id(
                target_ws_idx,
                self.state.workspaces[target_ws_idx].tabs[target_tab_idx]
                    .layout
                    .focused(),
            )
            .unwrap_or_else(|| pane.pane_id.clone());
        let move_result = PaneMoveResult {
            changed: true,
            reason: None,
            previous_pane_id: previous_pane_id.clone(),
            previous_workspace_id: previous_workspace_id.clone(),
            previous_tab_id: previous_tab_id.clone(),
            pane: Box::new(pane.clone()),
            created_workspace: created_workspace.clone(),
            created_tab: created_tab.clone(),
            closed_workspace_id: closed_workspace_id.clone(),
            closed_tab_id: source_removed_tab_id.clone(),
            focused_pane_id,
        };
        if let Some(closed_tab_id) = &source_removed_tab_id {
            self.emit_event(EventEnvelope {
                event: EventKind::TabClosed,
                data: EventData::TabClosed {
                    tab_id: closed_tab_id.clone(),
                    workspace_id: previous_workspace_id.clone(),
                },
            });
        }
        if let Some(closed_workspace_id) = &closed_workspace_id {
            self.emit_event(EventEnvelope {
                event: EventKind::WorkspaceClosed,
                data: EventData::WorkspaceClosed {
                    workspace_id: closed_workspace_id.clone(),
                },
            });
        }
        if let Some(workspace) = &created_workspace {
            self.emit_event(EventEnvelope {
                event: EventKind::WorkspaceCreated,
                data: EventData::WorkspaceCreated {
                    workspace: workspace.clone(),
                },
            });
        }
        if let Some(tab) = &created_tab {
            self.emit_event(EventEnvelope {
                event: EventKind::TabCreated,
                data: EventData::TabCreated { tab: tab.clone() },
            });
        }
        self.emit_event(EventEnvelope {
            event: EventKind::PaneMoved,
            data: EventData::PaneMoved {
                previous_pane_id,
                previous_workspace_id,
                previous_tab_id,
                pane: Box::new(pane),
                created_workspace,
                created_tab,
                closed_workspace_id,
                closed_tab_id: source_removed_tab_id,
            },
        });

        encode_success(id, ResponseResult::PaneMove { move_result })
    }

    /// Rebuild the source side of a failed move so the pane lives somewhere
    /// reasonable. Tries to reattach to the original workspace if it still
    /// exists; otherwise recreates the workspace at the original index with
    /// the same id and federation metadata so peer summaries stay aligned.
    fn recover_failed_pane_move(
        &mut self,
        context: PaneMoveRecoveryContext,
        moved: crate::workspace::MovedPane,
    ) {
        if let Some(ws_idx) = self.parse_workspace_id(&context.previous_workspace_id) {
            self.state.workspaces[ws_idx].create_tab_from_existing_pane(
                moved,
                context.previous_tab_label,
                self.event_tx.clone(),
                self.render_notify.clone(),
                self.render_dirty.clone(),
            );
        } else {
            let mut workspace = crate::workspace::Workspace::from_existing_pane(
                context.previous_workspace_label,
                context.previous_tab_label,
                context.identity_cwd,
                moved,
                self.event_tx.clone(),
                self.render_notify.clone(),
                self.render_dirty.clone(),
            );
            workspace.id = context.previous_workspace_id;
            workspace.worktree_space = context.previous_worktree_space;
            let insert_idx = context.source_ws_idx.min(self.state.workspaces.len());
            if let Some(active) = self.state.active {
                if active >= insert_idx {
                    self.state.active = Some(active + 1);
                }
            }
            if self.state.selected >= insert_idx && !self.state.workspaces.is_empty() {
                self.state.selected += 1;
            }
            self.state.workspaces.insert(insert_idx, workspace);
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();
    }

    pub(super) fn handle_pane_list(&mut self, id: String, params: PaneListParams) -> String {
        match self.collect_panes_for_workspace(params.workspace_id.as_deref()) {
            Ok(panes) => encode_success(id, ResponseResult::PaneList { panes }),
            Err((code, message)) => encode_error(id, &code, message),
        }
    }

    pub(super) fn handle_pane_get(&mut self, id: String, target: PaneTarget) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&target.pane_id) else {
            return pane_not_found(id, &target.pane_id);
        };
        let Some(pane) = self.pane_info(ws_idx, pane_id) else {
            return pane_not_found(id, &target.pane_id);
        };

        encode_success(id, ResponseResult::PaneInfo { pane })
    }

    pub(super) fn handle_pane_rename(&mut self, id: String, params: PaneRenameParams) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.terminal_id(pane_id))
            .cloned()
        else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some(terminal) = self.state.terminals.get_mut(&terminal_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        match params.label.map(|label| label.trim().to_string()) {
            Some(label) if !label.is_empty() => terminal.set_manual_label(label),
            _ => terminal.clear_manual_label(),
        }
        self.state.mark_session_dirty();
        let pane = self.pane_info(ws_idx, pane_id).unwrap();

        encode_success(id, ResponseResult::PaneInfo { pane })
    }

    pub(super) fn handle_pane_read(&mut self, id: String, params: PaneReadParams) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        // Echo back the canonical public id so a request that used the legacy
        // `<ws>-<n>` form still reads the new-style `<ws>:p<n>` in the result.
        let Some(public_pane_id) = self.public_pane_id(ws_idx, pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some((pane, workspace_id)) = self.lookup_runtime(ws_idx, pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some(tab_idx) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.find_tab_index_for_pane(pane_id))
        else {
            return pane_not_found(id, &params.pane_id);
        };
        let requested_lines = params.lines.unwrap_or(80).min(1000) as usize;
        let text = match params.format {
            ReadFormat::Text => match params.source {
                ReadSource::Visible => pane.visible_text(),
                ReadSource::Recent => pane.recent_text(requested_lines),
                ReadSource::RecentUnwrapped => pane.recent_unwrapped_text(requested_lines),
            },
            ReadFormat::Ansi => match params.source {
                ReadSource::Visible => pane.visible_ansi(),
                ReadSource::Recent => pane.recent_ansi(requested_lines),
                ReadSource::RecentUnwrapped => pane.recent_unwrapped_ansi(requested_lines),
            },
        };

        encode_success(
            id,
            ResponseResult::PaneRead {
                read: PaneReadResult {
                    pane_id: public_pane_id,
                    workspace_id,
                    tab_id: self.public_tab_id(ws_idx, tab_idx).unwrap(),
                    source: params.source,
                    format: params.format,
                    text,
                    revision: 0,
                    truncated: false,
                },
            },
        )
    }

    pub(super) fn handle_pane_report_agent(
        &mut self,
        id: String,
        params: PaneReportAgentParams,
    ) -> String {
        let Some((_ws_idx, pane_id)) =
            self.parse_pane_id_or_peer(&params.pane_id, self.current_api_peer_pid)
        else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some(agent_label) = normalize_reported_agent_label(&params.agent) else {
            return invalid_agent(id);
        };
        self.handle_internal_event(crate::events::AppEvent::HookStateReported {
            pane_id,
            session_ref: crate::agent_resume::session_ref_from_report(
                &params.source,
                &agent_label,
                params.agent_session_id,
                params.agent_session_path,
            ),
            source: params.source,
            agent_label,
            state: detect_state_from_api(params.state),
            message: params.message,
            custom_status: normalize_custom_status(params.custom_status),
            seq: params.seq,
        });

        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_pane_report_prompt(
        &mut self,
        id: String,
        params: crate::api::schema::PaneReportPromptParams,
    ) -> String {
        let Some((_ws_idx, pane_id)) =
            self.parse_pane_id_or_peer(&params.pane_id, self.current_api_peer_pid)
        else {
            return pane_not_found(id, &params.pane_id);
        };
        if normalize_reported_agent_label(&params.agent).is_none() {
            return invalid_agent(id);
        }
        let prompt = sanitize_reported_prompt(&params.prompt);
        if prompt.is_empty() {
            return encode_success(id, ResponseResult::Ok {});
        }
        self.handle_internal_event(crate::events::AppEvent::HookPromptReported { pane_id, prompt });
        encode_success(id, ResponseResult::Ok {})
    }

    /// Append a recap entry to the pane's prompt-history scrollback. Recaps
    /// are wired from session lifecycle hooks (e.g. Claude Stop) — the API
    /// just stores them; they render visually distinct from prompts.
    pub(super) fn handle_pane_report_recap(
        &mut self,
        id: String,
        params: crate::api::schema::PaneReportRecapParams,
    ) -> String {
        let Some((_ws_idx, pane_id)) =
            self.parse_pane_id_or_peer(&params.pane_id, self.current_api_peer_pid)
        else {
            return pane_not_found(id, &params.pane_id);
        };
        if normalize_reported_agent_label(&params.agent).is_none() {
            return invalid_agent(id);
        }
        let recap = sanitize_reported_prompt(&params.recap);
        if recap.is_empty() {
            return encode_success(id, ResponseResult::Ok {});
        }
        self.handle_internal_event(crate::events::AppEvent::HookRecapReported { pane_id, recap });
        encode_success(id, ResponseResult::Ok {})
    }

    /// Append a reply entry to the pane's prompt-history scrollback. Replies
    /// carry the agent's last assistant message (Stop-hook scraped, capped
    /// on the wire). Stored verbatim after the same sanitize pass prompts
    /// get; rendered in a distinct palette color so prompt/reply/recap read
    /// as three glanceable tones in the float.
    pub(super) fn handle_pane_report_reply(
        &mut self,
        id: String,
        params: crate::api::schema::PaneReportReplyParams,
    ) -> String {
        let Some((_ws_idx, pane_id)) =
            self.parse_pane_id_or_peer(&params.pane_id, self.current_api_peer_pid)
        else {
            return pane_not_found(id, &params.pane_id);
        };
        if normalize_reported_agent_label(&params.agent).is_none() {
            return invalid_agent(id);
        }
        let reply = sanitize_reported_prompt(&params.reply);
        if reply.is_empty() {
            return encode_success(id, ResponseResult::Ok {});
        }
        self.handle_internal_event(crate::events::AppEvent::HookReplyReported { pane_id, reply });
        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_pane_report_agent_session(
        &mut self,
        id: String,
        params: PaneReportAgentSessionParams,
    ) -> String {
        let Some((_ws_idx, pane_id)) =
            self.parse_pane_id_or_peer(&params.pane_id, self.current_api_peer_pid)
        else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some(agent_label) = normalize_reported_agent_label(&params.agent) else {
            return invalid_agent(id);
        };
        self.handle_internal_event(crate::events::AppEvent::AgentSessionReported {
            pane_id,
            session_ref: crate::agent_resume::session_ref_from_report(
                &params.source,
                &agent_label,
                params.agent_session_id,
                params.agent_session_path,
            ),
            source: params.source,
            agent_label,
            seq: params.seq,
            session_start_source: crate::agent_resume::normalize_claude_session_start_source(
                params.session_start_source,
            ),
        });

        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_pane_report_metadata(
        &mut self,
        id: String,
        params: PaneReportMetadataParams,
    ) -> String {
        let Some((_ws_idx, pane_id)) =
            self.parse_pane_id_or_peer(&params.pane_id, self.current_api_peer_pid)
        else {
            return pane_not_found(id, &params.pane_id);
        };
        let agent_label = match params.agent.as_deref() {
            Some(agent) => match normalize_reported_agent_label(agent) {
                Some(agent_label) => Some(agent_label),
                None => return invalid_agent(id),
            },
            None => None,
        };
        let Some(source) = normalize_optional_text(Some(params.source)) else {
            return encode_error(id, "invalid_metadata_request", "missing metadata source");
        };
        let raw_title_set = params.title.is_some();
        let raw_display_agent_set = params.display_agent.is_some();
        let raw_custom_status_set = params.custom_status.is_some();
        let raw_state_labels_set = !params.state_labels.is_empty();
        let ttl = params.ttl_ms.map(std::time::Duration::from_millis);
        let title = normalize_presentation_text(params.title);
        let display_agent = normalize_presentation_text(params.display_agent);
        let custom_status = normalize_custom_status(params.custom_status);
        let applies_to_source = match params.applies_to_source {
            Some(applies_to_source) => {
                let Some(applies_to_source) = normalize_optional_text(Some(applies_to_source))
                else {
                    return encode_error(
                        id,
                        "invalid_metadata_request",
                        "missing metadata authority source",
                    );
                };
                Some(applies_to_source)
            }
            None => None,
        };
        let state_labels = match normalize_state_labels(params.state_labels) {
            Ok(labels) => labels,
            Err(status) => {
                return encode_error(
                    id,
                    "invalid_state_label",
                    format!("unknown state label: {status}"),
                );
            }
        };
        if raw_title_set && params.clear_title
            || raw_display_agent_set && params.clear_display_agent
            || raw_custom_status_set && params.clear_custom_status
            || raw_state_labels_set && params.clear_state_labels
        {
            return encode_error(
                id,
                "invalid_metadata_request",
                "cannot set and clear the same metadata field",
            );
        }
        if title.is_none()
            && display_agent.is_none()
            && custom_status.is_none()
            && state_labels.is_empty()
            && !params.clear_title
            && !params.clear_display_agent
            && !params.clear_custom_status
            && !params.clear_state_labels
        {
            return encode_error(
                id,
                "invalid_metadata_request",
                "missing metadata field to set or clear",
            );
        }
        self.handle_internal_event(crate::events::AppEvent::HookMetadataReported {
            pane_id,
            source,
            agent_label,
            applies_to_source,
            title,
            display_agent,
            custom_status,
            state_labels,
            clear_title: params.clear_title,
            clear_display_agent: params.clear_display_agent,
            clear_custom_status: params.clear_custom_status,
            clear_state_labels: params.clear_state_labels,
            seq: params.seq,
            ttl,
        });

        encode_success(id, ResponseResult::Ok {})
    }

    /// Promote a session-specific field onto the calling pane's header.
    /// Validation (lengths, cap) is answered synchronously here; the actual
    /// mutation rides `update_terminal_state` via an internal event, the
    /// shared chokepoint both event loops consume — same path as
    /// `pane.report_prompt`.
    pub(super) fn handle_pane_set_header_field(
        &mut self,
        id: String,
        params: crate::api::schema::PaneSetHeaderFieldParams,
    ) -> String {
        let Some((ws_idx, pane_id)) =
            self.parse_pane_id_or_peer(&params.pane_id, self.current_api_peer_pid)
        else {
            return pane_not_found(id, &params.pane_id);
        };
        let (key, value) = match crate::terminal::validate_header_field(&params.key, &params.value)
        {
            Ok(field) => field,
            Err(err) => return encode_error(id, "invalid_header_field", err.to_string()),
        };
        let Some(terminal) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.terminal_id(pane_id))
            .and_then(|terminal_id| self.state.terminals.get(terminal_id))
        else {
            return pane_not_found(id, &params.pane_id);
        };
        if !terminal.has_header_field_capacity(&key, std::time::Instant::now()) {
            return encode_error(
                id,
                "too_many_header_fields",
                crate::terminal::HeaderFieldError::TooManyFields.to_string(),
            );
        }
        self.handle_internal_event(crate::events::AppEvent::PaneHeaderFieldSet {
            pane_id,
            key,
            value,
            ttl: params.ttl_secs.map(std::time::Duration::from_secs),
        });

        encode_success(id, ResponseResult::Ok {})
    }

    /// Clear a promoted header field on the calling pane. Idempotent.
    pub(super) fn handle_pane_clear_header_field(
        &mut self,
        id: String,
        params: crate::api::schema::PaneClearHeaderFieldParams,
    ) -> String {
        let Some((_ws_idx, pane_id)) =
            self.parse_pane_id_or_peer(&params.pane_id, self.current_api_peer_pid)
        else {
            return pane_not_found(id, &params.pane_id);
        };
        let key = params.key.trim().to_string();
        if key.is_empty() {
            return encode_error(
                id,
                "invalid_header_field",
                crate::terminal::HeaderFieldError::EmptyKey.to_string(),
            );
        }
        self.handle_internal_event(crate::events::AppEvent::PaneHeaderFieldCleared {
            pane_id,
            key,
        });

        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_pane_clear_agent_authority(
        &mut self,
        id: String,
        params: PaneClearAgentAuthorityParams,
    ) -> String {
        let Some((_ws_idx, pane_id)) =
            self.parse_pane_id_or_peer(&params.pane_id, self.current_api_peer_pid)
        else {
            return pane_not_found(id, &params.pane_id);
        };
        self.handle_internal_event(crate::events::AppEvent::HookAuthorityCleared {
            pane_id,
            source: params.source,
            seq: params.seq,
        });

        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_pane_release_agent(
        &mut self,
        id: String,
        params: PaneReleaseAgentParams,
    ) -> String {
        let Some((_ws_idx, pane_id)) =
            self.parse_pane_id_or_peer(&params.pane_id, self.current_api_peer_pid)
        else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some(agent_label) = normalize_reported_agent_label(&params.agent) else {
            return invalid_agent(id);
        };
        self.handle_internal_event(crate::events::AppEvent::HookAgentReleased {
            pane_id,
            source: params.source,
            known_agent: crate::detect::parse_agent_label(&agent_label),
            agent_label,
            seq: params.seq,
        });

        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_pane_send_text(
        &mut self,
        id: String,
        params: PaneSendTextParams,
    ) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some(runtime) = self.lookup_runtime_sender(ws_idx, pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        if let Err(err) = runtime.try_send_bytes(Bytes::from(params.text)) {
            return encode_error(id, "pane_send_failed", err.to_string());
        }

        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_pane_send_input(
        &mut self,
        id: String,
        params: PaneSendInputParams,
    ) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some(runtime) = self.lookup_runtime_sender(ws_idx, pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let encoded_keys = match encode_api_keys(runtime, &params.keys) {
            Ok(encoded_keys) => encoded_keys,
            Err(key) => return encode_error(id, "invalid_key", format!("unsupported key {key}")),
        };
        if !params.text.is_empty() {
            let text_bytes = encode_api_text(runtime, &params.text);
            if let Err(err) = runtime.try_send_bytes(Bytes::from(text_bytes)) {
                return encode_error(id, "pane_send_failed", err.to_string());
            }
        }
        for bytes in encoded_keys {
            if let Err(err) = runtime.try_send_bytes(Bytes::from(bytes)) {
                return encode_error(id, "pane_send_failed", err.to_string());
            }
        }

        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_pane_close(&mut self, id: String, target: PaneTarget) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&target.pane_id) else {
            return pane_not_found(id, &target.pane_id);
        };
        // Capture the canonical public pane id BEFORE close (closing drops
        // the public-number mapping, so emitting the event afterwards using
        // the request's id would echo a legacy form for new-style callers).
        let Some(public_pane_id) = self.public_pane_id(ws_idx, pane_id) else {
            return pane_not_found(id, &target.pane_id);
        };
        if self.state.close_pane_would_close_workspace(ws_idx, pane_id)
            && self.state.confirm_implicit_worktree_group_close(ws_idx)
        {
            return encode_error(
                id,
                "confirmation_required",
                "closing this pane would close a worktree group",
            );
        }
        let workspace_id = self.state.workspaces[ws_idx].id.clone();
        let terminal_id = self.state.terminal_id_for_pane(ws_idx, pane_id);
        let should_close_workspace = {
            let Some(ws) = self.state.workspaces.get_mut(ws_idx) else {
                return pane_not_found(id, &target.pane_id);
            };
            ws.close_pane(pane_id)
        };
        if should_close_workspace {
            self.state.selected = ws_idx;
            self.state.close_selected_workspace();
            self.shutdown_detached_terminal_runtimes();
            self.emit_event(EventEnvelope {
                event: EventKind::PaneClosed,
                data: EventData::PaneClosed {
                    pane_id: public_pane_id,
                    workspace_id: workspace_id.clone(),
                },
            });
            self.emit_event(EventEnvelope {
                event: EventKind::WorkspaceClosed,
                data: EventData::WorkspaceClosed { workspace_id },
            });
        } else {
            self.state.remove_unattached_terminal_ids(terminal_id);
            self.shutdown_detached_terminal_runtimes();
            self.schedule_session_save();
            self.emit_event(EventEnvelope {
                event: EventKind::PaneClosed,
                data: EventData::PaneClosed {
                    pane_id: public_pane_id,
                    workspace_id,
                },
            });
        }

        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_pane_send_keys(
        &mut self,
        id: String,
        params: PaneSendKeysParams,
    ) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some(runtime) = self.lookup_runtime_sender(ws_idx, pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let encoded_keys = match encode_api_keys(runtime, &params.keys) {
            Ok(encoded_keys) => encoded_keys,
            Err(key) => return encode_error(id, "invalid_key", format!("unsupported key {key}")),
        };
        for bytes in encoded_keys {
            if let Err(err) = runtime.try_send_bytes(Bytes::from(bytes)) {
                return encode_error(id, "pane_send_failed", err.to_string());
            }
        }

        encode_success(id, ResponseResult::Ok {})
    }
}

enum ResolvedPaneMoveDestination {
    ExistingTab {
        tab_id: String,
        target_pane_id: crate::layout::PaneId,
        split: crate::api::schema::SplitDirection,
        ratio: f32,
        cross_workspace: bool,
    },
    NewTab {
        workspace_id: String,
        label: Option<String>,
    },
    NewWorkspace {
        label: Option<String>,
        tab_label: Option<String>,
    },
}

struct PaneMoveRecoveryContext {
    source_ws_idx: usize,
    previous_workspace_id: String,
    previous_workspace_label: Option<String>,
    previous_tab_label: Option<String>,
    previous_worktree_space: Option<crate::workspace::WorktreeSpaceMembership>,
    identity_cwd: std::path::PathBuf,
}

fn split_direction_to_layout(
    direction: crate::api::schema::SplitDirection,
) -> ratatui::layout::Direction {
    match direction {
        crate::api::schema::SplitDirection::Right => ratatui::layout::Direction::Horizontal,
        crate::api::schema::SplitDirection::Down => ratatui::layout::Direction::Vertical,
    }
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    let value = value?.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn normalize_presentation_text(value: Option<String>) -> Option<String> {
    let trimmed = value?.trim().to_string();
    let normalized: String = trimmed
        .chars()
        .filter(|ch| !ch.is_control())
        .take(80)
        .collect();
    (!normalized.trim().is_empty()).then(|| normalized.trim().to_string())
}

fn normalize_state_labels(
    labels: std::collections::HashMap<String, String>,
) -> Result<std::collections::HashMap<String, String>, String> {
    labels
        .into_iter()
        .map(|(status, label)| {
            let status = status.trim().to_ascii_lowercase();
            if !matches!(
                status.as_str(),
                "idle" | "working" | "blocked" | "done" | "unknown"
            ) {
                return Err(status);
            }
            Ok(normalize_presentation_text(Some(label)).map(|label| (status, label)))
        })
        .filter_map(Result::transpose)
        .collect()
}

fn pane_not_found(id: String, pane_id: &str) -> String {
    encode_error(id, "pane_not_found", format!("pane {pane_id} not found"))
}

fn invalid_agent(id: String) -> String {
    encode_error(id, "invalid_agent", "agent label must not be empty")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{api::schema::SuccessResponse, config::Config, workspace::Workspace};

    fn app_with_linked_worktree() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("issue")];
        app.state.workspaces[0].worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "flock".into(),
            repo_root: "/repo/flock".into(),
            checkout_path: "/repo/flock-issue".into(),
            is_linked_worktree: true,
        });
        app
    }

    #[test]
    fn api_pane_close_closes_linked_worktree_workspace_only() {
        let mut app = app_with_linked_worktree();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();

        let response = app.handle_pane_close(
            "req".into(),
            PaneTarget {
                pane_id: public_pane_id,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(success.id, "req");
        assert_eq!(app.state.request_remove_linked_worktree, None);
        assert!(app.state.workspaces.is_empty());
    }
    /// App with one workspace whose root pane has a live TerminalState.
    /// Returns the app, the terminal id, and the public pane id.
    fn app_with_terminal() -> (App, crate::terminal::TerminalId, String) {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("main")];
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0]
            .pane_state(pane_id)
            .expect("pane state")
            .attached_terminal_id
            .clone();
        app.state.terminals.insert(
            terminal_id.clone(),
            crate::terminal::TerminalState::new(terminal_id.clone(), "/tmp".into()),
        );
        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();
        (app, terminal_id, public_pane_id)
    }

    fn set_field_response(app: &mut App, pane_id: &str, key: &str, value: &str) -> String {
        app.handle_pane_set_header_field(
            "req".into(),
            crate::api::schema::PaneSetHeaderFieldParams {
                pane_id: pane_id.to_string(),
                key: key.to_string(),
                value: value.to_string(),
                ttl_secs: None,
            },
        )
    }

    #[test]
    fn set_and_clear_header_field_round_trip_through_update_terminal_state() {
        let (mut app, terminal_id, public_pane_id) = app_with_terminal();

        assert!(set_field_response(&mut app, &public_pane_id, "build", "73%").contains("\"ok\""));
        assert!(set_field_response(&mut app, &public_pane_id, "pg", "up").contains("\"ok\""));
        assert_eq!(
            app.state
                .terminals
                .get(&terminal_id)
                .unwrap()
                .active_header_fields(),
            vec![
                ("build".to_string(), "73%".to_string()),
                ("pg".to_string(), "up".to_string()),
            ]
        );

        let response = app.handle_pane_clear_header_field(
            "req-clear".into(),
            crate::api::schema::PaneClearHeaderFieldParams {
                pane_id: public_pane_id,
                key: "build".into(),
            },
        );
        assert!(response.contains("\"ok\""));
        assert_eq!(
            app.state
                .terminals
                .get(&terminal_id)
                .unwrap()
                .active_header_fields(),
            vec![("pg".to_string(), "up".to_string())]
        );
    }

    #[test]
    fn set_header_field_rejects_over_cap_requests() {
        let (mut app, terminal_id, public_pane_id) = app_with_terminal();

        // Fill the per-pane cap (6 fields).
        for i in 0..6 {
            assert!(
                set_field_response(&mut app, &public_pane_id, &format!("k{i}"), "v")
                    .contains("\"ok\"")
            );
        }
        let response = set_field_response(&mut app, &public_pane_id, "k6", "v");
        assert!(response.contains("too_many_header_fields"));
        // Updating an existing key is still allowed at the cap.
        assert!(set_field_response(&mut app, &public_pane_id, "k0", "v2").contains("\"ok\""));

        // Key/value length caps reject via RPC error.
        let response = set_field_response(&mut app, &public_pane_id, &"k".repeat(17), "v");
        assert!(response.contains("invalid_header_field"));
        let response = set_field_response(&mut app, &public_pane_id, "k", &"v".repeat(49));
        assert!(response.contains("invalid_header_field"));
        let response = set_field_response(&mut app, &public_pane_id, "  ", "v");
        assert!(response.contains("invalid_header_field"));

        assert_eq!(
            app.state
                .terminals
                .get(&terminal_id)
                .unwrap()
                .active_header_fields()
                .len(),
            6
        );
    }

    #[test]
    fn set_header_field_with_ttl_arms_the_shared_expiry_deadline() {
        let (mut app, terminal_id, public_pane_id) = app_with_terminal();
        assert_eq!(app.agent_metadata_deadline, None);

        let response = app.handle_pane_set_header_field(
            "req".into(),
            crate::api::schema::PaneSetHeaderFieldParams {
                pane_id: public_pane_id,
                key: "build".into(),
                value: "73%".into(),
                ttl_secs: Some(30),
            },
        );
        assert!(response.contains("\"ok\""));

        // The TTL rides the same scheduled tick that expires agent metadata,
        // in both event loops.
        let deadline = app.agent_metadata_deadline.expect("deadline armed");
        let now = std::time::Instant::now();
        assert!(deadline > now + std::time::Duration::from_secs(25));
        assert!(deadline <= now + std::time::Duration::from_secs(30));

        // Firing the shared sweep past the deadline drops the chip.
        let updates = app
            .state
            .expire_agent_metadata_at(deadline, deadline + std::time::Duration::from_millis(1));
        assert!(updates.is_empty());
        assert!(app
            .state
            .terminals
            .get(&terminal_id)
            .unwrap()
            .active_header_fields()
            .is_empty());
        assert_eq!(app.state.next_agent_metadata_expiry(), None);
    }

    #[test]
    fn header_field_requests_for_unknown_panes_fail() {
        let (mut app, _terminal_id, _public_pane_id) = app_with_terminal();
        let response = set_field_response(&mut app, "w_99-1", "build", "73%");
        assert!(response.contains("pane_not_found"));
    }

    #[test]
    fn report_prompt_appends_timestamped_history_entry() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("main")];
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0]
            .pane_state(pane_id)
            .expect("pane state")
            .attached_terminal_id
            .clone();
        app.state.terminals.insert(
            terminal_id.clone(),
            crate::terminal::TerminalState::new(terminal_id.clone(), "/tmp".into()),
        );
        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();

        for prompt in ["first prompt", "second prompt", "third prompt"] {
            app.handle_pane_report_prompt(
                "rid".into(),
                crate::api::schema::PaneReportPromptParams {
                    pane_id: public_pane_id.clone(),
                    source: "flock:claude".into(),
                    agent: "claude".into(),
                    prompt: prompt.into(),
                    seq: None,
                },
            );
        }
        let history = &app
            .state
            .terminals
            .get(&terminal_id)
            .unwrap()
            .prompt_history;
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].text, "first prompt");
        assert_eq!(history[2].text, "third prompt");
        assert!(history
            .iter()
            .all(|e| e.kind == crate::terminal::PromptHistoryKind::Prompt));
        // The legacy collapsed-header field still mirrors the latest.
        assert_eq!(
            app.state
                .terminals
                .get(&terminal_id)
                .and_then(|t| t.last_prompt.as_deref()),
            Some("third prompt")
        );
    }

    #[test]
    fn report_recap_round_trips_through_update_terminal_state() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("main")];
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0]
            .pane_state(pane_id)
            .expect("pane state")
            .attached_terminal_id
            .clone();
        app.state.terminals.insert(
            terminal_id.clone(),
            crate::terminal::TerminalState::new(terminal_id.clone(), "/tmp".into()),
        );
        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();

        // A prompt, then a recap — the recap survives visually distinct and
        // does NOT touch `last_prompt`.
        app.handle_pane_report_prompt(
            "rid".into(),
            crate::api::schema::PaneReportPromptParams {
                pane_id: public_pane_id.clone(),
                source: "flock:claude".into(),
                agent: "claude".into(),
                prompt: "fix the bug".into(),
                seq: None,
            },
        );
        let response = app.handle_pane_report_recap(
            "rid-recap".into(),
            crate::api::schema::PaneReportRecapParams {
                pane_id: public_pane_id.clone(),
                source: "flock:claude".into(),
                agent: "claude".into(),
                recap: "fixed the parser bug".into(),
                seq: None,
            },
        );
        assert!(response.contains("\"ok\""));

        let history = &app
            .state
            .terminals
            .get(&terminal_id)
            .unwrap()
            .prompt_history;
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].kind, crate::terminal::PromptHistoryKind::Prompt);
        assert_eq!(history[1].kind, crate::terminal::PromptHistoryKind::Recap);
        assert_eq!(history[1].text, "fixed the parser bug");
        // Recap does not update last_prompt — collapsed header still shows
        // the latest USER prompt.
        assert_eq!(
            app.state
                .terminals
                .get(&terminal_id)
                .and_then(|t| t.last_prompt.as_deref()),
            Some("fix the bug")
        );
    }

    #[test]
    fn report_reply_round_trips_through_update_terminal_state() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("main")];
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0]
            .pane_state(pane_id)
            .expect("pane state")
            .attached_terminal_id
            .clone();
        app.state.terminals.insert(
            terminal_id.clone(),
            crate::terminal::TerminalState::new(terminal_id.clone(), "/tmp".into()),
        );
        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();

        // The full conversation shape: user prompt, agent reply, recap.
        // All three land in history as distinct kinds; only the prompt
        // updates `last_prompt`.
        app.handle_pane_report_prompt(
            "rid-p".into(),
            crate::api::schema::PaneReportPromptParams {
                pane_id: public_pane_id.clone(),
                source: "flock:claude".into(),
                agent: "claude".into(),
                prompt: "fix the bug".into(),
                seq: None,
            },
        );
        let reply_resp = app.handle_pane_report_reply(
            "rid-r".into(),
            crate::api::schema::PaneReportReplyParams {
                pane_id: public_pane_id.clone(),
                source: "flock:claude".into(),
                agent: "claude".into(),
                reply: "I'll fix the parser at line 42.".into(),
                seq: None,
            },
        );
        assert!(reply_resp.contains("\"ok\""));
        app.handle_pane_report_recap(
            "rid-rc".into(),
            crate::api::schema::PaneReportRecapParams {
                pane_id: public_pane_id.clone(),
                source: "flock:claude".into(),
                agent: "claude".into(),
                recap: "\u{203b} recap: parser bug fixed. Next: ship PR.".into(),
                seq: None,
            },
        );

        let history = &app
            .state
            .terminals
            .get(&terminal_id)
            .unwrap()
            .prompt_history;
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].kind, crate::terminal::PromptHistoryKind::Prompt);
        assert_eq!(history[1].kind, crate::terminal::PromptHistoryKind::Reply);
        assert_eq!(history[2].kind, crate::terminal::PromptHistoryKind::Recap);
        assert_eq!(history[1].text, "I'll fix the parser at line 42.");
        assert_eq!(
            app.state
                .terminals
                .get(&terminal_id)
                .and_then(|t| t.last_prompt.as_deref()),
            Some("fix the bug"),
            "reply and recap do not touch last_prompt"
        );
    }

    #[test]
    fn report_reply_rejects_unknown_pane_and_invalid_agent() {
        let (mut app, _terminal_id, public_pane_id) = app_with_terminal();
        let response = app.handle_pane_report_reply(
            "rid".into(),
            crate::api::schema::PaneReportReplyParams {
                pane_id: "w_99-1".into(),
                source: "flock:claude".into(),
                agent: "claude".into(),
                reply: "nope".into(),
                seq: None,
            },
        );
        assert!(response.contains("pane_not_found"));

        let response = app.handle_pane_report_reply(
            "rid".into(),
            crate::api::schema::PaneReportReplyParams {
                pane_id: public_pane_id,
                source: "flock:claude".into(),
                agent: "   ".into(),
                reply: "nope".into(),
                seq: None,
            },
        );
        assert!(response.contains("invalid_agent"));
    }

    #[test]
    fn report_recap_rejects_unknown_pane_and_invalid_agent() {
        let (mut app, _terminal_id, public_pane_id) = app_with_terminal();
        let response = app.handle_pane_report_recap(
            "rid".into(),
            crate::api::schema::PaneReportRecapParams {
                pane_id: "w_99-1".into(),
                source: "flock:claude".into(),
                agent: "claude".into(),
                recap: "nope".into(),
                seq: None,
            },
        );
        assert!(response.contains("pane_not_found"));

        let response = app.handle_pane_report_recap(
            "rid".into(),
            crate::api::schema::PaneReportRecapParams {
                pane_id: public_pane_id,
                source: "flock:claude".into(),
                agent: "   ".into(),
                recap: "nope".into(),
                seq: None,
            },
        );
        assert!(response.contains("invalid_agent"));
    }

    #[test]
    fn prompt_history_drops_oldest_whole_entries_past_cap() {
        let (mut app, terminal_id, public_pane_id) = app_with_terminal();
        // Each entry: chrome + 50 body lines = 51 rendered lines.
        let big_body = (0..50)
            .map(|i| format!("body-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        for i in 0..25 {
            app.handle_pane_report_prompt(
                "rid".into(),
                crate::api::schema::PaneReportPromptParams {
                    pane_id: public_pane_id.clone(),
                    source: "flock:claude".into(),
                    agent: "claude".into(),
                    prompt: format!("entry-{i}\n{big_body}"),
                    seq: None,
                },
            );
        }
        let history = &app
            .state
            .terminals
            .get(&terminal_id)
            .unwrap()
            .prompt_history;
        let total: usize = history.iter().map(|e| e.rendered_line_count()).sum();
        assert!(total <= crate::terminal::state::MAX_PROMPT_HISTORY_LINES);
        assert!(history.last().unwrap().text.starts_with("entry-24\n"));
        // The earliest entries were dropped.
        assert!(history.iter().all(|e| !e.text.starts_with("entry-0\n")));
    }

    #[test]
    fn report_prompt_stores_sanitized_last_prompt() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("main")];
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0]
            .pane_state(pane_id)
            .expect("pane state")
            .attached_terminal_id
            .clone();
        app.state.terminals.insert(
            terminal_id.clone(),
            crate::terminal::TerminalState::new(terminal_id.clone(), "/tmp".into()),
        );
        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();

        let response = app.handle_pane_report_prompt(
            "req-1".into(),
            crate::api::schema::PaneReportPromptParams {
                pane_id: public_pane_id,
                source: "flock:claude".into(),
                agent: "claude".into(),
                prompt: "  fix the \u{1b}[31mparser\u{1b}[0m bug  ".into(),
                seq: Some(1),
            },
        );
        assert!(response.contains("\"ok\""));
        assert_eq!(
            app.state
                .terminals
                .get(&terminal_id)
                .and_then(|terminal| terminal.last_prompt.as_deref()),
            Some("fix the parser bug")
        );
    }

    // -- pane.move tests --

    /// Ensure each pane in the app's workspaces has a backing TerminalState
    /// registered, so `pane.move` can look up cwd for `NewWorkspace` and so
    /// `pane_info` is populated.
    fn seed_terminal_states(app: &mut App) {
        for ws in &app.state.workspaces {
            for tab in &ws.tabs {
                for pane in tab.panes.values() {
                    app.state
                        .terminals
                        .entry(pane.attached_terminal_id.clone())
                        .or_insert_with(|| {
                            crate::terminal::TerminalState::new(
                                pane.attached_terminal_id.clone(),
                                std::path::PathBuf::from("/flock-test"),
                            )
                        });
                }
            }
        }
    }

    /// Moving a pane into a different tab in the same workspace preserves the
    /// pane's terminal id (and the runtime registered under it), so the
    /// pane's PTY/process is NOT respawned across the move.
    #[test]
    fn api_pane_move_to_existing_tab_preserves_internal_pane_and_terminal() {
        let mut app = app_with_linked_worktree();
        let source = app.state.workspaces[0].tabs[0].root_pane;
        let source_terminal = app.state.workspaces[0].tabs[0]
            .terminal_id(source)
            .unwrap()
            .clone();
        let target_tab = app.state.workspaces[0].test_add_tab(Some("target"));
        let target = app.state.workspaces[0].tabs[target_tab].root_pane;
        seed_terminal_states(&mut app);
        let source_public = app.public_pane_id(0, source).unwrap();
        let source_tab_public = app.public_tab_id(0, 0).unwrap();
        let target_public = app.public_pane_id(0, target).unwrap();
        let target_tab_public = app.public_tab_id(0, target_tab).unwrap();

        let response = app.handle_pane_move(
            "req".into(),
            PaneMoveParams {
                pane_id: source_public.clone(),
                destination: PaneMoveDestination::Tab {
                    tab_id: target_tab_public.clone(),
                    target_pane_id: Some(target_public),
                    split: crate::api::schema::SplitDirection::Right,
                    ratio: Some(0.25),
                },
                focus: true,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::PaneMove { move_result } = success.result else {
            panic!("expected pane move response");
        };
        assert!(move_result.changed);
        assert_eq!(move_result.reason, None);
        assert_eq!(move_result.previous_pane_id, source_public);
        assert_eq!(move_result.previous_tab_id, source_tab_public);
        // Same workspace: stable public id stays the same.
        assert_eq!(move_result.pane.pane_id, move_result.previous_pane_id);
        assert_eq!(move_result.pane.tab_id, target_tab_public);
        // Terminal id is preserved — the PTY/runtime did NOT respawn.
        assert_eq!(move_result.pane.terminal_id, source_terminal.to_string());
        assert_eq!(move_result.closed_tab_id, Some(source_tab_public));
        assert_eq!(move_result.closed_workspace_id, None);
        assert_eq!(app.state.workspaces[0].tabs.len(), 1);
        assert_eq!(app.state.workspaces[0].tabs[0].layout.focused(), source);
        assert_eq!(
            app.state.workspaces[0].tabs[0].terminal_id(source),
            Some(&source_terminal)
        );
    }

    /// Cross-workspace move: under the fork's per-workspace stable numbering
    /// the pane gets a fresh public id under the new workspace, but the
    /// terminal id (and thus the PTY) is preserved and the source workspace
    /// is closed because it had no other panes.
    #[test]
    fn api_pane_move_to_existing_tab_across_workspace_preserves_pty_and_closes_source() {
        let mut app = app_with_linked_worktree();
        app.state.workspaces.push(Workspace::test_new("other"));
        let source = app.state.workspaces[0].tabs[0].root_pane;
        let source_terminal = app.state.workspaces[0].tabs[0]
            .terminal_id(source)
            .unwrap()
            .clone();
        let target = app.state.workspaces[1].tabs[0].root_pane;
        seed_terminal_states(&mut app);
        let previous_pane_id = app.public_pane_id(0, source).unwrap();
        let previous_workspace_id = app.public_workspace_id(0);
        let target_workspace_id = app.public_workspace_id(1);
        let target_tab_id = app.public_tab_id(1, 0).unwrap();
        let target_pane_id = app.public_pane_id(1, target).unwrap();

        let response = app.handle_pane_move(
            "req".into(),
            PaneMoveParams {
                pane_id: previous_pane_id.clone(),
                destination: PaneMoveDestination::Tab {
                    tab_id: target_tab_id.clone(),
                    target_pane_id: Some(target_pane_id),
                    split: crate::api::schema::SplitDirection::Down,
                    ratio: None,
                },
                focus: false,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::PaneMove { move_result } = success.result else {
            panic!("expected pane move response");
        };
        assert!(move_result.changed);
        assert_eq!(move_result.previous_pane_id, previous_pane_id);
        assert_eq!(move_result.previous_workspace_id, previous_workspace_id);
        // Empty source workspace gets closed.
        assert_eq!(move_result.closed_workspace_id, Some(previous_workspace_id));
        // Pane re-numbered under the destination workspace (fork's per-ws ids).
        assert_ne!(move_result.pane.pane_id, move_result.previous_pane_id);
        assert!(move_result
            .pane
            .pane_id
            .starts_with(&format!("{target_workspace_id}:p")));
        assert_eq!(move_result.pane.workspace_id, target_workspace_id);
        assert_eq!(move_result.pane.tab_id, target_tab_id);
        // PTY preserved: terminal id is unchanged.
        assert_eq!(move_result.pane.terminal_id, source_terminal.to_string());
        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(
            app.state.workspaces[0].tabs[0].terminal_id(source),
            Some(&source_terminal)
        );
    }

    /// `--new-tab` (same workspace) keeps the pane's terminal id and parks it
    /// under a new tab — useful for promoting a side-pane into a top-level tab
    /// without restarting its agent.
    #[test]
    fn api_pane_move_to_new_tab_creates_tab_without_spawning_terminal() {
        let mut app = app_with_linked_worktree();
        let source = app.state.workspaces[0].tabs[0].root_pane;
        let right = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
        let source_terminal = app.state.workspaces[0].tabs[0]
            .terminal_id(source)
            .unwrap()
            .clone();
        seed_terminal_states(&mut app);
        let source_public = app.public_pane_id(0, source).unwrap();

        let response = app.handle_pane_move(
            "req".into(),
            PaneMoveParams {
                pane_id: source_public.clone(),
                destination: PaneMoveDestination::NewTab {
                    workspace_id: None,
                    label: Some("moved".into()),
                },
                focus: true,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::PaneMove { move_result } = success.result else {
            panic!("expected pane move response");
        };
        assert!(move_result.changed);
        assert_eq!(
            move_result
                .created_tab
                .as_ref()
                .map(|tab| tab.label.as_str()),
            Some("moved")
        );
        assert_eq!(move_result.closed_tab_id, None);
        assert_eq!(move_result.pane.pane_id, source_public);
        // PTY preserved.
        assert_eq!(move_result.pane.terminal_id, source_terminal.to_string());
        assert_eq!(app.state.workspaces[0].tabs.len(), 2);
        assert!(app.state.workspaces[0].tabs[0].terminal_id(right).is_some());
        assert_eq!(
            app.state.workspaces[0].tabs[1].terminal_id(source),
            Some(&source_terminal)
        );
    }

    /// `--new-workspace` lifts a pane out into a brand-new workspace; the
    /// pane's PTY keeps running (same terminal id) and the now-empty source
    /// workspace is closed.
    #[test]
    fn api_pane_move_to_new_workspace_closes_empty_source_workspace() {
        let mut app = app_with_linked_worktree();
        let source = app.state.workspaces[0].tabs[0].root_pane;
        let source_terminal = app.state.workspaces[0].tabs[0]
            .terminal_id(source)
            .unwrap()
            .clone();
        seed_terminal_states(&mut app);
        let source_public = app.public_pane_id(0, source).unwrap();
        let source_workspace = app.public_workspace_id(0);

        let response = app.handle_pane_move(
            "req".into(),
            PaneMoveParams {
                pane_id: source_public.clone(),
                destination: PaneMoveDestination::NewWorkspace {
                    label: Some("promoted".into()),
                    tab_label: Some("main".into()),
                },
                focus: true,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::PaneMove { move_result } = success.result else {
            panic!("expected pane move response");
        };
        assert!(move_result.changed);
        assert_eq!(move_result.closed_workspace_id, Some(source_workspace));
        assert_eq!(
            move_result
                .created_workspace
                .as_ref()
                .map(|ws| ws.label.as_str()),
            Some("promoted")
        );
        assert_eq!(
            move_result
                .created_tab
                .as_ref()
                .map(|tab| tab.label.as_str()),
            Some("main")
        );
        // Public pane id is rescoped into the new workspace.
        assert_ne!(move_result.pane.pane_id, source_public);
        // PTY preserved across workspace promotion.
        assert_eq!(move_result.pane.terminal_id, source_terminal.to_string());
        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(
            app.state.workspaces[0].tabs[0].terminal_id(source),
            Some(&source_terminal)
        );
    }

    /// Moving a pane into the tab it already lives in is a no-op carrying
    /// `changed=false` and `reason=SameTab`. The layout and pane count stay
    /// exactly as they were.
    #[test]
    fn api_pane_move_same_tab_returns_same_tab_noop() {
        let mut app = app_with_linked_worktree();
        let source = app.state.workspaces[0].tabs[0].root_pane;
        seed_terminal_states(&mut app);
        let source_public = app.public_pane_id(0, source).unwrap();
        let source_tab = app.public_tab_id(0, 0).unwrap();

        let response = app.handle_pane_move(
            "req".into(),
            PaneMoveParams {
                pane_id: source_public,
                destination: PaneMoveDestination::Tab {
                    tab_id: source_tab,
                    target_pane_id: None,
                    split: crate::api::schema::SplitDirection::Right,
                    ratio: None,
                },
                focus: true,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::PaneMove { move_result } = success.result else {
            panic!("expected pane move response");
        };
        assert!(!move_result.changed);
        assert_eq!(move_result.reason, Some(PaneMoveReason::SameTab));
        assert_eq!(app.state.workspaces[0].tabs.len(), 1);
    }

    /// `--target-pane` must live in the same tab as `--tab`; otherwise the
    /// request is rejected without disturbing any layout.
    #[test]
    fn api_pane_move_rejects_target_pane_outside_target_tab() {
        let mut app = app_with_linked_worktree();
        let source = app.state.workspaces[0].tabs[0].root_pane;
        let target_tab = app.state.workspaces[0].test_add_tab(Some("target"));
        let other_tab = app.state.workspaces[0].test_add_tab(Some("other"));
        seed_terminal_states(&mut app);
        let source_public = app.public_pane_id(0, source).unwrap();
        let target_tab_public = app.public_tab_id(0, target_tab).unwrap();
        let wrong_target = app
            .public_pane_id(0, app.state.workspaces[0].tabs[other_tab].root_pane)
            .unwrap();

        let response = app.handle_pane_move(
            "req".into(),
            PaneMoveParams {
                pane_id: source_public,
                destination: PaneMoveDestination::Tab {
                    tab_id: target_tab_public,
                    target_pane_id: Some(wrong_target),
                    split: crate::api::schema::SplitDirection::Right,
                    ratio: None,
                },
                focus: true,
            },
        );

        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "target_pane_not_found");
        // Layout untouched: source pane stays put.
        assert_eq!(app.state.workspaces[0].tabs.len(), 3);
        assert!(app.state.workspaces[0].tabs[0]
            .terminal_id(source)
            .is_some());
    }
}
