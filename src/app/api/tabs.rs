use std::path::PathBuf;

use crate::api::schema::{
    EventData, EventEnvelope, EventKind, ResponseResult, TabCreateParams, TabListParams,
    TabRenameParams, TabTarget,
};
use crate::app::{App, Mode};

use super::responses::{encode_error, encode_success};

impl App {
    pub(super) fn handle_tab_list(&mut self, id: String, params: TabListParams) -> String {
        let tabs = if let Some(workspace_id) = params.workspace_id {
            let Some(ws_idx) = self.parse_workspace_id(&workspace_id) else {
                return workspace_not_found(id, &workspace_id);
            };
            let Some(ws) = self.state.workspaces.get(ws_idx) else {
                return workspace_not_found(id, &workspace_id);
            };
            (0..ws.tabs.len())
                .filter_map(|tab_idx| self.tab_info(ws_idx, tab_idx))
                .collect()
        } else {
            let mut tabs = Vec::new();
            for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
                for tab_idx in 0..ws.tabs.len() {
                    if let Some(tab) = self.tab_info(ws_idx, tab_idx) {
                        tabs.push(tab);
                    }
                }
            }
            tabs
        };

        encode_success(id, ResponseResult::TabList { tabs })
    }

    pub(super) fn handle_tab_get(&mut self, id: String, target: TabTarget) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&target.tab_id) else {
            return tab_not_found(id, &target.tab_id);
        };
        let Some(tab) = self.tab_info(ws_idx, tab_idx) else {
            return tab_not_found(id, &target.tab_id);
        };

        encode_success(id, ResponseResult::TabInfo { tab })
    }

    /// `tab.create` under workspace tab mode: spawn the sibling workspace the
    /// keyboard would have, then answer with ITS root tab. The caller asked for
    /// a tab and gets a real tab id — it simply lives in the sibling, which is
    /// exactly what the human sees happen.
    fn create_sibling_workspace_for_api(
        &mut self,
        id: String,
        source_ws_idx: usize,
        cwd: Option<String>,
        label: Option<String>,
        focus: bool,
    ) -> String {
        let cwd_override = cwd.map(PathBuf::from);
        let ws_idx = match self.create_sibling_workspace_from(
            Some(source_ws_idx),
            label,
            cwd_override,
            focus,
        ) {
            Ok(ws_idx) => ws_idx,
            Err(err) => return encode_error(id, "tab_create_failed", err.to_string()),
        };
        if focus {
            self.state.mode = Mode::Terminal;
        }
        self.emit_workspace_open_events(ws_idx);
        match self.tab_created_result(ws_idx, 0) {
            Some(result) => encode_success(id, result),
            None => encode_error(
                id,
                "tab_create_failed",
                "sibling workspace produced no root tab",
            ),
        }
    }

    pub(super) fn handle_tab_create(&mut self, id: String, params: TabCreateParams) -> String {
        let TabCreateParams {
            workspace_id,
            cwd,
            focus,
            label,
        } = params;
        let ws_idx = if let Some(workspace_id) = workspace_id {
            let Some(ws_idx) = self.parse_workspace_id(&workspace_id) else {
                return workspace_not_found(id, &workspace_id);
            };
            ws_idx
        } else if let Some(active) = self.state.active {
            active
        } else {
            return encode_error(id, "workspace_not_found", "no active workspace");
        };
        // Workspace-as-unit tab mode (#25): a "tab" IS a sibling workspace.
        // The keyboard path branches here; this one did not, so an agent
        // calling tab.create grew an inner tab that the workspace-mode strip
        // never shows and that carries none of the space grouping.
        if self.state.tab_mode == crate::config::TabModeConfig::Workspace {
            return self.create_sibling_workspace_for_api(id, ws_idx, cwd, label, focus);
        }
        let cwd = cwd.map(PathBuf::from).unwrap_or_else(|| {
            let follow_cwd = self
                .state
                .focused_runtime_in_workspace(&self.terminal_runtimes, ws_idx)
                .and_then(|rt| rt.cwd());
            self.resolve_new_terminal_cwd(follow_cwd)
        });
        let (rows, cols) = self.state.estimate_pane_size();
        let default_shell = self.state.default_shell.clone();
        let scrollback_limit_bytes = self.state.pane_scrollback_limit_bytes;
        let host_terminal_theme = self.state.host_terminal_theme;
        let result = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .ok_or_else(|| std::io::Error::other("workspace disappeared"))
            .and_then(|ws| {
                ws.create_tab(
                    rows,
                    cols,
                    cwd,
                    scrollback_limit_bytes,
                    host_terminal_theme,
                    crate::pane::PaneShellConfig::new(&default_shell, self.state.shell_mode),
                )
            });
        match result {
            Ok((tab_idx, terminal, runtime)) => {
                self.terminal_runtimes.insert(terminal.id.clone(), runtime);
                self.state.terminals.insert(terminal.id.clone(), terminal);
                self.state.remove_alias_shadowed_by_new_pane(
                    self.state.workspaces[ws_idx].tabs[tab_idx].root_pane,
                );
                if let Some(label) = label {
                    let workspace_id = self.state.workspaces[ws_idx].id.clone();
                    let tab_id = self.public_tab_id(ws_idx, tab_idx).unwrap_or_else(|| {
                        crate::workspace::public_tab_id_for_number(&workspace_id, tab_idx + 1)
                    });
                    if let Some(tab) = self
                        .state
                        .workspaces
                        .get_mut(ws_idx)
                        .and_then(|ws| ws.tabs.get_mut(tab_idx))
                    {
                        tab.set_custom_name(label);
                        crate::logging::tab_renamed(&workspace_id, &tab_id);
                    }
                }
                if focus {
                    self.state.switch_workspace_tab(ws_idx, tab_idx);
                    self.state.mode = Mode::Terminal;
                }
                self.schedule_session_save();
                let tab = self.tab_info(ws_idx, tab_idx).unwrap();
                let root_pane = self
                    .root_pane_info(ws_idx, tab_idx)
                    .expect("new tab should have a root pane");
                self.emit_event(EventEnvelope {
                    event: EventKind::TabCreated,
                    data: EventData::TabCreated { tab: tab.clone() },
                });
                self.emit_event(EventEnvelope {
                    event: EventKind::PaneCreated,
                    data: EventData::PaneCreated {
                        pane: root_pane.clone(),
                    },
                });
                encode_success(
                    id,
                    self.tab_created_result(ws_idx, tab_idx)
                        .expect("new tab should produce a complete create response"),
                )
            }
            Err(err) => encode_error(id, "tab_create_failed", err.to_string()),
        }
    }

    pub(super) fn handle_tab_focus(&mut self, id: String, target: TabTarget) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&target.tab_id) else {
            return tab_not_found(id, &target.tab_id);
        };
        self.state.switch_workspace_tab(ws_idx, tab_idx);
        let tab = self.tab_info(ws_idx, tab_idx).unwrap();

        encode_success(id, ResponseResult::TabInfo { tab })
    }

    pub(super) fn handle_tab_rename(&mut self, id: String, params: TabRenameParams) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&params.tab_id) else {
            return tab_not_found(id, &params.tab_id);
        };
        let workspace_id = self.state.workspaces[ws_idx].id.clone();
        let tab_id = self.public_tab_id(ws_idx, tab_idx).unwrap_or_else(|| {
            crate::workspace::public_tab_id_for_number(&workspace_id, tab_idx + 1)
        });
        let Some(tab) = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .and_then(|ws| ws.tabs.get_mut(tab_idx))
        else {
            return tab_not_found(id, &params.tab_id);
        };
        tab.set_custom_name(params.label.clone());
        crate::logging::tab_renamed(&workspace_id, &tab_id);
        self.schedule_session_save();
        self.emit_event(EventEnvelope {
            event: EventKind::TabRenamed,
            data: EventData::TabRenamed {
                tab_id: self.public_tab_id(ws_idx, tab_idx).unwrap(),
                workspace_id: self.public_workspace_id(ws_idx),
                label: params.label,
            },
        });
        let tab = self.tab_info(ws_idx, tab_idx).unwrap();

        encode_success(id, ResponseResult::TabInfo { tab })
    }

    pub(super) fn handle_tab_close(&mut self, id: String, target: TabTarget) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&target.tab_id) else {
            return tab_not_found(id, &target.tab_id);
        };
        // Resolve to the canonical public id BEFORE closing, so the
        // tab_closed event always carries the new-style id even when the
        // request used the legacy `:N` form.
        let Some(tab_id) = self.public_tab_id(ws_idx, tab_idx) else {
            return tab_not_found(id, &target.tab_id);
        };
        let workspace_id = self.public_workspace_id(ws_idx);
        let terminal_ids = self.state.terminal_ids_for_tab(ws_idx, tab_idx);
        let Some(ws) = self.state.workspaces.get_mut(ws_idx) else {
            return tab_not_found(id, &target.tab_id);
        };
        if ws.tabs.len() <= 1 {
            return encode_error(
                id,
                "tab_close_failed",
                "cannot close the last tab in a workspace",
            );
        }
        if !ws.close_tab(tab_idx) {
            return encode_error(
                id,
                "tab_close_failed",
                format!("tab {} could not be closed", target.tab_id),
            );
        }
        self.state.remove_unattached_terminal_ids(terminal_ids);
        self.shutdown_detached_terminal_runtimes();
        self.schedule_session_save();
        self.emit_event(EventEnvelope {
            event: EventKind::TabClosed,
            data: EventData::TabClosed {
                tab_id,
                workspace_id,
            },
        });

        encode_success(id, ResponseResult::Ok {})
    }
}

