use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::api::schema::{
    AgentForkParams, EventData, EventEnvelope, EventKind, ResponseResult, WorktreeCreateParams,
    WorktreeInfo, WorktreeListParams, WorktreeOpenParams, WorktreeRemoveParams, WorktreeSourceInfo,
};
use crate::app::App;

use super::responses::{encode_error, encode_error_body, encode_success};

struct ApiFailure {
    code: &'static str,
    message: String,
}

impl ApiFailure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// #175 S3 commit 4 (ops): mint a unique run_id for an `agent.fork`
/// child. Format: `fork:<unix_us>-<pid>-<counter>` — cheap, monotonic
/// within a process, and never colliding across a restart because unix
/// micros + a fresh pid + a fresh counter never re-hit the same tuple.
/// Callers stamp this on the child's env and reuse it for the
/// `AgentForked.run_id` field so the two agree.
///
/// EXTENSION POINT: when a scheduler-spawned path lands (S1 `SpawnAgent`
/// action), reuse this function to mint the run_id there too — the
/// trailer hook already keys on `FLOCK_RUN_ID`, so the same env stamp
/// makes those commits revertable by `flk revert-run`.
fn generated_fork_run_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0);
    format!("fork:{micros:x}-{}-{count}", std::process::id())
}

fn absolute_user_path(path: &str) -> Result<PathBuf, ApiFailure> {
    let path = crate::worktree::expand_tilde_path(path);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(ApiFailure::new(
            "invalid_request",
            "worktree path must be absolute",
        ))
    }
}

struct WorktreeSource {
    workspace_idx: Option<usize>,
    source_checkout_path: PathBuf,
    source_repo_root: PathBuf,
    repo_key: String,
    repo_name: String,
}

impl App {
    pub(super) fn handle_worktree_list(
        &mut self,
        id: String,
        params: WorktreeListParams,
    ) -> String {
        let source = match self.resolve_worktree_list_source(params.workspace_id, params.cwd) {
            Ok(source) => source,
            Err(err) => return encode_error(id, err.code, err.message),
        };
        let entries = match crate::worktree::list_existing_worktrees(&source.source_repo_root) {
            Ok(entries) => entries,
            Err(err) => return encode_error(id, "worktree_list_failed", err),
        };
        let worktrees = entries
            .into_iter()
            .map(|entry| self.worktree_info_for_entry(&source, entry))
            .collect();

        encode_success(
            id,
            ResponseResult::WorktreeList {
                source: self.worktree_source_info(&source),
                worktrees,
            },
        )
    }

    pub(super) fn handle_worktree_create(
        &mut self,
        id: String,
        params: WorktreeCreateParams,
    ) -> String {
        let branch = params
            .branch
            .unwrap_or_else(|| {
                let seed = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|duration| duration.as_micros().min(u128::from(u64::MAX)) as u64)
                    .unwrap_or(0);
                crate::worktree::generated_branch_slug(seed)
            })
            .trim()
            .to_string();
        if branch.is_empty() {
            return encode_error(id, "invalid_request", "branch is required");
        }
        let base = params.base.unwrap_or_else(|| "HEAD".into());
        let mut source = match self.resolve_worktree_source(params.workspace_id, params.cwd) {
            Ok(source) => source,
            Err(err) => return encode_error(id, err.code, err.message),
        };
        let checkout_path = match params.path {
            Some(path) => match absolute_user_path(&path) {
                Ok(path) => path,
                Err(err) => return encode_error(id, err.code, err.message),
            },
            None => crate::worktree::default_checkout_path(
                &self.state.worktree_directory,
                &source.repo_name,
                &branch,
            ),
        };

        if let Some(parent_dir) = checkout_path.parent() {
            if let Err(err) = std::fs::create_dir_all(parent_dir) {
                return encode_error(id, "worktree_create_failed", err.to_string());
            }
        }

        let command = crate::worktree::build_worktree_add_new_branch_command(
            &source.source_checkout_path,
            &checkout_path,
            &branch,
            &base,
        );
        if let Err(err) = crate::worktree::run_worktree_command(&command) {
            return encode_error(
                id,
                "worktree_create_failed",
                crate::worktree::explain_worktree_add_failure(&base, &err),
            );
        }
        if let Err(err) = self.ensure_source_parent_membership(&mut source, true) {
            return encode_error(id, err.code, err.message);
        }

        let ws_idx = match self.create_workspace_with_options(checkout_path.clone(), params.focus) {
            Ok(ws_idx) => ws_idx,
            Err(err) => {
                return encode_error(
                    id,
                    "worktree_open_failed",
                    format!("created worktree but failed to open workspace: {err}"),
                );
            }
        };
        self.mark_worktree_membership(&source, ws_idx, checkout_path, true, false);
        if let Some(label) = params.label {
            if let Some(ws) = self.state.workspaces.get_mut(ws_idx) {
                ws.set_custom_name(label);
            }
        }
        self.state.mark_session_dirty();
        self.emit_workspace_open_events(ws_idx);

        let worktree = self
            .worktree_info_for_checkout(&source, ws_idx)
            .expect("created worktree workspace should have worktree info");
        encode_success(
            id,
            ResponseResult::WorktreeCreated {
                workspace: self.workspace_info(ws_idx),
                tab: self
                    .tab_info(ws_idx, 0)
                    .expect("new worktree workspace should have an initial tab"),
                root_pane: self
                    .root_pane_info(ws_idx, 0)
                    .expect("new worktree workspace should have an initial root pane"),
                worktree,
            },
        )
    }

    /// `agent.fork` (#175 F1): fork the target pane's agent conversation into
    /// a new linked worktree — the socket twin of the TUI `branch_session`
    /// flow. Lives here (not in `agents.rs`) because everything after the
    /// branch-plan step is the worktree-create machinery; the source
    /// resolution mirrors the TUI's (#124 branch-from-here included) instead
    /// of `resolve_worktree_source`, which refuses linked-worktree sources.
    pub(super) fn handle_agent_fork(&mut self, id: String, params: AgentForkParams) -> String {
        let resolved = match self.resolve_terminal_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let session = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|ws| ws.pane_state(resolved.pane_id))
            .and_then(|pane| self.state.terminals.get(&pane.attached_terminal_id))
            .and_then(super::super::creation::terminal_agent_session_info);
        let Some(info) = session else {
            return encode_error(
                id,
                "no_agent_session",
                format!(
                    "agent target {} has no resumable session — check `flk integration status`",
                    params.target
                ),
            );
        };
        let session_ref = crate::agent_resume::AgentSessionRef {
            kind: info.kind,
            value: info.value,
        };
        let mut plan =
            match crate::agent_resume::branch_plan(&info.source, &info.agent, &session_ref) {
                Ok(plan) => plan,
                Err(crate::agent_resume::BranchUnsupported::ForkUnsupported { agent }) => {
                    return encode_error(
                        id,
                        "unsupported_for_agent",
                        format!(
                            "{agent} cannot fork a conversation: only claude has a fork \
                             affordance (--fork-session); a plain resume would double-attach \
                             the session id"
                        ),
                    );
                }
                Err(crate::agent_resume::BranchUnsupported::NotResumable { source, agent }) => {
                    return encode_error(
                        id,
                        "unsupported_for_agent",
                        format!("{agent} ({source}) has no resume integration to fork from"),
                    );
                }
            };

        // #178: a fork whose parent transcript is not on disk resumes
        // nothing — claude exits instantly and the workspace would be
        // silently reaped. Refuse up front, before any disk mutation
        // (§8.4: "corrupt/truncate a transcript → fork refuses, clear
        // error").
        if let Some(session_id) = crate::agent_resume::claude_fork_session_id(&plan) {
            let home = std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_default();
            if crate::agent_resume::claude_transcript_path(&home, session_id).is_none() {
                return encode_error(
                    id,
                    "transcript_not_found",
                    format!(
                        "no transcript on disk for session {session_id} — transcript saving \
                         may be disabled for the parent; a fork would resume nothing and die"
                    ),
                );
            }
        }

        let branch = params
            .branch
            .unwrap_or_else(|| {
                let seed = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|duration| duration.as_micros().min(u128::from(u64::MAX)) as u64)
                    .unwrap_or(0);
                crate::worktree::generated_branch_slug(seed)
            })
            .trim()
            .to_string();
        if branch.is_empty() {
            return encode_error(id, "invalid_request", "branch is required");
        }
        let base = params.base.unwrap_or_else(|| "HEAD".into());

        let (existing_membership, space, source_checkout_path, source_workspace_id) =
            match self.worktree_source_metadata(resolved.ws_idx) {
                Ok(metadata) => metadata,
                Err(message) => return encode_error(id, "not_git_worktree", message),
            };
        let checkout_path = match params.path {
            Some(path) => match absolute_user_path(&path) {
                Ok(path) => path,
                Err(err) => return encode_error(id, err.code, err.message),
            },
            None => crate::worktree::default_checkout_path(
                &self.state.worktree_directory,
                &space.label,
                &branch,
            ),
        };
        if let Some(parent_dir) = checkout_path.parent() {
            if let Err(err) = std::fs::create_dir_all(parent_dir) {
                return encode_error(id, "worktree_create_failed", err.to_string());
            }
        }
        let command = crate::worktree::build_worktree_add_new_branch_command(
            &source_checkout_path,
            &checkout_path,
            &branch,
            &base,
        );
        if let Err(err) = crate::worktree::run_worktree_command(&command) {
            return encode_error(
                id,
                "worktree_create_failed",
                crate::worktree::explain_worktree_add_failure(&base, &err),
            );
        }

        // #106/#159 pivot: explicit param wins; omitted falls back to the
        // configured template; empty string opts out.
        let pivot_template = params
            .pivot
            .unwrap_or_else(|| self.state.branch_pivot_message.clone());
        let pivot = pivot_template.replace("<branch>", branch.trim());
        let argv_len_before = plan.argv.len();
        crate::agent_resume::append_pivot_message(&mut plan, &pivot);
        let seeded = plan.argv.len() > argv_len_before;

        let (rows, cols) = self.state.estimate_pane_size();
        // #175 S3 commit 4 (ops): compute the run_id BEFORE spawn so we
        // can stamp it as FLOCK_RUN_ID in the child's environment via
        // `integration::set_pending_run_id` (thread-local one-shot,
        // consumed inside `apply_pane_env`). The event we emit below
        // reuses the same value so an operator can join `agent.lineage`
        // → `flk revert-run` off a single id.
        //
        // Uniqueness: process id + monotonic counter + wallclock micros.
        // The old `fork:<child terminal id>` shape was equally unstable
        // across restarts, and both remain audit-tokens, not addresses.
        let run_id = generated_fork_run_id();
        // Install the trailer hook when opt-in configuration says so
        // (`[checks] run_trailers = true`) — never silently write into a
        // user repo. Failures are non-fatal: the trailer is a
        // convenience, not a correctness invariant.
        if self.state.config.checks.run_trailers {
            let outcome = crate::integration::install_run_trailer_at(&checkout_path);
            match outcome {
                Ok(crate::integration::RunTrailerInstall { state, hook_path }) => match state {
                    crate::integration::RunTrailerInstallState::Installed => {
                        crate::logging::event_log_write_failed(&format!(
                            "run-trailer hook installed at {} (run_trailers=true)",
                            hook_path.display()
                        ));
                    }
                    crate::integration::RunTrailerInstallState::SkippedUserHook => {
                        crate::logging::event_log_write_failed(&format!(
                            "run-trailer hook not installed at {}: user-authored prepare-commit-msg present",
                            hook_path.display()
                        ));
                    }
                },
                Err(err) => crate::logging::event_log_write_failed(&format!(
                    "run-trailer hook install failed: {err}"
                )),
            }
        }
        // Guard, not a bare set: any early return below (spawn failure)
        // must disarm the id, or the next unrelated pane inherits it and
        // `flk revert-run` would revert that pane's work (US-8).
        let _run_id_guard = crate::integration::set_pending_run_id(run_id.clone());
        let ws_idx = match self.spawn_agent_workspace(
            checkout_path.clone(),
            rows,
            cols,
            &plan.argv,
            params.focus,
        ) {
            Ok((ws_idx, _, _)) => ws_idx,
            Err(err) => {
                let body = self.agent_start_error_body(err);
                // P4: fail toward leaking — the created worktree stays on
                // disk for the operator rather than being rolled back.
                return encode_error(
                    id,
                    "agent_fork_failed",
                    format!(
                        "created worktree at {} but failed to start the forked agent ({}): {}",
                        checkout_path.display(),
                        body.code,
                        body.message
                    ),
                );
            }
        };

        // Membership stamping mirrors the TUI confirm path: the source keeps
        // (or gains non-linked) membership; the child is a linked worktree of
        // the shared repo root.
        let source_membership =
            existing_membership.unwrap_or_else(|| crate::workspace::WorktreeSpaceMembership {
                key: space.key.clone(),
                label: space.label.clone(),
                repo_root: space.repo_root.clone(),
                checkout_path: source_checkout_path,
                is_linked_worktree: false,
            });
        if let Some(ws) = self
            .state
            .workspaces
            .iter_mut()
            .find(|ws| ws.id == source_workspace_id)
        {
            ws.worktree_space = Some(source_membership);
        }
        if let Some(ws) = self.state.workspaces.get_mut(ws_idx) {
            ws.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
                key: space.key.clone(),
                label: space.label.clone(),
                repo_root: space.repo_root.clone(),
                checkout_path: checkout_path.clone(),
                is_linked_worktree: true,
            });
            if let Some(label) = params.label {
                ws.set_custom_name(label);
            }
        }
        self.state.mark_session_dirty();

        // Assemble the full response before emitting any event, so a
        // (defensive, effectively unreachable) internal_error return can
        // never leave half a fork on the event log.
        let parent_pane_id = self
            .public_pane_id(resolved.ws_idx, resolved.pane_id)
            .unwrap_or_else(|| params.target.clone());
        let Some(root_pane) = self.root_pane_info(ws_idx, 0) else {
            return encode_error(
                id,
                "internal_error",
                "forked workspace is missing its root pane",
            );
        };
        // NOTE: `run_id` was minted before spawn so we could stamp
        // FLOCK_RUN_ID in the child env (S3 commit 4). The event below
        // reuses that same value so `agent.lineage` and `flk revert-run
        // <id>` join on a single token.
        let info_source = WorktreeSource {
            workspace_idx: None,
            source_checkout_path: space.repo_root.clone(),
            source_repo_root: space.repo_root.clone(),
            repo_key: space.key.clone(),
            repo_name: space.label.clone(),
        };
        let Some(worktree) = self.worktree_info_for_checkout(&info_source, ws_idx) else {
            return encode_error(
                id,
                "internal_error",
                "forked workspace is missing worktree info",
            );
        };
        self.emit_workspace_open_events(ws_idx);
        // #175 O2: the lineage edge + telemetry ride the verb itself, so the
        // fork-vs-message question is measurable from day one.
        self.emit_event(EventEnvelope {
            event: EventKind::AgentForked,
            data: EventData::AgentForked {
                run_id: run_id.clone(),
                parent_pane_id: parent_pane_id.clone(),
                parent_workspace_id: self.public_workspace_id(resolved.ws_idx),
                parent_repo: space.key.clone(),
                agent: info.agent.clone(),
                child_workspace_id: self.public_workspace_id(ws_idx),
                child_pane_id: root_pane.pane_id.clone(),
                child_worktree: checkout_path.display().to_string(),
                child_branch: branch.clone(),
                seeded,
            },
        });
        encode_success(
            id,
            ResponseResult::AgentForked {
                run_id,
                parent_pane_id,
                workspace: self.workspace_info(ws_idx),
                tab: self
                    .tab_info(ws_idx, 0)
                    .expect("forked workspace should have an initial tab"),
                root_pane,
                worktree,
                argv: plan.argv,
                seeded,
            },
        )
    }

    pub(super) fn handle_worktree_open(
        &mut self,
        id: String,
        params: WorktreeOpenParams,
    ) -> String {
        if params.path.is_some() == params.branch.is_some() {
            return encode_error(
                id,
                "invalid_request",
                "exactly one of path or branch is required",
            );
        }
        let mut source = match self.resolve_worktree_source(params.workspace_id, params.cwd) {
            Ok(source) => source,
            Err(err) => return encode_error(id, err.code, err.message),
        };
        let entry = match self.find_worktree_entry(&source, params.path, params.branch) {
            Ok(entry) => entry,
            Err(err) => return encode_error(id, err.code, err.message),
        };
        if entry.is_bare || entry.is_prunable {
            return encode_error(id, "worktree_not_found", "worktree cannot be opened");
        }
        let canonical_path = crate::worktree::canonical_or_original(&entry.path);
        let canonical_source = crate::worktree::canonical_or_original(&source.source_checkout_path);
        let target_is_source = canonical_path == canonical_source;
        let already_open = self.open_workspace_idx_for_checkout(&canonical_path);
        let defer_source_created_event = target_is_source && already_open.is_none();
        let created_source_workspace =
            match self.ensure_source_parent_membership(&mut source, !defer_source_created_event) {
                Ok(created) => created,
                Err(err) => return encode_error(id, err.code, err.message),
            };
        let (ws_idx, created_workspace) = if let Some(ws_idx) = already_open {
            if params.focus {
                self.state.switch_workspace(ws_idx);
            }
            (ws_idx, false)
        } else if target_is_source {
            let ws_idx = source
                .workspace_idx
                .expect("source workspace should exist after membership ensure");
            if params.focus {
                self.state.switch_workspace(ws_idx);
            }
            (ws_idx, created_source_workspace)
        } else {
            match self.create_workspace_with_options(entry.path.clone(), params.focus) {
                Ok(ws_idx) => (ws_idx, true),
                Err(err) => return encode_error(id, "worktree_open_failed", err.to_string()),
            }
        };
        self.mark_worktree_membership(
            &source,
            ws_idx,
            entry.path.clone(),
            canonical_path != crate::worktree::canonical_or_original(&source.source_repo_root),
            !created_workspace,
        );
        if let Some(label) = params.label {
            let workspace_id = self.public_workspace_id(ws_idx);
            if let Some(ws) = self.state.workspaces.get_mut(ws_idx) {
                ws.set_custom_name(label.clone());
                crate::logging::workspace_renamed(&ws.id);
            }
            if !created_workspace {
                self.emit_event(EventEnvelope {
                    event: EventKind::WorkspaceRenamed,
                    data: EventData::WorkspaceRenamed {
                        workspace_id,
                        label,
                    },
                });
            }
        }
        self.state.mark_session_dirty();
        if created_workspace {
            self.emit_workspace_open_events(ws_idx);
        }

        let tab_idx = self.state.workspaces[ws_idx].active_tab;
        encode_success(
            id,
            ResponseResult::WorktreeOpened {
                workspace: self.workspace_info(ws_idx),
                tab: self
                    .tab_info(ws_idx, tab_idx)
                    .expect("opened worktree workspace should have an active tab"),
                root_pane: self
                    .root_pane_info(ws_idx, tab_idx)
                    .expect("opened worktree workspace should have an active root pane"),
                worktree: self.worktree_info_for_entry(&source, entry),
                already_open: already_open.is_some(),
            },
        )
    }

    pub(super) fn handle_worktree_remove(
        &mut self,
        id: String,
        params: WorktreeRemoveParams,
    ) -> String {
        let Some(ws_idx) = self.parse_workspace_id(&params.workspace_id) else {
            return encode_error(
                id,
                "workspace_not_found",
                format!("workspace {} not found", params.workspace_id),
            );
        };
        // #197: the action view. This is the mutation itself — a membership
        // live git places in another repo would delete THAT repo's checkout
        // on this workspace's name.
        let Some(space) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.worktree_space_here().cloned())
        else {
            return encode_error(
                id,
                "not_linked_worktree",
                "workspace is not a Flock-managed worktree checkout",
            );
        };
        if !space.is_linked_worktree {
            return encode_error(
                id,
                "not_linked_worktree",
                "workspace is not a linked worktree checkout",
            );
        }

        let command = crate::worktree::build_worktree_remove_command(
            &space.repo_root,
            &space.checkout_path,
            params.force,
        );
        if let Err(err) = crate::worktree::run_worktree_command(&command) {
            let code = if !params.force && crate::worktree::is_dirty_worktree_remove_error(&err) {
                "dirty_worktree_requires_force"
            } else {
                "worktree_remove_failed"
            };
            return encode_error(id, code, err);
        }

        let workspace_id = self.public_workspace_id(ws_idx);
        let path = space.checkout_path.display().to_string();
        let still_same_linked_worktree = {
            let ws = &self.state.workspaces[ws_idx];
            ws.worktree_space_here().is_some_and(|current| {
                current.is_linked_worktree && current.checkout_path == space.checkout_path
            })
        };
        if still_same_linked_worktree {
            self.state.selected = ws_idx;
            self.state.close_selected_workspace();
            self.shutdown_detached_terminal_runtimes();
            self.emit_event(EventEnvelope {
                event: EventKind::WorkspaceClosed,
                data: EventData::WorkspaceClosed {
                    workspace_id: workspace_id.clone(),
                },
            });
        }

        encode_success(
            id,
            ResponseResult::WorktreeRemoved {
                workspace_id,
                path,
                forced: params.force,
            },
        )
    }

    fn resolve_worktree_source(
        &mut self,
        workspace_id: Option<String>,
        cwd: Option<String>,
    ) -> Result<WorktreeSource, ApiFailure> {
        if workspace_id.is_some() && cwd.is_some() {
            return Err(ApiFailure::new(
                "invalid_request",
                "only one of workspace_id or cwd may be supplied",
            ));
        }

        if let Some(workspace_id) = workspace_id {
            let Some(ws_idx) = self.parse_workspace_id(&workspace_id) else {
                return Err(ApiFailure::new(
                    "workspace_not_found",
                    format!("workspace {workspace_id} not found"),
                ));
            };
            return self.worktree_source_from_workspace(ws_idx);
        }

        if let Some(cwd) = cwd {
            let path = absolute_user_path(&cwd)?;
            let space = crate::workspace::git_space_metadata(&path).ok_or_else(|| {
                ApiFailure::new(
                    "not_git_worktree",
                    "Flock worktree actions require a path inside a Git work tree",
                )
            })?;
            if space.is_linked_worktree {
                return Err(ApiFailure::new(
                    "linked_worktree_source",
                    "New and open worktree actions start from the repo parent workspace.",
                ));
            }
            let source = WorktreeSource {
                workspace_idx: self.find_parent_workspace_for_space(&space),
                source_checkout_path: space.repo_root.clone(),
                source_repo_root: space.repo_root,
                repo_key: space.key,
                repo_name: space.label,
            };
            return Ok(source);
        }

        let Some(ws_idx) = self.state.active.or_else(|| {
            self.state
                .workspaces
                .get(self.state.selected)
                .map(|_| self.state.selected)
        }) else {
            return Err(ApiFailure::new(
                "invalid_request",
                "workspace_id or cwd is required when no workspace is active",
            ));
        };
        self.worktree_source_from_workspace(ws_idx)
    }

    fn resolve_worktree_list_source(
        &mut self,
        workspace_id: Option<String>,
        cwd: Option<String>,
    ) -> Result<WorktreeSource, ApiFailure> {
        if workspace_id.is_some() && cwd.is_some() {
            return Err(ApiFailure::new(
                "invalid_request",
                "only one of workspace_id or cwd may be supplied",
            ));
        }

        if let Some(workspace_id) = workspace_id {
            let Some(ws_idx) = self.parse_workspace_id(&workspace_id) else {
                return Err(ApiFailure::new(
                    "workspace_not_found",
                    format!("workspace {workspace_id} not found"),
                ));
            };
            return self.worktree_list_source_from_workspace(ws_idx);
        }

        if let Some(cwd) = cwd {
            let path = absolute_user_path(&cwd)?;
            let space = crate::workspace::git_space_metadata(&path).ok_or_else(|| {
                ApiFailure::new(
                    "not_git_worktree",
                    "Flock worktree actions require a path inside a Git work tree",
                )
            })?;
            let workspace_idx = self.list_source_workspace_idx_for_space(&space);
            return Ok(worktree_source_from_space(space, workspace_idx, true));
        }

        let Some(ws_idx) = self.state.active.or_else(|| {
            self.state
                .workspaces
                .get(self.state.selected)
                .map(|_| self.state.selected)
        }) else {
            return Err(ApiFailure::new(
                "invalid_request",
                "workspace_id or cwd is required when no workspace is active",
            ));
        };
        self.worktree_list_source_from_workspace(ws_idx)
    }

    fn worktree_source_from_workspace(&self, ws_idx: usize) -> Result<WorktreeSource, ApiFailure> {
        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return Err(ApiFailure::new(
                "workspace_not_found",
                "workspace not found",
            ));
        };
        // #197: the action view here and in the list variant below — a
        // membership live git places in another repo would make every
        // create/open/list on this workspace operate on that repo.
        // #124 parity: a flock-managed linked worktree is a valid source over
        // the API too — the new branch forks from that worktree's own HEAD,
        // and `git worktree add` runs from its checkout. This path used to
        // refuse every linked worktree, so branch-from-here worked in the app
        // and through agent.fork but not through `flk worktree create`. An
        // ad-hoc linked checkout (no membership) still falls through to the
        // refusal below: its repo_root is ambiguous.
        if let Some(membership) = ws.worktree_space_here() {
            return Ok(WorktreeSource {
                workspace_idx: Some(ws_idx),
                source_checkout_path: membership.checkout_path.clone(),
                source_repo_root: membership.repo_root.clone(),
                repo_key: membership.key.clone(),
                repo_name: membership.label.clone(),
            });
        }

        let git_space = ws.git_space().cloned().or_else(|| {
            ws.resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
                .as_deref()
                .and_then(crate::workspace::git_space_metadata)
        });
        let Some(space) = git_space else {
            return Err(ApiFailure::new(
                "not_git_worktree",
                "Flock worktree actions require a workspace inside a Git work tree",
            ));
        };
        if space.is_linked_worktree {
            return Err(ApiFailure::new(
                "linked_worktree_source",
                "New and open worktree actions start from the repo parent workspace.",
            ));
        }
        Ok(WorktreeSource {
            workspace_idx: Some(ws_idx),
            source_checkout_path: space.repo_root.clone(),
            source_repo_root: space.repo_root,
            repo_key: space.key,
            repo_name: space.label,
        })
    }

    fn worktree_list_source_from_workspace(
        &self,
        ws_idx: usize,
    ) -> Result<WorktreeSource, ApiFailure> {
        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return Err(ApiFailure::new(
                "workspace_not_found",
                "workspace not found",
            ));
        };
        if let Some(membership) = ws.worktree_space_here() {
            let source_checkout_path = if membership.is_linked_worktree {
                membership.repo_root.clone()
            } else {
                membership.checkout_path.clone()
            };
            let workspace_idx = if membership.is_linked_worktree {
                self.open_workspace_idx_for_checkout(&membership.repo_root)
            } else {
                Some(ws_idx)
            };
            return Ok(WorktreeSource {
                workspace_idx,
                source_checkout_path,
                source_repo_root: membership.repo_root.clone(),
                repo_key: membership.key.clone(),
                repo_name: membership.label.clone(),
            });
        }

        let git_space = ws.git_space().cloned().or_else(|| {
            ws.resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
                .as_deref()
                .and_then(crate::workspace::git_space_metadata)
        });
        let Some(space) = git_space else {
            return Err(ApiFailure::new(
                "not_git_worktree",
                "Flock worktree actions require a workspace inside a Git work tree",
            ));
        };
        let workspace_idx = if space.is_linked_worktree {
            self.list_source_workspace_idx_for_space(&space)
        } else {
            Some(ws_idx)
        };
        Ok(worktree_source_from_space(space, workspace_idx, true))
    }

    fn ensure_source_parent_membership(
        &mut self,
        source: &mut WorktreeSource,
        emit_created_event: bool,
    ) -> Result<bool, ApiFailure> {
        if source.workspace_idx.is_none() {
            source.workspace_idx = self.find_parent_workspace_by_key(&source.repo_key);
        }
        let mut created_parent = false;
        if source.workspace_idx.is_none() {
            let ws_idx = self
                .create_workspace_with_options(source.source_checkout_path.clone(), false)
                .map_err(|err| ApiFailure::new("worktree_open_failed", err.to_string()))?;
            source.workspace_idx = Some(ws_idx);
            created_parent = true;
        }
        if let Some(ws_idx) = source.workspace_idx {
            // #124 branch-from-here: a source that is itself a flock-managed
            // linked worktree keeps its own membership. Stamping the parent
            // shape over it would demote the row out of its worktree group
            // for having been branched from — the TUI path preserves it, and
            // this one has to agree.
            let existing_linked = self
                .state
                .workspaces
                .get(ws_idx)
                .and_then(|ws| ws.worktree_space_here())
                .is_some_and(|space| space.is_linked_worktree);
            let membership = if existing_linked {
                worktree_membership(source, source.source_checkout_path.clone(), true)
            } else {
                worktree_membership(source, source.source_checkout_path.clone(), false)
            };
            self.set_worktree_membership(ws_idx, membership, !created_parent);
            if created_parent && emit_created_event {
                self.emit_workspace_open_events(ws_idx);
            }
        }
        Ok(created_parent)
    }

    fn find_parent_workspace_for_space(
        &self,
        space: &crate::workspace::GitSpaceMetadata,
    ) -> Option<usize> {
        self.find_parent_workspace_by_key(&space.key)
            .or_else(|| self.open_workspace_idx_for_checkout(&space.repo_root))
    }

    fn list_source_workspace_idx_for_space(
        &self,
        space: &crate::workspace::GitSpaceMetadata,
    ) -> Option<usize> {
        if space.is_linked_worktree {
            let parent_checkout = parent_checkout_path_for_space(space);
            self.open_workspace_idx_for_checkout(&parent_checkout)
        } else {
            self.find_parent_workspace_for_space(space)
        }
    }

    fn find_parent_workspace_by_key(&self, repo_key: &str) -> Option<usize> {
        self.state.workspaces.iter().position(|ws| {
            // #197: the action view — a stale membership must not let an
            // unrelated workspace stand in as this repo's parent checkout.
            ws.worktree_space_here()
                .is_some_and(|space| space.key == repo_key && !space.is_linked_worktree)
                || ws
                    .git_space()
                    .is_some_and(|space| space.key == repo_key && !space.is_linked_worktree)
        })
    }

    fn mark_worktree_membership(
        &mut self,
        source: &WorktreeSource,
        target_ws_idx: usize,
        target_path: PathBuf,
        target_is_linked_worktree: bool,
        emit_update: bool,
    ) {
        let membership = worktree_membership(source, target_path, target_is_linked_worktree);
        self.set_worktree_membership(target_ws_idx, membership, emit_update);
    }

    fn set_worktree_membership(
        &mut self,
        ws_idx: usize,
        membership: crate::workspace::WorktreeSpaceMembership,
        emit_update: bool,
    ) {
        let changed = if let Some(workspace) = self.state.workspaces.get_mut(ws_idx) {
            if workspace.worktree_space.as_ref() == Some(&membership) {
                false
            } else {
                workspace.worktree_space = Some(membership);
                true
            }
        } else {
            false
        };
        if changed {
            self.state.mark_session_dirty();
            if emit_update {
                self.emit_workspace_updated(ws_idx);
            }
        }
    }

    fn find_worktree_entry(
        &self,
        source: &WorktreeSource,
        path: Option<String>,
        branch: Option<String>,
    ) -> Result<crate::worktree::ExistingWorktree, ApiFailure> {
        let entries = crate::worktree::list_existing_worktrees(&source.source_repo_root)
            .map_err(|err| ApiFailure::new("worktree_list_failed", err))?;
        if let Some(path) = path {
            let expected = absolute_user_path(&path)?;
            let expected = crate::worktree::canonical_or_original(&expected);
            entries
                .into_iter()
                .find(|entry| crate::worktree::canonical_or_original(&entry.path) == expected)
                .ok_or_else(|| ApiFailure::new("worktree_not_found", "worktree path not found"))
        } else if let Some(branch) = branch {
            let matches = entries
                .into_iter()
                .filter(|entry| {
                    !entry.is_bare
                        && !entry.is_prunable
                        && !entry.is_detached
                        && entry.branch.as_deref() == Some(branch.as_str())
                })
                .collect::<Vec<_>>();
            match matches.len() {
                0 => Err(ApiFailure::new(
                    "worktree_not_found",
                    "worktree branch not found",
                )),
                1 => Ok(matches.into_iter().next().expect("one match should exist")),
                _ => Err(ApiFailure::new(
                    "ambiguous_worktree_branch",
                    "multiple worktrees matched branch",
                )),
            }
        } else {
            Err(ApiFailure::new(
                "invalid_request",
                "exactly one of path or branch is required",
            ))
        }
    }

    fn worktree_source_info(&self, source: &WorktreeSource) -> WorktreeSourceInfo {
        WorktreeSourceInfo {
            repo_key: source.repo_key.clone(),
            repo_name: source.repo_name.clone(),
            repo_root: source.source_repo_root.display().to_string(),
            source_checkout_path: source.source_checkout_path.display().to_string(),
            source_workspace_id: source
                .workspace_idx
                .map(|idx| self.public_workspace_id(idx)),
        }
    }

    fn worktree_info_for_entry(
        &self,
        source: &WorktreeSource,
        entry: crate::worktree::ExistingWorktree,
    ) -> WorktreeInfo {
        let canonical_path = crate::worktree::canonical_or_original(&entry.path);
        let repo_root = crate::worktree::canonical_or_original(&source.source_repo_root);
        WorktreeInfo {
            path: entry.path.display().to_string(),
            branch: entry.branch,
            is_bare: entry.is_bare,
            is_detached: entry.is_detached,
            is_prunable: entry.is_prunable,
            is_linked_worktree: canonical_path != repo_root,
            open_workspace_id: self
                .open_workspace_idx_for_checkout(&canonical_path)
                .map(|idx| self.public_workspace_id(idx)),
            label: source.repo_name.clone(),
        }
    }

    fn worktree_info_for_checkout(
        &self,
        source: &WorktreeSource,
        ws_idx: usize,
    ) -> Option<WorktreeInfo> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let membership = ws.worktree_space_here()?;
        let branch = crate::workspace::git_branch(&membership.checkout_path);
        let is_detached = branch.is_none();
        Some(WorktreeInfo {
            path: membership.checkout_path.display().to_string(),
            branch,
            is_bare: false,
            is_detached,
            is_prunable: false,
            is_linked_worktree: membership.is_linked_worktree,
            open_workspace_id: Some(self.public_workspace_id(ws_idx)),
            label: source.repo_name.clone(),
        })
    }

    fn open_workspace_idx_for_checkout(&self, checkout_path: &Path) -> Option<usize> {
        let canonical_checkout = crate::worktree::canonical_or_original(checkout_path);
        let checkout_key = canonical_checkout.display().to_string();
        self.state.workspaces.iter().position(|ws| {
            if ws.worktree_space_here().is_some_and(|space| {
                crate::worktree::canonical_or_original(&space.checkout_path) == canonical_checkout
            }) {
                return true;
            }

            let git_space = ws.git_space().cloned().or_else(|| {
                ws.resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
                    .as_deref()
                    .and_then(crate::workspace::git_space_metadata)
            });
            if git_space
                .as_ref()
                .is_some_and(|metadata| metadata.checkout_key == checkout_key)
            {
                return true;
            }

            ws.resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
                .as_deref()
                .is_some_and(|cwd| {
                    crate::worktree::canonical_or_original(cwd) == canonical_checkout
                })
        })
    }

    fn emit_workspace_open_events(&self, ws_idx: usize) {
        let workspace_info = self.workspace_info(ws_idx);
        let Some(tab) = self.tab_info(ws_idx, 0) else {
            return;
        };
        let Some(root_pane) = self.root_pane_info(ws_idx, 0) else {
            return;
        };
        self.emit_event(EventEnvelope {
            event: EventKind::WorkspaceCreated,
            data: EventData::WorkspaceCreated {
                workspace: workspace_info,
            },
        });
        self.emit_event(EventEnvelope {
            event: EventKind::TabCreated,
            data: EventData::TabCreated { tab },
        });
        self.emit_event(EventEnvelope {
            event: EventKind::PaneCreated,
            data: EventData::PaneCreated { pane: root_pane },
        });
    }

    fn emit_workspace_updated(&self, ws_idx: usize) {
        self.emit_event(EventEnvelope {
            event: EventKind::WorkspaceUpdated,
            data: EventData::WorkspaceUpdated {
                workspace: self.workspace_info(ws_idx),
            },
        });
    }
}