fn workspace_not_found(id: String, workspace_id: &str) -> String {
    encode_error(
        id,
        "workspace_not_found",
        format!("workspace {workspace_id} not found"),
    )
}

fn tab_not_found(id: String, tab_id: &str) -> String {
    encode_error(id, "tab_not_found", format!("tab {tab_id} not found"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::Request;

    fn test_app() -> App {
        App::new(
            &crate::config::Config::default(),
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        )
    }

    /// A "tab" under workspace tab mode (#25) is a sibling workspace. The
    /// keyboard has branched on this since #25; `tab.create` did not, so an
    /// agent asking for a tab grew an inner tab the workspace-mode strip never
    /// shows, carrying none of the space grouping the sibling would inherit.
    #[tokio::test]
    async fn tab_create_spawns_a_sibling_workspace_under_workspace_tab_mode() {
        let mut app = test_app();
        app.state.tab_mode = crate::config::TabModeConfig::Workspace;
        let mut ws = crate::workspace::Workspace::test_new("main");
        ws.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "/repo/flock/.git".into(),
            label: "flock".into(),
            repo_root: "/repo/flock".into(),
            checkout_path: "/repo/flock".into(),
            is_linked_worktree: false,
        });
        let source_id = ws.id.clone();
        app.state.workspaces = vec![ws];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: crate::api::schema::Method::TabCreate(TabCreateParams {
                workspace_id: Some(source_id.clone()),
                cwd: None,
                focus: false,
                label: None,
            }),
        });
        assert!(
            !response.contains("\"error\""),
            "tab.create should succeed: {response}"
        );

        // A sibling workspace, not an inner tab.
        assert_eq!(app.state.workspaces.len(), 2);
        assert_eq!(app.state.workspaces[0].tabs.len(), 1);
        assert_eq!(app.state.workspaces[1].tabs.len(), 1);

        // And it inherits the space grouping, which is what makes it a sibling
        // rather than an unrelated workspace.
        assert_eq!(
            app.state.workspaces[1]
                .worktree_space()
                .map(|space| space.key.as_str()),
            Some("/repo/flock/.git")
        );
    }

    /// The default mode is unchanged: a tab is still a tab.
    #[tokio::test]
    async fn tab_create_still_grows_an_inner_tab_under_tabs_mode() {
        let mut app = test_app();
        app.state.tab_mode = crate::config::TabModeConfig::Tabs;
        let ws = crate::workspace::Workspace::test_new("main");
        let source_id = ws.id.clone();
        app.state.workspaces = vec![ws];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: crate::api::schema::Method::TabCreate(TabCreateParams {
                workspace_id: Some(source_id),
                cwd: None,
                focus: false,
                label: None,
            }),
        });
        assert!(!response.contains("\"error\""), "{response}");
        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(app.state.workspaces[0].tabs.len(), 2);
    }
}