fn worktree_source_from_space(
    space: crate::workspace::GitSpaceMetadata,
    workspace_idx: Option<usize>,
    allow_linked: bool,
) -> WorktreeSource {
    let source_checkout_path = if allow_linked {
        parent_checkout_path_for_space(&space)
    } else {
        space.repo_root.clone()
    };
    WorktreeSource {
        workspace_idx,
        source_checkout_path: source_checkout_path.clone(),
        source_repo_root: source_checkout_path,
        repo_key: space.key,
        repo_name: space.label,
    }
}

fn parent_checkout_path_for_space(space: &crate::workspace::GitSpaceMetadata) -> PathBuf {
    if !space.is_linked_worktree {
        return space.repo_root.clone();
    }

    crate::worktree::list_existing_worktrees(&space.repo_root)
        .ok()
        .and_then(|entries| {
            entries.into_iter().find_map(|entry| {
                let entry_space = crate::workspace::git_space_metadata(&entry.path)?;
                if entry_space.key == space.key && !entry_space.is_linked_worktree {
                    Some(entry_space.repo_root)
                } else {
                    None
                }
            })
        })
        .unwrap_or_else(|| space.repo_root.clone())
}

fn worktree_membership(
    source: &WorktreeSource,
    checkout_path: PathBuf,
    is_linked_worktree: bool,
) -> crate::workspace::WorktreeSpaceMembership {
    crate::workspace::WorktreeSpaceMembership {
        key: source.repo_key.clone(),
        label: source.repo_name.clone(),
        repo_root: source.source_repo_root.clone(),
        checkout_path,
        is_linked_worktree,
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Tests exec real git to prime fixtures — TracedCommand polices product code (logging redesign PR-3).
mod tests {
    use super::*;
    use crate::api::schema::{ErrorResponse, Request, SuccessResponse};
    use crate::{config::Config, workspace::Workspace};

    fn unique_temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("flock-{name}-{}-{nanos}", std::process::id()))
    }

    fn run_git(repo: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "git command failed: git -C {} {}",
            repo.display(),
            args.join(" ")
        );
    }

    fn create_committed_repo(name: &str) -> PathBuf {
        let repo = unique_temp_path(name);
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "--quiet"]);
        run_git(&repo, &["config", "user.email", "flock@example.invalid"]);
        run_git(&repo, &["config", "user.name", "Flock Test"]);
        std::fs::write(repo.join("README.md"), "test\n").unwrap();
        run_git(&repo, &["add", "README.md"]);
        run_git(&repo, &["commit", "--quiet", "-m", "initial"]);
        repo
    }

    fn test_app() -> App {
        test_app_with_event_hub(crate::api::EventHub::default())
    }

    fn test_app_with_event_hub(event_hub: crate::api::EventHub) -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(&Config::default(), true, None, api_rx, event_hub)
    }

    fn app_with_parent(repo: &Path) -> App {
        app_with_parent_on(test_app(), repo)
    }

    fn app_with_parent_on(app: App, repo: &Path) -> App {
        let mut app = app;
        let mut parent = Workspace::test_new("main");
        parent.identity_cwd = repo.to_path_buf();
        app.state.workspaces = vec![parent];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        app
    }

    #[tokio::test]
    async fn api_worktree_create_opens_workspace_and_marks_membership() {
        let repo = create_committed_repo("api-worktree-create-repo");
        let worktree_root = unique_temp_path("api-worktree-create-root");
        let mut app = app_with_parent(&repo);
        app.state.worktree_directory = worktree_root.clone();

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: crate::api::schema::Method::WorktreeCreate(WorktreeCreateParams {
                workspace_id: Some(app.state.workspaces[0].id.clone()),
                branch: Some("worktree/api-create".into()),
                ..WorktreeCreateParams::default()
            }),
        });

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::WorktreeCreated {
            workspace,
            tab,
            root_pane,
            worktree,
        } = success.result
        else {
            panic!("expected worktree_created response");
        };
        assert_eq!(tab.workspace_id, workspace.workspace_id);
        assert_eq!(root_pane.workspace_id, workspace.workspace_id);
        assert_eq!(worktree.branch.as_deref(), Some("worktree/api-create"));
        assert!(Path::new(&worktree.path).join("README.md").exists());
        assert_eq!(app.state.workspaces.len(), 2);
        assert!(
            !app.state.workspaces[0]
                .worktree_space()
                .unwrap()
                .is_linked_worktree
        );
        assert!(
            app.state.workspaces[1]
                .worktree_space()
                .unwrap()
                .is_linked_worktree
        );
        assert!(workspace.worktree.unwrap().is_linked_worktree);

        let remove =
            crate::worktree::build_worktree_remove_command(&repo, Path::new(&worktree.path), false);
        crate::worktree::run_worktree_command(&remove).unwrap();
        let _ = std::fs::remove_dir_all(worktree_root);
        let _ = std::fs::remove_dir_all(repo);
    }

    #[tokio::test]
    async fn api_worktree_create_from_cwd_emits_parent_with_membership() {
        let repo = create_committed_repo("api-worktree-create-cwd-repo");
        let worktree_root = unique_temp_path("api-worktree-create-cwd-root");
        let event_hub = crate::api::EventHub::default();
        let mut app = test_app_with_event_hub(event_hub.clone());
        app.state.worktree_directory = worktree_root.clone();

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: crate::api::schema::Method::WorktreeCreate(WorktreeCreateParams {
                cwd: Some(repo.display().to_string()),
                branch: Some("worktree/api-create-cwd".into()),
                ..WorktreeCreateParams::default()
            }),
        });
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::WorktreeCreated { worktree, .. } = success.result else {
            panic!("expected worktree_created response");
        };

        let events = event_hub.events_after(0);
        let parent_created = events
            .iter()
            .filter_map(|(_, event)| match &event.data {
                EventData::WorkspaceCreated { workspace } => Some(workspace),
                _ => None,
            })
            .find(|workspace| {
                workspace
                    .worktree
                    .as_ref()
                    .is_some_and(|worktree| !worktree.is_linked_worktree)
            });
        assert!(
            parent_created.is_some(),
            "auto-created parent workspace event should include parent worktree membership"
        );

        let remove =
            crate::worktree::build_worktree_remove_command(&repo, Path::new(&worktree.path), false);
        crate::worktree::run_worktree_command(&remove).unwrap();
        let _ = std::fs::remove_dir_all(worktree_root);
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn invalid_worktree_create_from_cwd_does_not_create_parent_workspace() {
        let repo = create_committed_repo("api-worktree-create-invalid-cwd-repo");
        let mut app = test_app();

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: crate::api::schema::Method::WorktreeCreate(WorktreeCreateParams {
                cwd: Some(repo.display().to_string()),
                branch: Some("   ".into()),
                ..WorktreeCreateParams::default()
            }),
        });

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "invalid_request");
        assert!(app.state.workspaces.is_empty());
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn invalid_worktree_open_from_cwd_does_not_create_parent_workspace() {
        let repo = create_committed_repo("api-worktree-open-invalid-cwd-repo");
        let mut app = test_app();

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: crate::api::schema::Method::WorktreeOpen(WorktreeOpenParams {
                cwd: Some(repo.display().to_string()),
                path: Some("/tmp/one".into()),
                branch: Some("worktree/one".into()),
                ..WorktreeOpenParams::default()
            }),
        });

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "invalid_request");
        assert!(app.state.workspaces.is_empty());
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn raw_api_worktree_create_rejects_relative_path_override() {
        let repo = create_committed_repo("api-worktree-relative-path-repo");
        let mut app = app_with_parent(&repo);

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: crate::api::schema::Method::WorktreeCreate(WorktreeCreateParams {
                workspace_id: Some(app.state.workspaces[0].id.clone()),
                branch: Some("worktree/relative".into()),
                path: Some("relative-checkout".into()),
                ..WorktreeCreateParams::default()
            }),
        });

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "invalid_request");
        assert_eq!(app.state.workspaces.len(), 1);
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn raw_api_worktree_create_rejects_relative_cwd() {
        let mut app = test_app();

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: crate::api::schema::Method::WorktreeCreate(WorktreeCreateParams {
                cwd: Some("relative-repo".into()),
                branch: Some("worktree/relative-cwd".into()),
                ..WorktreeCreateParams::default()
            }),
        });

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "invalid_request");
        assert!(app.state.workspaces.is_empty());
    }

    #[test]
    fn api_worktree_open_reuses_already_open_checkout_from_subdirectory() {
        let repo = create_committed_repo("api-worktree-open-repo");
        let checkout = unique_temp_path("api-worktree-open-checkout");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "worktree/api-open",
                checkout.to_str().unwrap(),
                "HEAD",
            ],
        );
        let subdir = checkout.join("nested");
        std::fs::create_dir_all(&subdir).unwrap();

        let mut app = app_with_parent(&repo);
        let mut child = Workspace::test_new("child");
        child.identity_cwd = subdir;
        app.state.workspaces.push(child);
        app.state.ensure_test_terminals();

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: crate::api::schema::Method::WorktreeOpen(WorktreeOpenParams {
                workspace_id: Some(app.state.workspaces[0].id.clone()),
                branch: Some("worktree/api-open".into()),
                focus: true,
                ..WorktreeOpenParams::default()
            }),
        });

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::WorktreeOpened {
            workspace,
            already_open,
            ..
        } = success.result
        else {
            panic!("expected worktree_opened response");
        };
        assert!(already_open);
        assert_eq!(app.state.workspaces.len(), 2);
        assert_eq!(app.state.active, Some(1));
        assert_eq!(workspace.workspace_id, app.state.workspaces[1].id);
        assert!(
            app.state.workspaces[1]
                .worktree_space()
                .unwrap()
                .is_linked_worktree
        );

        let remove = crate::worktree::build_worktree_remove_command(&repo, &checkout, false);
        crate::worktree::run_worktree_command(&remove).unwrap();
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn api_worktree_open_label_on_already_open_checkout_emits_rename_event() {
        let repo = create_committed_repo("api-worktree-open-label-repo");
        let checkout = unique_temp_path("api-worktree-open-label-checkout");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "worktree/api-open-label",
                checkout.to_str().unwrap(),
                "HEAD",
            ],
        );

        let event_hub = crate::api::EventHub::default();
        let mut app = test_app_with_event_hub(event_hub.clone());
        let mut parent = Workspace::test_new("main");
        parent.identity_cwd = repo.clone();
        app.state.workspaces = vec![parent];
        let mut child = Workspace::test_new("child");
        child.identity_cwd = checkout.clone();
        let child_id = child.id.clone();
        app.state.workspaces.push(child);
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: crate::api::schema::Method::WorktreeOpen(WorktreeOpenParams {
                workspace_id: Some(app.state.workspaces[0].id.clone()),
                branch: Some("worktree/api-open-label".into()),
                label: Some("review".into()),
                ..WorktreeOpenParams::default()
            }),
        });

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::WorktreeOpened {
            workspace,
            already_open,
            ..
        } = success.result
        else {
            panic!("expected worktree_opened response");
        };
        assert!(already_open);
        assert_eq!(workspace.workspace_id, child_id);
        assert_eq!(workspace.label, "review");
        assert!(event_hub.events_after(0).iter().any(|(_, event)| {
            matches!(
                &event.data,
                EventData::WorkspaceUpdated { workspace }
                    if workspace.workspace_id == child_id
                        && workspace
                            .worktree
                            .as_ref()
                            .is_some_and(|worktree| worktree.is_linked_worktree)
            )
        }));
        assert!(event_hub.events_after(0).iter().any(|(_, event)| {
            matches!(
                &event.data,
                EventData::WorkspaceRenamed {
                    workspace_id,
                    label
                } if workspace_id == &child_id && label == "review"
            )
        }));

        let remove = crate::worktree::build_worktree_remove_command(&repo, &checkout, false);
        crate::worktree::run_worktree_command(&remove).unwrap();
        let _ = std::fs::remove_dir_all(repo);
    }

    #[tokio::test]
    async fn api_worktree_open_source_checkout_created_by_request_is_not_already_open() {
        let repo = create_committed_repo("api-worktree-open-source-repo");
        let event_hub = crate::api::EventHub::default();
        let mut app = test_app_with_event_hub(event_hub.clone());
        app.state.default_shell = "/usr/bin/true".into();

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: crate::api::schema::Method::WorktreeOpen(WorktreeOpenParams {
                cwd: Some(repo.display().to_string()),
                path: Some(repo.display().to_string()),
                label: Some("source checkout".into()),
                ..WorktreeOpenParams::default()
            }),
        });

        let success: SuccessResponse = serde_json::from_str(&response).unwrap_or_else(|err| {
            panic!("expected success response, got {response}: {err}");
        });
        let ResponseResult::WorktreeOpened {
            workspace,
            already_open,
            ..
        } = success.result
        else {
            panic!("expected worktree_opened response");
        };
        assert!(!already_open);
        assert_eq!(workspace.label, "source checkout");
        assert_eq!(app.state.workspaces.len(), 1);
        assert!(event_hub.events_after(0).iter().any(|(_, event)| {
            matches!(
                &event.data,
                EventData::WorkspaceCreated { workspace }
                    if workspace.label == "source checkout"
                        && workspace
                            .worktree
                            .as_ref()
                            .is_some_and(|worktree| !worktree.is_linked_worktree)
            )
        }));
        assert!(!event_hub
            .events_after(0)
            .iter()
            .any(|(_, event)| { matches!(&event.data, EventData::WorkspaceRenamed { .. }) }));

        app.state.selected = 0;
        app.state.close_selected_workspace();
        app.shutdown_detached_terminal_runtimes();
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn api_worktree_list_reports_open_workspace_ids() {
        let repo = create_committed_repo("api-worktree-list-repo");
        let checkout = unique_temp_path("api-worktree-list-checkout");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "worktree/api-list",
                checkout.to_str().unwrap(),
                "HEAD",
            ],
        );
        let mut app = app_with_parent(&repo);
        let mut child = Workspace::test_new("child");
        child.identity_cwd = checkout.clone();
        app.state.workspaces.push(child);
        app.state.ensure_test_terminals();

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: crate::api::schema::Method::WorktreeList(WorktreeListParams {
                workspace_id: Some(app.state.workspaces[0].id.clone()),
                cwd: None,
            }),
        });

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::WorktreeList { worktrees, .. } = success.result else {
            panic!("expected worktree_list response");
        };
        let entry = worktrees
            .iter()
            .find(|entry| entry.branch.as_deref() == Some("worktree/api-list"))
            .unwrap();
        assert_eq!(
            entry.open_workspace_id.as_deref(),
            Some(app.state.workspaces[1].id.as_str())
        );
        assert!(entry.is_linked_worktree);

        let remove = crate::worktree::build_worktree_remove_command(&repo, &checkout, false);
        crate::worktree::run_worktree_command(&remove).unwrap();
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn api_worktree_list_accepts_linked_checkout_sources() {
        let repo = create_committed_repo("api-worktree-list-linked-repo");
        let checkout = unique_temp_path("api-worktree-list-linked-checkout");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "worktree/api-list-linked",
                checkout.to_str().unwrap(),
                "HEAD",
            ],
        );
        let mut app = app_with_parent(&repo);
        let parent_id = app.state.workspaces[0].id.clone();
        let mut child = Workspace::test_new("child");
        child.identity_cwd = checkout.clone();
        let child_id = child.id.clone();
        app.state.workspaces.push(child);
        app.state.ensure_test_terminals();

        for method in [
            crate::api::schema::Method::WorktreeList(WorktreeListParams {
                workspace_id: Some(child_id),
                cwd: None,
            }),
            crate::api::schema::Method::WorktreeList(WorktreeListParams {
                workspace_id: None,
                cwd: Some(checkout.display().to_string()),
            }),
        ] {
            let response = app.handle_api_request(Request {
                id: "req".into(),
                method,
            });
            let success: SuccessResponse = serde_json::from_str(&response).unwrap();
            let ResponseResult::WorktreeList { source, worktrees } = success.result else {
                panic!("expected worktree_list response");
            };
            assert_eq!(
                crate::worktree::canonical_or_original(std::path::Path::new(&source.repo_root)),
                crate::worktree::canonical_or_original(&repo)
            );
            assert_eq!(
                source.source_workspace_id.as_deref(),
                Some(parent_id.as_str())
            );
            assert!(worktrees.iter().any(|entry| {
                entry.branch.as_deref() == Some("worktree/api-list-linked")
                    && entry.is_linked_worktree
            }));
        }

        let remove = crate::worktree::build_worktree_remove_command(&repo, &checkout, false);
        crate::worktree::run_worktree_command(&remove).unwrap();
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn api_worktree_list_preserves_prunable_entries() {
        let repo = create_committed_repo("api-worktree-list-prunable-repo");
        let checkout = unique_temp_path("api-worktree-list-prunable-checkout");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "worktree/api-list-prunable",
                checkout.to_str().unwrap(),
                "HEAD",
            ],
        );
        std::fs::remove_dir_all(&checkout).unwrap();
        let mut app = app_with_parent(&repo);

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: crate::api::schema::Method::WorktreeList(WorktreeListParams {
                workspace_id: Some(app.state.workspaces[0].id.clone()),
                cwd: None,
            }),
        });

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::WorktreeList { worktrees, .. } = success.result else {
            panic!("expected worktree_list response");
        };
        let entry = worktrees
            .iter()
            .find(|entry| entry.branch.as_deref() == Some("worktree/api-list-prunable"))
            .unwrap();
        assert!(entry.is_prunable);
        assert!(entry.is_linked_worktree);

        run_git(&repo, &["worktree", "prune"]);
        let _ = std::fs::remove_dir_all(repo);
    }

    #[tokio::test]
    async fn api_worktree_create_branches_from_a_linked_worktree_source() {
        // #124 parity over the API. `worktree.create` used to refuse every
        // linked worktree source, so branch-from-here worked in the app and
        // through agent.fork but `flk worktree create --workspace <linked>`
        // said "start from the repo parent workspace" — for a checkout flock
        // manages and knows the parent of.
        let repo = create_committed_repo("api-create-from-linked-repo");
        let linked = unique_temp_path("api-create-from-linked-child");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "task/linked-source",
                linked.to_str().unwrap(),
            ],
        );
        let worktree_root = unique_temp_path("api-create-from-linked-root");

        let mut app = app_with_parent(&linked);
        app.state.worktree_directory = worktree_root.clone();
        app.state.workspaces[0].worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: crate::workspace::git_space_metadata(&repo).unwrap().key,
            label: "api-create-from-linked-repo".into(),
            repo_root: repo.clone(),
            checkout_path: linked.clone(),
            is_linked_worktree: true,
        });
        app.state.workspaces[0].cached_git_space = crate::workspace::git_space_metadata(&linked);
        let workspace_id = app.state.workspaces[0].id.clone();

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: crate::api::schema::Method::WorktreeCreate(WorktreeCreateParams {
                workspace_id: Some(workspace_id),
                branch: Some("worktree/from-linked".into()),
                focus: false,
                ..Default::default()
            }),
        });

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::WorktreeCreated { worktree, .. } = success.result else {
            panic!("expected worktree_created response: {response}");
        };
        assert_eq!(worktree.branch.as_deref(), Some("worktree/from-linked"));

        // The source keeps its linked membership — branch-from-here must not
        // demote it into the repo's parent row.
        assert!(
            app.state.workspaces[0]
                .worktree_space()
                .expect("source keeps membership")
                .is_linked_worktree
        );

        let _ = std::fs::remove_dir_all(&worktree_root);
        let _ = std::fs::remove_dir_all(&linked);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn api_worktree_remove_refuses_a_membership_live_git_places_elsewhere() {
        // #197 over the API seam. Two real repos: the workspace carries a
        // membership for `owner`'s worktree while live git says its checkout
        // is `elsewhere`'s main checkout. That is the shape a sibling
        // workspace (#25) takes once its pane `cd`s away.
        //
        // Unguarded, `flk worktree kill --workspace <this>` reads the
        // workspace's reported worktree, runs the merge gate over owner's
        // checkout, and calls worktree.remove — deleting a worktree of a repo
        // this workspace is not in. Both halves must refuse.
        let owner = create_committed_repo("api-foreign-owner-repo");
        let checkout = unique_temp_path("api-foreign-owner-checkout");
        run_git(
            &owner,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "worktree/api-foreign",
                checkout.to_str().unwrap(),
                "HEAD",
            ],
        );
        let elsewhere = create_committed_repo("api-foreign-elsewhere-repo");

        let mut app = app_with_parent(&owner);
        let mut diverged = Workspace::test_new("diverged");
        diverged.identity_cwd = elsewhere.clone();
        diverged.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: crate::workspace::git_space_metadata(&owner).unwrap().key,
            label: "api-foreign-owner-repo".into(),
            repo_root: owner.clone(),
            checkout_path: checkout.clone(),
            is_linked_worktree: true,
        });
        diverged.cached_git_space = crate::workspace::git_space_metadata(&elsewhere);
        let diverged_id = diverged.id.clone();
        app.state.workspaces.push(diverged);
        app.state.ensure_test_terminals();

        // What `flk worktree kill` reads to pick a checkout: nothing.
        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: crate::api::schema::Method::WorkspaceGet(crate::api::schema::WorkspaceTarget {
                workspace_id: diverged_id.clone(),
            }),
        });
        let reported_worktree = serde_json::from_str::<serde_json::Value>(&response)
            .unwrap()
            .pointer("/result/workspace/worktree")
            .cloned();
        assert!(
            reported_worktree.is_none_or(|value| value.is_null()),
            "a foreign membership must not be reported as this workspace's worktree"
        );

        // And the mutation itself refuses, so a caller holding a stale id
        // cannot delete owner's checkout through this workspace.
        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: crate::api::schema::Method::WorktreeRemove(WorktreeRemoveParams {
                workspace_id: diverged_id,
                force: true,
            }),
        });
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "not_linked_worktree");
        assert!(checkout.exists(), "owner's worktree must survive");
        assert_eq!(app.state.workspaces.len(), 2);

        let _ = std::fs::remove_dir_all(&checkout);
        let _ = std::fs::remove_dir_all(&owner);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    #[test]
    fn api_worktree_remove_requires_force_for_dirty_checkout() {
        let repo = create_committed_repo("api-worktree-remove-repo");
        let checkout = unique_temp_path("api-worktree-remove-checkout");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "worktree/api-remove",
                checkout.to_str().unwrap(),
                "HEAD",
            ],
        );
        std::fs::write(checkout.join("README.md"), "dirty\n").unwrap();

        let mut app = app_with_parent(&repo);
        let mut child = Workspace::test_new("child");
        child.identity_cwd = checkout.clone();
        child.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: crate::workspace::git_space_metadata(&repo).unwrap().key,
            label: "api-worktree-remove-repo".into(),
            repo_root: repo.clone(),
            checkout_path: checkout.clone(),
            is_linked_worktree: true,
        });
        let child_id = child.id.clone();
        app.state.workspaces.push(child);
        app.state.ensure_test_terminals();

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: crate::api::schema::Method::WorktreeRemove(WorktreeRemoveParams {
                workspace_id: child_id.clone(),
                force: false,
            }),
        });
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "dirty_worktree_requires_force");
        assert!(checkout.exists());
        assert_eq!(app.state.workspaces.len(), 2);

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: crate::api::schema::Method::WorktreeRemove(WorktreeRemoveParams {
                workspace_id: child_id,
                force: true,
            }),
        });
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::WorktreeRemoved { forced, path, .. } = success.result else {
            panic!("expected worktree_removed response");
        };
        assert!(forced);
        assert_eq!(path, checkout.display().to_string());
        assert!(!checkout.exists());
        assert_eq!(app.state.workspaces.len(), 1);

        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn api_worktree_remove_emits_close_event_and_drains_runtime_shutdowns() {
        let repo = create_committed_repo("api-worktree-remove-event-repo");
        let checkout = unique_temp_path("api-worktree-remove-event-checkout");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "worktree/api-remove-event",
                checkout.to_str().unwrap(),
                "HEAD",
            ],
        );

        let event_hub = crate::api::EventHub::default();
        let mut app = test_app_with_event_hub(event_hub.clone());
        let mut child = Workspace::test_new("child");
        child.identity_cwd = checkout.clone();
        child.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: crate::workspace::git_space_metadata(&repo).unwrap().key,
            label: "api-worktree-remove-event-repo".into(),
            repo_root: repo.clone(),
            checkout_path: checkout.clone(),
            is_linked_worktree: true,
        });
        let child_id = child.id.clone();
        app.state.workspaces.push(child);
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: crate::api::schema::Method::WorktreeRemove(WorktreeRemoveParams {
                workspace_id: child_id.clone(),
                force: false,
            }),
        });
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::WorktreeRemoved { .. }
        ));
        assert!(app.state.workspaces.is_empty());
        assert!(app.state.terminal_runtime_shutdowns.is_empty());
        assert!(event_hub.events_after(0).iter().any(|(_, event)| {
            matches!(
                &event.data,
                EventData::WorkspaceClosed { workspace_id } if workspace_id == &child_id
            )
        }));

        let _ = std::fs::remove_dir_all(repo);
    }

    /// Stamp a persisted agent session on the parent workspace's focused
    /// pane so `agent.fork` has something to resolve, and return the pane's
    /// terminal id (a valid `target`).
    fn stamp_agent_session(app: &mut App, source: &str, agent: &str) -> String {
        let ws = &app.state.workspaces[0];
        let pane_id = ws.focused_pane_id().expect("parent pane");
        let terminal_id = ws
            .pane_state(pane_id)
            .expect("pane state")
            .attached_terminal_id
            .clone();
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal");
        let terminal_id = terminal_id.to_string();
        terminal.persisted_agent_session = Some(crate::agent_resume::PersistedAgentSession {
            source: source.into(),
            agent: agent.into(),
            session_ref: crate::agent_resume::AgentSessionRef::id("sess-fork").expect("session id"),
        });
        terminal_id
    }

    /// #178: fork pre-flight requires the parent transcript on disk. Point
    /// HOME at a temp dir containing one (nextest = process per test, so the
    /// env mutation is isolated).
    fn fake_claude_home(name: &str, session_id: &str) -> PathBuf {
        let home = unique_temp_path(name);
        let project = home.join(".claude/projects/-repo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(format!("{session_id}.jsonl")), "{}\n").unwrap();
        std::env::set_var("HOME", &home);
        home
    }

    /// Put a stub `claude` executable on PATH so the forked pane's PTY spawn
    /// succeeds without the real CLI. Safe: nextest runs each test in its own
    /// process.
    fn stub_claude_on_path(name: &str) -> PathBuf {
        let bin_dir = unique_temp_path(name);
        std::fs::create_dir_all(&bin_dir).unwrap();
        let stub = bin_dir.join("claude");
        std::fs::write(&stub, "#!/bin/sh\nexec sleep 30\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{path}", bin_dir.display()));
        bin_dir
    }

    #[tokio::test]
    async fn api_agent_fork_creates_worktree_and_spawns_forked_pane() {
        let claude_home = fake_claude_home("fork-home", "sess-fork");
        let repo = create_committed_repo("api-agent-fork-repo");
        let worktree_root = unique_temp_path("api-agent-fork-root");
        let bin_dir = stub_claude_on_path("api-agent-fork-bin");
        let event_hub = crate::api::EventHub::default();
        let mut app = app_with_parent_on(test_app_with_event_hub(event_hub.clone()), &repo);
        app.state.worktree_directory = worktree_root.clone();
        let target = stamp_agent_session(&mut app, "flock:claude", "claude");

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: crate::api::schema::Method::AgentFork(crate::api::schema::AgentForkParams {
                target: target.clone(),
                branch: Some("fork/alt-approach".into()),
                base: None,
                path: None,
                label: None,
                pivot: Some("try the alternative on <branch>".into()),
                focus: false,
            }),
        });

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentForked {
            run_id,
            parent_pane_id,
            workspace,
            tab,
            root_pane,
            worktree,
            argv,
            seeded,
        } = success.result
        else {
            panic!("expected agent_forked response");
        };
        assert_eq!(
            argv,
            vec![
                "claude",
                "--resume",
                "sess-fork",
                "--fork-session",
                "try the alternative on fork/alt-approach",
            ]
        );
        assert!(seeded);
        // #175 S3 commit 4: run_id is now pre-computed (so it can stamp
        // FLOCK_RUN_ID in the child env before spawn), no longer derived
        // from the terminal id. Assert the shape instead.
        assert!(
            run_id.starts_with("fork:"),
            "run_id has the fork: prefix: {run_id}"
        );
        let expected_parent = app.state.workspaces[0]
            .focused_pane_id()
            .and_then(|pane_id| app.public_pane_id(0, pane_id))
            .expect("parent public pane id");
        assert_eq!(parent_pane_id, expected_parent);
        assert_eq!(tab.workspace_id, workspace.workspace_id);
        assert_eq!(root_pane.workspace_id, workspace.workspace_id);
        assert_eq!(worktree.branch.as_deref(), Some("fork/alt-approach"));
        assert!(Path::new(&worktree.path).join("README.md").exists());
        assert_eq!(app.state.workspaces.len(), 2);
        assert!(
            !app.state.workspaces[0]
                .worktree_space()
                .unwrap()
                .is_linked_worktree,
            "source keeps parent membership"
        );
        assert!(
            app.state.workspaces[1]
                .worktree_space()
                .unwrap()
                .is_linked_worktree,
            "child is a linked worktree"
        );

        // #175 O2: the lineage/telemetry event is emitted with the verb.
        let events = event_hub.events_after(0);
        let forked = events
            .iter()
            .find_map(|(_, event)| match &event.data {
                EventData::AgentForked {
                    run_id: event_run_id,
                    parent_pane_id: event_parent,
                    parent_repo,
                    agent,
                    child_branch,
                    child_worktree,
                    seeded: event_seeded,
                    ..
                } => Some((
                    event_run_id.clone(),
                    event_parent.clone(),
                    parent_repo.clone(),
                    agent.clone(),
                    child_branch.clone(),
                    child_worktree.clone(),
                    *event_seeded,
                )),
                _ => None,
            })
            .expect("agent_forked event must be emitted");
        assert_eq!(forked.0, run_id);
        assert_eq!(forked.1, parent_pane_id);
        assert!(!forked.2.is_empty(), "parent repo key present");
        assert_eq!(forked.3, "claude");
        assert_eq!(forked.4, "fork/alt-approach");
        assert_eq!(forked.5, worktree.path);
        assert!(forked.6, "pivot seeded");

        let remove =
            crate::worktree::build_worktree_remove_command(&repo, Path::new(&worktree.path), true);
        let _ = crate::worktree::run_worktree_command(&remove);
        let _ = std::fs::remove_dir_all(worktree_root);
        let _ = std::fs::remove_dir_all(repo);
        let _ = std::fs::remove_dir_all(bin_dir);
        let _ = std::fs::remove_dir_all(claude_home);
    }

    #[tokio::test]
    async fn api_agent_fork_empty_pivot_opts_out_of_seed() {
        let claude_home = fake_claude_home("nopivot-home", "sess-fork");
        let repo = create_committed_repo("api-agent-fork-nopivot-repo");
        let worktree_root = unique_temp_path("api-agent-fork-nopivot-root");
        let bin_dir = stub_claude_on_path("api-agent-fork-nopivot-bin");
        let mut app = app_with_parent(&repo);
        app.state.worktree_directory = worktree_root.clone();
        app.state.branch_pivot_message = "configured template for <branch>".into();
        let target = stamp_agent_session(&mut app, "flock:claude", "claude");

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: crate::api::schema::Method::AgentFork(crate::api::schema::AgentForkParams {
                target,
                branch: Some("fork/no-seed".into()),
                base: None,
                path: None,
                label: None,
                pivot: Some(String::new()),
                focus: false,
            }),
        });

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentForked {
            argv,
            seeded,
            worktree,
            ..
        } = success.result
        else {
            panic!("expected agent_forked response");
        };
        assert!(!seeded, "empty pivot must opt out of the configured seed");
        assert_eq!(
            argv,
            vec!["claude", "--resume", "sess-fork", "--fork-session"]
        );

        let remove =
            crate::worktree::build_worktree_remove_command(&repo, Path::new(&worktree.path), true);
        let _ = crate::worktree::run_worktree_command(&remove);
        let _ = std::fs::remove_dir_all(worktree_root);
        let _ = std::fs::remove_dir_all(repo);
        let _ = std::fs::remove_dir_all(bin_dir);
        let _ = std::fs::remove_dir_all(claude_home);
    }

    #[tokio::test]
    async fn api_agent_fork_branches_from_a_linked_worktree_source() {
        let claude_home = fake_claude_home("linked-home", "sess-fork");
        // #124 branch-from-here over the API: the target pane lives in a
        // flock-managed linked worktree; the fork branches from that
        // checkout's HEAD and the source keeps its linked membership.
        let repo = create_committed_repo("api-agent-fork-linked-repo");
        let linked = unique_temp_path("api-agent-fork-linked-child");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "task/linked-source",
                linked.to_str().unwrap(),
            ],
        );
        let worktree_root = unique_temp_path("api-agent-fork-linked-root");
        let bin_dir = stub_claude_on_path("api-agent-fork-linked-bin");
        let mut app = app_with_parent(&linked);
        app.state.worktree_directory = worktree_root.clone();
        app.state.workspaces[0].worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: repo.join(".git").display().to_string(),
            label: "flock-test".into(),
            repo_root: repo.clone(),
            checkout_path: linked.clone(),
            is_linked_worktree: true,
        });
        let target = stamp_agent_session(&mut app, "flock:claude", "claude");

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: crate::api::schema::Method::AgentFork(crate::api::schema::AgentForkParams {
                target,
                branch: Some("fork/from-linked".into()),
                base: None,
                path: None,
                label: None,
                pivot: Some(String::new()),
                focus: false,
            }),
        });

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentForked { worktree, .. } = success.result else {
            panic!("expected agent_forked response: {response}");
        };
        assert_eq!(worktree.branch.as_deref(), Some("fork/from-linked"));
        let source_space = app.state.workspaces[0]
            .worktree_space()
            .expect("source keeps membership");
        assert!(
            source_space.is_linked_worktree,
            "branch-from-here must not demote the source to a parent"
        );
        let child_space = app.state.workspaces[1]
            .worktree_space()
            .expect("child membership");
        assert!(child_space.is_linked_worktree);
        assert_eq!(child_space.repo_root, repo);

        let remove =
            crate::worktree::build_worktree_remove_command(&repo, Path::new(&worktree.path), true);
        let _ = crate::worktree::run_worktree_command(&remove);
        let remove_linked = crate::worktree::build_worktree_remove_command(&repo, &linked, true);
        let _ = crate::worktree::run_worktree_command(&remove_linked);
        let _ = std::fs::remove_dir_all(worktree_root);
        let _ = std::fs::remove_dir_all(repo);
        let _ = std::fs::remove_dir_all(bin_dir);
        let _ = std::fs::remove_dir_all(claude_home);
    }

    #[tokio::test]
    async fn api_agent_fork_refuses_unsupported_agent_and_spawns_nothing() {
        let repo = create_committed_repo("api-agent-fork-codex-repo");
        let worktree_root = unique_temp_path("api-agent-fork-codex-root");
        let mut app = app_with_parent(&repo);
        app.state.worktree_directory = worktree_root.clone();
        let target = stamp_agent_session(&mut app, "flock:codex", "codex");

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: crate::api::schema::Method::AgentFork(crate::api::schema::AgentForkParams {
                target,
                branch: Some("fork/never".into()),
                base: None,
                path: None,
                label: None,
                pivot: None,
                focus: false,
            }),
        });

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "unsupported_for_agent");
        assert!(
            error.error.message.contains("codex"),
            "{}",
            error.error.message
        );
        assert_eq!(app.state.workspaces.len(), 1, "nothing spawned");
        assert!(
            !worktree_root.exists(),
            "no worktree may be created for a refused fork"
        );
        let _ = std::fs::remove_dir_all(repo);
    }

    #[tokio::test]
    async fn api_agent_fork_refuses_when_transcript_is_missing() {
        // #178 / §8.4: a fork whose parent transcript is absent must refuse
        // loudly before any disk mutation — never spawn a doomed pane.
        let repo = create_committed_repo("api-agent-fork-notranscript-repo");
        let worktree_root = unique_temp_path("api-agent-fork-notranscript-root");
        let home = unique_temp_path("api-agent-fork-notranscript-home");
        std::fs::create_dir_all(home.join(".claude/projects")).unwrap();
        std::env::set_var("HOME", &home);
        let mut app = app_with_parent(&repo);
        app.state.worktree_directory = worktree_root.clone();
        let target = stamp_agent_session(&mut app, "flock:claude", "claude");

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: crate::api::schema::Method::AgentFork(crate::api::schema::AgentForkParams {
                target,
                branch: Some("fork/doomed".into()),
                base: None,
                path: None,
                label: None,
                pivot: None,
                focus: false,
            }),
        });

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "transcript_not_found");
        assert!(
            error.error.message.contains("sess-fork"),
            "{}",
            error.error.message
        );
        assert_eq!(app.state.workspaces.len(), 1, "nothing spawned");
        assert!(!worktree_root.exists(), "no worktree created");
        let _ = std::fs::remove_dir_all(repo);
        let _ = std::fs::remove_dir_all(home);
    }

    #[tokio::test]
    async fn api_agent_fork_requires_a_resumable_session() {
        let repo = create_committed_repo("api-agent-fork-nosession-repo");
        let mut app = app_with_parent(&repo);
        let ws = &app.state.workspaces[0];
        let pane_id = ws.focused_pane_id().expect("parent pane");
        let terminal_id = ws
            .pane_state(pane_id)
            .expect("pane state")
            .attached_terminal_id
            .clone();

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: crate::api::schema::Method::AgentFork(crate::api::schema::AgentForkParams {
                target: terminal_id.to_string(),
                branch: None,
                base: None,
                path: None,
                label: None,
                pivot: None,
                focus: false,
            }),
        });

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "no_agent_session");
        assert_eq!(app.state.workspaces.len(), 1);
        let _ = std::fs::remove_dir_all(repo);
    }
}
