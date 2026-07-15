use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{
    state::{
        WorktreeCreateState, WorktreeKillAllState, WorktreeKillRow, WorktreeKillRowStatus,
        WorktreeOpenEntry, WorktreeOpenState, WorktreeRemoveState,
    },
    App, Mode,
};
use crate::events::{AppEvent, WorktreeAddResult, WorktreeKillAllResult, WorktreeRemoveResult};
use crate::worktree::{KillAction, KillFacts};

/// Dry-run label for one sweep row: the branch (or checkout dir name), with a
/// `(main)` / `[adopted]` marker so the batch list reads clearly (#81).
fn kill_all_row_label(
    branch: &Option<String>,
    checkout: &std::path::Path,
    is_main: bool,
    managed: bool,
) -> String {
    let base = branch.clone().unwrap_or_else(|| {
        checkout
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("?")
            .to_string()
    });
    let mut label = if is_main {
        format!("{base} (main)")
    } else {
        base
    };
    if !managed {
        label.push_str(" [adopted]");
    }
    label
}

/// Resolve the branch-session dialog's editable seed (#159) into the fork's
/// opening prompt: substitute the `<branch>` token with the final branch name.
/// An empty seed stays empty, so [`crate::agent_resume::append_pivot_message`]
/// injects nothing — the user's opt-out.
fn resolve_seed_prompt(seed: &str, branch: &str) -> String {
    seed.replace("<branch>", branch.trim())
}

impl App {
    fn worktree_source_metadata(
        &self,
        ws_idx: usize,
    ) -> Result<
        (
            Option<crate::workspace::WorktreeSpaceMembership>,
            crate::workspace::GitSpaceMetadata,
            std::path::PathBuf,
            String,
        ),
        String,
    > {
        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return Err("Workspace not found.".into());
        };
        // #124: a flock-managed linked worktree IS a valid source — the new
        // branch forks from that worktree's HEAD (branch-from-here). Its
        // membership carries the shared repo_root + its own checkout_path, so
        // `git worktree add` runs from the linked checkout. (An ad-hoc, non
        // -membership linked checkout below stays refused — its repo_root is
        // ambiguous.)
        let existing_membership = ws.worktree_space().cloned();

        let git_space = ws.git_space().cloned().or_else(|| {
            ws.resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
                .as_deref()
                .and_then(crate::workspace::git_space_metadata)
        });
        // An ad-hoc linked checkout (no flock-managed membership) stays refused
        // — its repo_root is ambiguous. A membership-backed linked worktree is
        // the #124 branch-from-here case, resolved from the membership below.
        if existing_membership.is_none()
            && git_space
                .as_ref()
                .is_some_and(|metadata| metadata.is_linked_worktree)
        {
            return Err(
                "New and open worktree actions start from the repo parent workspace.".into(),
            );
        }

        let space = existing_membership
            .as_ref()
            .map_or(git_space, |membership| {
                Some(crate::workspace::GitSpaceMetadata {
                    key: membership.key.clone(),
                    checkout_key: membership.checkout_path.display().to_string(),
                    label: membership.label.clone(),
                    repo_root: membership.repo_root.clone(),
                    is_linked_worktree: membership.is_linked_worktree,
                    project_key: crate::workspace::project_key_for_common_dir(
                        std::path::Path::new(&membership.key),
                        &membership.label,
                    ),
                })
            })
            .ok_or_else(|| {
                "Flock worktree actions require a workspace inside a Git work tree.".to_string()
            })?;
        let source_checkout_path = existing_membership
            .as_ref()
            .map(|membership| membership.checkout_path.clone())
            .unwrap_or_else(|| space.repo_root.clone());
        let source_workspace_id = self.state.workspaces[ws_idx].id.clone();
        Ok((
            existing_membership,
            space,
            source_checkout_path,
            source_workspace_id,
        ))
    }

    pub(crate) fn open_new_linked_worktree_dialog(&mut self, ws_idx: usize, base: Option<String>) {
        let (existing_membership, space, source_checkout_path, source_workspace_id) =
            match self.worktree_source_metadata(ws_idx) {
                Ok(metadata) => metadata,
                Err(err) => {
                    self.show_action_notice(err);
                    return;
                }
            };

        let repo_name = space.label.clone();
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_micros().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0);
        let branch = crate::worktree::generated_branch_slug(seed);
        let checkout_path = crate::worktree::default_checkout_path(
            &self.state.worktree_directory,
            &repo_name,
            &branch,
        );

        crate::logging::worktree_dialog_opened(
            ws_idx,
            &space.repo_root.display().to_string(),
            &branch,
            &checkout_path.display().to_string(),
        );
        self.state.selected = ws_idx;
        self.state.worktree_create = Some(WorktreeCreateState {
            branch_plan: None,
            source_workspace_id,
            source_checkout_path,
            source_existing_membership: existing_membership,
            source_repo_root: space.repo_root,
            repo_key: space.key,
            repo_name,
            branch_input: crate::app::line_editor::LineEditor::new(branch.clone()),
            branch,
            base: base.unwrap_or_else(|| "HEAD".into()),
            checkout_path,
            seed: crate::app::line_editor::LineEditor::default(),
            focus: crate::app::state::WorktreeCreateFocus::Branch,
            error: None,
            creating: false,
        });
        self.state.mode = Mode::NewLinkedWorktree;
    }

    /// Branch the focused pane's agent session into a new worktree: same
    /// dialog as new-worktree, but the created workspace's root pane resumes
    /// a fork of the session instead of starting a shell.
    pub(crate) fn open_branch_session_dialog(&mut self, ws_idx: usize) {
        let Some(plan) = self.focused_branch_plan(ws_idx) else {
            let notice = self.branch_unavailable_notice(ws_idx);
            self.show_action_notice(notice);
            return;
        };
        self.open_new_linked_worktree_dialog(ws_idx, None);
        // Pre-fill the editable seed with the pivot template (#159). The
        // `<branch>` token is kept verbatim and resolved at confirm, so it
        // tracks the branch name even if the user edits it. Empty config =>
        // empty field => no seed, same as today.
        let pivot_template = self.state.branch_pivot_message.clone();
        if let Some(create) = self.state.worktree_create.as_mut() {
            create.branch_plan = Some(plan);
            // Fresh dialog => empty seed; guard so a re-open can't clobber edits.
            if create.seed.is_empty() {
                create.seed.set(pivot_template);
            }
        }
    }

    /// Diagnostic notice for a branch-session attempt that found no resumable
    /// plan. A pane running a *detected* agent that still has no session ref
    /// almost always means the agent integration hook isn't installed on this
    /// host (e.g. a read-only, nix-managed `~/.claude/settings.json` that never
    /// got flock's hooks) — so the agent never reports its session id. Point at
    /// `flk integration status` instead of the flat "no resumable agent
    /// session", which sent a real debugging session down the wrong path.
    fn branch_unavailable_notice(&self, ws_idx: usize) -> String {
        let agent = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| Some((ws, ws.focused_pane_id()?)))
            .and_then(|(ws, pane_id)| ws.pane_state(pane_id))
            .and_then(|pane| self.state.terminals.get(&pane.attached_terminal_id))
            .and_then(|terminal| terminal.effective_agent_label());
        match agent {
            Some(agent) => format!(
                "branch session: {agent} reports no resumable session here — check `flk integration status`"
            ),
            None => "branch session: focused pane has no resumable agent session".to_string(),
        }
    }

    /// Resolve a fork-aware resume plan for the focused pane of `ws_idx`.
    /// Prefers the live hook-authority session over the persisted one.
    fn focused_branch_plan(&self, ws_idx: usize) -> Option<crate::agent_resume::AgentResumePlan> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let pane_id = ws.focused_pane_id()?;
        let pane = ws.pane_state(pane_id)?;
        let terminal = self.state.terminals.get(&pane.attached_terminal_id)?;
        let info = super::creation::terminal_agent_session_info(terminal)?;
        let session_ref = crate::agent_resume::AgentSessionRef {
            kind: info.kind,
            value: info.value,
        };
        crate::agent_resume::branch_plan(&info.source, &info.agent, &session_ref)
    }

    pub(crate) fn open_remove_linked_worktree_confirmation(&mut self, ws_idx: usize) {
        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return;
        };
        if !ws
            .worktree_space()
            .is_some_and(|space| space.is_linked_worktree)
        {
            self.state.config_diagnostic =
                Some("This workspace is not a Flock-managed worktree checkout.".into());
            return;
        }
        let Some(space) = ws.worktree_space().cloned() else {
            return;
        };
        self.state.selected = ws_idx;
        self.state.worktree_remove = Some(WorktreeRemoveState {
            managed: true,
            workspace_id: ws.id.clone(),
            repo_root: space.repo_root,
            path: space.checkout_path,
            error: None,
            removing: false,
            force_confirmation: false,
            delete_branch: false,
            branch: None,
            merge_gate: None,
            branch_protected: false,
            gate_timed_out: false,
        });
        self.state.mode = Mode::ConfirmRemoveWorktree;
    }

    /// Kill flow: like remove, but also deletes the local branch when the
    /// async merge gate (gh pr view / git branch --merged) passes. Flock
    /// never deletes branches otherwise, so this needs positive evidence.
    pub(crate) fn open_kill_worktree_confirmation(&mut self, ws_idx: usize) {
        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return;
        };
        // Flock-managed membership is just bookkeeping; any linked git
        // worktree (created by an agent, by hand, by another tool) gets the
        // same merge-gated kill. Only non-worktree checkouts are refused —
        // the main checkout is never killable.
        let managed_space = ws
            .worktree_space()
            .filter(|space| space.is_linked_worktree)
            .cloned();
        let (repo_root, checkout, managed) = match managed_space {
            Some(space) => (space.repo_root, space.checkout_path, true),
            None => {
                let git_space = ws.git_space().cloned().or_else(|| {
                    ws.resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
                        .as_deref()
                        .and_then(crate::workspace::git_space_metadata)
                });
                match git_space {
                    Some(space) if space.is_linked_worktree => {
                        let main_root = crate::worktree::main_root_from_common_dir(
                            std::path::Path::new(&space.key),
                        );
                        (main_root, space.repo_root, false)
                    }
                    _ => {
                        self.show_action_notice(
                            "kill worktree: this workspace is not a linked git worktree checkout",
                        );
                        return;
                    }
                }
            }
        };
        self.state.selected = ws_idx;
        self.state.worktree_remove = Some(WorktreeRemoveState {
            managed,
            workspace_id: ws.id.clone(),
            repo_root: repo_root.clone(),
            path: checkout.clone(),
            error: None,
            removing: false,
            force_confirmation: false,
            delete_branch: true,
            branch: None,
            merge_gate: None,
            branch_protected: false,
            gate_timed_out: false,
        });
        self.state.mode = Mode::ConfirmRemoveWorktree;

        let workspace_id = ws.id.clone();
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let branch = crate::worktree::checkout_branch_name(&checkout);
            let (gate, timed_out) = match branch.clone() {
                Some(branch) => crate::worktree::branch_merge_gate_with_timeout(
                    repo_root.clone(),
                    checkout.clone(),
                    branch,
                ),
                None => (crate::worktree::WorktreeMergeGate::NotMerged, false),
            };
            let _ = event_tx.blocking_send(AppEvent::WorktreeKillGateFinished(
                crate::events::WorktreeKillGateResult {
                    workspace_id,
                    path: checkout,
                    branch,
                    gate,
                    timed_out,
                },
            ));
        });
    }

    pub(crate) fn handle_worktree_kill_gate_finished(
        &mut self,
        result: crate::events::WorktreeKillGateResult,
    ) {
        // The fleet sweep (#81) shares the gate event/worker; route to its row
        // when its dialog is open.
        if self.state.worktree_kill_all.is_some() {
            self.apply_kill_all_gate(result);
            return;
        }
        // #121: is this the repo's default (or a configured protected) branch?
        // Decide BEFORE borrowing `remove` mutably — needs `remove.repo_root`
        // and `config`, both immutable borrows of `self.state`. `is_protected_
        // branch` carries the main/master floor even if detection returns None.
        let protected = match self.state.worktree_remove.as_ref() {
            Some(remove)
                if remove.workspace_id == result.workspace_id && remove.path == result.path =>
            {
                result.branch.as_deref().is_some_and(|branch| {
                    let default = crate::worktree::detect_default_branch(&remove.repo_root);
                    crate::worktree::is_protected_branch(
                        branch,
                        default.as_deref(),
                        &self.state.config.worktrees.protected_branches,
                    )
                })
            }
            _ => false,
        };

        let Some(remove) = &mut self.state.worktree_remove else {
            return;
        };
        if !remove.delete_branch
            || remove.workspace_id != result.workspace_id
            || remove.path != result.path
        {
            return;
        }
        crate::logging::worktree_kill_merge_gate_resolved(
            &result.workspace_id.to_string(),
            result.branch.as_deref().unwrap_or("<detached>"),
            &format!("{:?}", result.gate),
        );
        remove.branch = result.branch;
        remove.merge_gate = Some(result.gate);
        // A protected branch — the repo's default, the main/master floor, or a
        // configured `protected_branches` entry (#121, `is_protected_branch`
        // above) — is kept regardless of merge evidence: the gate treats the
        // default branch as "merged" (trivially contained in every downstream
        // remote ref), which would otherwise build `git branch -D <default>`.
        // Force checkout-only and flag the dialog; the checkout folder can
        // still be removed.
        if protected {
            remove.delete_branch = false;
            remove.branch_protected = true;
        }
        remove.gate_timed_out = result.timed_out;
        self.render_dirty.store(true, Ordering::Release);
        self.render_notify.notify_one();
    }

    /// Fleet-wide sweep (#81): enumerate every worktree flock knows (managed,
    /// adopted, external, plus main checkouts), classify each into its tier, and
    /// open the batch-confirm dialog. Linked rows resolve their merge gate async
    /// (the confirm is held until the whole plan is known).
    pub(crate) fn open_kill_all_worktrees_confirmation(&mut self) {
        let mut rows: Vec<WorktreeKillRow> = Vec::new();
        for ws in &self.state.workspaces {
            let candidate = if let Some(space) =
                ws.worktree_space().filter(|space| space.is_linked_worktree)
            {
                Some((
                    space.repo_root.clone(),
                    space.checkout_path.clone(),
                    true,
                    false,
                ))
            } else {
                let git_space = ws.git_space().cloned().or_else(|| {
                    ws.resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
                        .as_deref()
                        .and_then(crate::workspace::git_space_metadata)
                });
                match git_space {
                    Some(space) if space.is_linked_worktree => {
                        let main_root = crate::worktree::main_root_from_common_dir(
                            std::path::Path::new(&space.key),
                        );
                        Some((main_root, space.repo_root, false, false))
                    }
                    // A non-linked git workspace is the main checkout.
                    Some(space) => Some((space.repo_root.clone(), space.repo_root, true, true)),
                    // Not a git workspace at all — not part of the sweep.
                    None => None,
                }
            };
            let Some((repo_root, checkout, managed, is_main)) = candidate else {
                continue;
            };
            let working_agent = ws.has_working_pane(&self.state.terminals);
            // Unknown dirtiness ⇒ treat as dirty: an unmerged row then skips
            // (safe), and a merged row only force-removes scratch the merge gate
            // already proved redundant — never committed work.
            let dirty = crate::worktree::checkout_is_dirty(&checkout).unwrap_or(true);
            let branch = crate::worktree::checkout_branch_name(&checkout);
            // Main rows need no gate and resolve immediately; linked rows wait.
            let merge_gate = is_main.then_some(crate::worktree::WorktreeMergeGate::NotMerged);
            let tier = crate::worktree::classify_kill_tier(KillFacts {
                is_main,
                working_agent,
                dirty,
                merged: false,
            });
            rows.push(WorktreeKillRow {
                workspace_id: ws.id.clone(),
                label: kill_all_row_label(&branch, &checkout, is_main, managed),
                repo_root,
                checkout,
                managed,
                branch,
                dirty,
                working_agent,
                merge_gate,
                tier,
                status: WorktreeKillRowStatus::Pending,
            });
        }

        if rows.is_empty() {
            self.show_action_notice("kill all: no worktrees to sweep");
            return;
        }

        // Resolve the merge gate for each linked row that could still be killed
        // (agent-busy rows are already a protected skip — don't dial gh for them).
        for row in &rows {
            if row.checkout_is_main() || row.working_agent {
                continue;
            }
            let workspace_id = row.workspace_id.clone();
            let repo_root = row.repo_root.clone();
            let checkout = row.checkout.clone();
            let branch = row.branch.clone();
            let event_tx = self.event_tx.clone();
            std::thread::spawn(move || {
                let (gate, timed_out) = match branch.clone() {
                    Some(branch) => crate::worktree::branch_merge_gate_with_timeout(
                        repo_root.clone(),
                        checkout.clone(),
                        branch,
                    ),
                    None => (crate::worktree::WorktreeMergeGate::NotMerged, false),
                };
                let _ = event_tx.blocking_send(AppEvent::WorktreeKillGateFinished(
                    crate::events::WorktreeKillGateResult {
                        workspace_id,
                        path: checkout,
                        branch,
                        gate,
                        timed_out,
                    },
                ));
            });
        }

        self.state.worktree_kill_all = Some(WorktreeKillAllState {
            rows,
            executing: false,
            force_dirty: false,
        });
        self.state.mode = Mode::ConfirmKillAllWorktrees;
    }

    /// Fold one resolved merge gate into its sweep row and recompute its tier.
    fn apply_kill_all_gate(&mut self, result: crate::events::WorktreeKillGateResult) {
        let Some(kill_all) = &mut self.state.worktree_kill_all else {
            return;
        };
        let Some(row) = kill_all
            .rows
            .iter_mut()
            .find(|row| row.workspace_id == result.workspace_id && row.checkout == result.path)
        else {
            return;
        };
        let merged = matches!(
            result.gate,
            crate::worktree::WorktreeMergeGate::Merged { .. }
        );
        row.branch = result.branch;
        row.merge_gate = Some(result.gate);
        row.tier = crate::worktree::classify_kill_tier(KillFacts {
            is_main: row.checkout_is_main(),
            working_agent: row.working_agent,
            dirty: row.dirty,
            merged,
        });
        self.render_dirty.store(true, Ordering::Release);
        self.render_notify.notify_one();
    }

    /// Key handling for the batch-confirm dialog: Esc cancels, `f` toggles the
    /// force escalation (unmerged-dirty → checkout-only), Enter executes once
    /// the whole plan is resolved.
    pub(crate) fn handle_worktree_kill_all_key(&mut self, key: KeyEvent) {
        let Some(kill_all) = &mut self.state.worktree_kill_all else {
            return;
        };
        if kill_all.executing {
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.state.worktree_kill_all = None;
                self.state.mode = if self.state.active.is_some() {
                    Mode::Terminal
                } else {
                    Mode::Navigate
                };
            }
            KeyCode::Char('f') | KeyCode::Char('F') => {
                kill_all.force_dirty = !kill_all.force_dirty;
                self.render_dirty.store(true, Ordering::Release);
                self.render_notify.notify_one();
            }
            KeyCode::Enter if !kill_all.resolving() => self.start_kill_all_worktrees(),
            _ => {}
        }
    }

    /// Execute the sweep: fire the checkout removals (+ branch deletes for merged
    /// rows) on a worker thread; main-checkout "close pane" rows and the final
    /// workspace closes are applied when the worker reports back.
    pub(crate) fn start_kill_all_worktrees(&mut self) {
        let Some(kill_all) = &mut self.state.worktree_kill_all else {
            return;
        };
        if kill_all.executing || kill_all.resolving() {
            return;
        }
        let force = kill_all.force_dirty;
        // (workspace_id, repo_root, checkout, branch_to_delete, force_remove)
        let mut jobs: Vec<(
            String,
            std::path::PathBuf,
            std::path::PathBuf,
            Option<String>,
            bool,
        )> = Vec::new();
        let mut acted = false;
        for row in &mut kill_all.rows {
            match crate::worktree::planned_action(row.tier, force) {
                KillAction::KillBranch { dirty } => {
                    row.status = WorktreeKillRowStatus::Removing;
                    jobs.push((
                        row.workspace_id.clone(),
                        row.repo_root.clone(),
                        row.checkout.clone(),
                        row.branch.clone(),
                        dirty,
                    ));
                    acted = true;
                }
                KillAction::CheckoutOnly => {
                    row.status = WorktreeKillRowStatus::Removing;
                    // Force-remove only when dirty (the forced unmerged-dirty case).
                    jobs.push((
                        row.workspace_id.clone(),
                        row.repo_root.clone(),
                        row.checkout.clone(),
                        None,
                        row.dirty,
                    ));
                    acted = true;
                }
                KillAction::ClosePane => acted = true,
                KillAction::Skip => {}
            }
        }

        if !acted {
            self.state.worktree_kill_all = None;
            self.state.mode = if self.state.active.is_some() {
                Mode::Terminal
            } else {
                Mode::Navigate
            };
            self.show_action_notice("kill all: nothing eligible");
            return;
        }

        kill_all.executing = true;
        self.render_dirty.store(true, Ordering::Release);
        self.render_notify.notify_one();

        if jobs.is_empty() {
            // Only close-pane rows — no git work; finalize immediately.
            self.handle_worktree_kill_all_finished(WorktreeKillAllResult {
                outcomes: Vec::new(),
            });
            return;
        }

        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let mut outcomes = Vec::new();
            for (ws_id, repo_root, checkout, branch_to_delete, force_remove) in jobs {
                let command = crate::worktree::build_worktree_remove_command(
                    &repo_root,
                    &checkout,
                    force_remove,
                );
                let outcome = match crate::worktree::run_worktree_command(&command) {
                    Ok(()) => {
                        if let Some(branch) = branch_to_delete {
                            // Best-effort: the checkout is already gone; a failed
                            // branch delete is surfaced via tracing, not a row error.
                            if let Err(err) =
                                crate::worktree::delete_local_branch(&repo_root, &branch)
                            {
                                tracing::warn!(branch, err, "kill all: branch delete failed");
                            }
                        }
                        Ok(())
                    }
                    Err(err) => Err(err),
                };
                outcomes.push((ws_id, outcome));
            }
            let _ =
                event_tx.blocking_send(AppEvent::WorktreeKillAllFinished(WorktreeKillAllResult {
                    outcomes,
                }));
        });
    }

    /// Finalize the sweep: record per-row results, then close every workspace
    /// whose checkout was removed and every clean+idle main checkout, and report
    /// a one-line summary.
    pub(crate) fn handle_worktree_kill_all_finished(&mut self, result: WorktreeKillAllResult) {
        let Some(kill_all) = &mut self.state.worktree_kill_all else {
            return;
        };
        let force = kill_all.force_dirty;
        let mut close_ws_ids: Vec<String> = Vec::new();
        let mut errors = 0usize;
        for (ws_id, outcome) in &result.outcomes {
            if let Some(row) = kill_all.rows.iter_mut().find(|r| &r.workspace_id == ws_id) {
                match outcome {
                    Ok(()) => {
                        row.status = WorktreeKillRowStatus::Done;
                        close_ws_ids.push(ws_id.clone());
                    }
                    Err(err) => {
                        row.status = WorktreeKillRowStatus::Error(err.clone());
                        errors += 1;
                    }
                }
            }
        }
        let removed = close_ws_ids.len();
        let mut closed_panes = 0usize;
        for row in &kill_all.rows {
            if matches!(
                crate::worktree::planned_action(row.tier, force),
                KillAction::ClosePane
            ) {
                close_ws_ids.push(row.workspace_id.clone());
                closed_panes += 1;
            }
        }

        let indices: Vec<usize> = close_ws_ids
            .iter()
            .filter_map(|id| self.state.workspaces.iter().position(|ws| &ws.id == id))
            .collect();
        self.state.worktree_kill_all = None;
        if !indices.is_empty() {
            self.state.close_workspace_indices(indices);
        }
        self.state.mode = if self.state.active.is_some() {
            Mode::Terminal
        } else {
            Mode::Navigate
        };
        self.show_action_notice(format!(
            "kill all: {removed} removed · {closed_panes} pane(s) closed · {errors} error(s)"
        ));
    }

    pub(crate) fn handle_worktree_branch_delete_finished(
        &mut self,
        result: crate::events::WorktreeBranchDeleteResult,
    ) {
        match result.result {
            Ok(()) => {
                crate::logging::worktree_branch_deleted(&result.branch);
            }
            Err(message) => {
                crate::logging::worktree_branch_delete_failed(&result.branch, &message);
                self.show_action_notice(format!(
                    "removed checkout, but failed to delete branch {}: {message}",
                    result.branch
                ));
                self.render_dirty.store(true, Ordering::Release);
                self.render_notify.notify_one();
            }
        }
    }

    pub(crate) fn open_existing_worktree_dialog(&mut self, ws_idx: usize) {
        let (existing_membership, space, source_checkout_path, source_workspace_id) =
            match self.worktree_source_metadata(ws_idx) {
                Ok(metadata) => metadata,
                Err(err) => {
                    self.show_action_notice(err);
                    return;
                }
            };

        let list = match crate::worktree::list_existing_worktrees(&space.repo_root) {
            Ok(list) => list,
            Err(err) => {
                self.state.config_diagnostic = Some(err);
                return;
            }
        };
        let entries = list
            .into_iter()
            .filter(|entry| !entry.is_bare && !entry.is_prunable)
            .map(|entry| {
                let entry_checkout_path = crate::worktree::canonical_or_original(&entry.path);
                let entry_checkout_key = entry_checkout_path.display().to_string();
                let repo_checkout_path = crate::worktree::canonical_or_original(&space.repo_root);
                let already_open_ws_idx = self.state.workspaces.iter().position(|ws| {
                    if let Some(membership) = ws.worktree_space() {
                        return crate::worktree::canonical_or_original(&membership.checkout_path)
                            == entry_checkout_path;
                    }

                    let git_space = ws.git_space().cloned().or_else(|| {
                        ws.resolved_identity_cwd_from(
                            &self.state.terminals,
                            &self.terminal_runtimes,
                        )
                        .as_deref()
                        .and_then(crate::workspace::git_space_metadata)
                    });
                    if git_space
                        .as_ref()
                        .is_some_and(|metadata| metadata.checkout_key == entry_checkout_key)
                    {
                        return true;
                    }

                    ws.resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
                        .as_deref()
                        .is_some_and(|cwd| {
                            crate::worktree::canonical_or_original(cwd) == entry_checkout_path
                        })
                });
                WorktreeOpenEntry {
                    is_linked_worktree: entry_checkout_path != repo_checkout_path,
                    path: entry.path,
                    branch: entry.branch,
                    already_open_ws_idx,
                }
            })
            .collect::<Vec<_>>();

        if entries.is_empty() {
            self.show_action_notice("No Git worktrees found for this repo.");
            return;
        }

        self.state.selected = ws_idx;
        self.state.worktree_open = Some(WorktreeOpenState {
            source_workspace_id,
            source_existing_membership: existing_membership,
            source_checkout_path,
            source_repo_root: space.repo_root,
            repo_key: space.key,
            repo_name: space.label,
            entries,
            selected: 0,
            query: String::new(),
            search_focused: false,
            error: None,
        });
        self.state.mode = Mode::OpenExistingWorktree;
    }

    pub(crate) fn handle_worktree_create_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                if self
                    .state
                    .worktree_create
                    .as_ref()
                    .is_some_and(|create| create.creating)
                {
                    return;
                }
                self.close_worktree_create_dialog();
            }
            KeyCode::Enter => self.start_worktree_add(),
            KeyCode::Tab => self.toggle_worktree_create_focus(),
            _ => self.edit_focused_worktree_field(key),
        }
    }

    /// Route an editing key to the focused line editor (branch or seed), with
    /// readline-style bindings (#159): arrows / Home / End / ^A / ^E to move,
    /// ^W word-delete, ^U delete-to-start, Delete forward. A branch edit
    /// re-syncs the checkout path.
    fn edit_focused_worktree_field(&mut self, key: KeyEvent) {
        use crossterm::event::KeyModifiers;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let editing_branch = !self.seed_field_focused();
        let Some(create) = self.state.worktree_create.as_mut() else {
            return;
        };
        let editor = if editing_branch {
            &mut create.branch_input
        } else {
            &mut create.seed
        };

        let mut mutated = false;
        match key.code {
            KeyCode::Char('a') if ctrl => editor.home(),
            KeyCode::Char('e') if ctrl => editor.end(),
            KeyCode::Char('u') if ctrl => {
                editor.delete_to_start();
                mutated = true;
            }
            KeyCode::Char('w') if ctrl => {
                editor.delete_word_back();
                mutated = true;
            }
            KeyCode::Char(c) if !ctrl => {
                editor.insert(c);
                mutated = true;
            }
            KeyCode::Backspace if ctrl => {
                editor.delete_word_back();
                mutated = true;
            }
            KeyCode::Backspace => {
                editor.backspace();
                mutated = true;
            }
            KeyCode::Delete => {
                editor.delete();
                mutated = true;
            }
            KeyCode::Left => editor.left(),
            KeyCode::Right => editor.right(),
            KeyCode::Home => editor.home(),
            KeyCode::End => editor.end(),
            _ => {}
        }

        // Only a branch-name change moves the checkout path.
        if mutated && editing_branch {
            self.sync_worktree_branch_from_input();
        }
    }

    /// The seed-prompt row only exists for a branch-session (branch_plan set),
    /// so Tab only moves focus there; a plain new-worktree stays on the branch.
    fn toggle_worktree_create_focus(&mut self) {
        use crate::app::state::WorktreeCreateFocus;
        if let Some(create) = self.state.worktree_create.as_mut() {
            if create.branch_plan.is_none() {
                return;
            }
            create.focus = match create.focus {
                WorktreeCreateFocus::Branch => WorktreeCreateFocus::Seed,
                WorktreeCreateFocus::Seed => WorktreeCreateFocus::Branch,
            };
        }
    }

    /// True when keystrokes should edit the seed prompt rather than the branch
    /// name: the seed field exists (branch-session) AND holds focus.
    fn seed_field_focused(&self) -> bool {
        self.state.worktree_create.as_ref().is_some_and(|create| {
            create.branch_plan.is_some()
                && create.focus == crate::app::state::WorktreeCreateFocus::Seed
        })
    }

    pub(crate) fn handle_worktree_open_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.state.worktree_open = None;
                self.state.mode = if self.state.active.is_some() {
                    Mode::Terminal
                } else {
                    Mode::Navigate
                };
            }
            KeyCode::Up => {
                if let Some(open) = &mut self.state.worktree_open {
                    open.select_previous_filtered();
                }
            }
            KeyCode::Down => {
                if let Some(open) = &mut self.state.worktree_open {
                    open.select_next_filtered();
                }
            }
            KeyCode::Char('/') => {
                if let Some(open) = &mut self.state.worktree_open {
                    if open.search_focused {
                        open.query.push('/');
                        open.normalize_selection();
                    } else {
                        open.search_focused = true;
                    }
                }
            }
            KeyCode::Char(ch)
                if self
                    .state
                    .worktree_open
                    .as_ref()
                    .is_some_and(|open| open.search_focused)
                    && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT) =>
            {
                if let Some(open) = &mut self.state.worktree_open {
                    if !ch.is_control() {
                        open.query.push(ch);
                        open.normalize_selection();
                    }
                }
            }
            KeyCode::Backspace
                if self
                    .state
                    .worktree_open
                    .as_ref()
                    .is_some_and(|open| open.search_focused) =>
            {
                if let Some(open) = &mut self.state.worktree_open {
                    open.query.pop();
                    open.normalize_selection();
                }
            }
            KeyCode::Enter => self.open_selected_existing_worktree(),
            _ => {}
        }
    }

    pub(crate) fn open_selected_existing_worktree(&mut self) {
        let Some(open) = self.state.worktree_open.as_ref() else {
            return;
        };
        let Some(entry_idx) = open.selected_entry_index() else {
            return;
        };
        let Some(entry) = open.entries.get(entry_idx).cloned() else {
            return;
        };
        let source_workspace_id = open.source_workspace_id.clone();
        let source_existing_membership = open.source_existing_membership.clone();
        let source_checkout_path = open.source_checkout_path.clone();
        let source_repo_root = open.source_repo_root.clone();
        let repo_key = open.repo_key.clone();
        let repo_name = open.repo_name.clone();
        self.state.worktree_open = None;

        if let Some(ws_idx) = entry.already_open_ws_idx {
            self.mark_opened_existing_worktree_membership(
                &source_workspace_id,
                source_existing_membership,
                source_checkout_path,
                source_repo_root,
                repo_key,
                repo_name,
                ws_idx,
                entry.path,
                entry.is_linked_worktree,
            );
            self.state.switch_workspace(ws_idx);
            self.state.mode = Mode::Terminal;
            return;
        }

        match self.create_workspace_with_options(entry.path.clone(), true) {
            Ok(new_ws_idx) => {
                self.mark_opened_existing_worktree_membership(
                    &source_workspace_id,
                    source_existing_membership,
                    source_checkout_path,
                    source_repo_root,
                    repo_key,
                    repo_name,
                    new_ws_idx,
                    entry.path,
                    entry.is_linked_worktree,
                );
            }
            Err(err) => {
                self.state.worktree_open = Some(WorktreeOpenState {
                    source_workspace_id,
                    source_existing_membership,
                    source_checkout_path,
                    source_repo_root,
                    repo_key,
                    repo_name,
                    entries: vec![entry],
                    selected: 0,
                    query: String::new(),
                    search_focused: false,
                    error: Some(format!("failed to open worktree: {err}")),
                });
                self.state.mode = Mode::OpenExistingWorktree;
            }
        }
    }

    // The caller has already extracted the open-worktree dialog state; keeping the
    // membership fields explicit here avoids borrowing AppState across workspace creation.
    #[allow(clippy::too_many_arguments)]
    fn mark_opened_existing_worktree_membership(
        &mut self,
        source_workspace_id: &str,
        source_existing_membership: Option<crate::workspace::WorktreeSpaceMembership>,
        source_checkout_path: std::path::PathBuf,
        source_repo_root: std::path::PathBuf,
        repo_key: String,
        repo_name: String,
        target_ws_idx: usize,
        target_path: std::path::PathBuf,
        target_is_linked_worktree: bool,
    ) {
        if let Some(source_ws_idx) = self
            .state
            .workspaces
            .iter()
            .position(|ws| ws.id == source_workspace_id)
        {
            if let Some(source_membership) = source_existing_membership {
                self.state.workspaces[source_ws_idx].worktree_space = Some(source_membership);
            } else {
                self.state.workspaces[source_ws_idx].worktree_space =
                    Some(crate::workspace::WorktreeSpaceMembership {
                        key: repo_key.clone(),
                        label: repo_name.clone(),
                        repo_root: source_repo_root.clone(),
                        checkout_path: source_checkout_path,
                        is_linked_worktree: false,
                    });
            }
        }
        if let Some(target) = self.state.workspaces.get_mut(target_ws_idx) {
            target.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
                key: repo_key,
                label: repo_name,
                repo_root: source_repo_root,
                checkout_path: target_path,
                is_linked_worktree: target_is_linked_worktree,
            });
        }
        self.state.mark_session_dirty();
    }

    fn close_worktree_create_dialog(&mut self) {
        // The dialog owns its editors in `worktree_create` (#159); dropping it
        // is the whole reset — the shared `name_input` is untouched here.
        self.state.worktree_create = None;
        self.state.mode = if self.state.active.is_some() {
            Mode::Terminal
        } else {
            Mode::Navigate
        };
    }

    fn sync_worktree_branch_from_input(&mut self) {
        let Some(create) = &mut self.state.worktree_create else {
            return;
        };
        create.branch = create.branch_input.value().to_string();
        create.checkout_path = crate::worktree::default_checkout_path(
            &self.state.worktree_directory,
            &create.repo_name,
            &create.branch,
        );
        create.error = None;
    }

    pub(crate) fn start_worktree_add(&mut self) {
        self.sync_worktree_branch_from_input();
        let Some(create) = &mut self.state.worktree_create else {
            return;
        };
        let branch = create.branch.trim().to_string();
        if branch.is_empty() {
            create.error = Some("branch is required".into());
            return;
        }
        if create.creating {
            return;
        }

        create.branch = branch.clone();
        create.checkout_path = crate::worktree::default_checkout_path(
            &self.state.worktree_directory,
            &create.repo_name,
            &branch,
        );
        create.creating = true;
        create.error = None;

        let command = crate::worktree::build_worktree_add_new_branch_command(
            &create.source_checkout_path,
            &create.checkout_path,
            &create.branch,
            &create.base,
        );
        let parent_dir = create
            .checkout_path
            .parent()
            .map(std::path::Path::to_path_buf);
        crate::logging::worktree_add_started(
            &create.source_repo_root.display().to_string(),
            &create.branch,
            &create.checkout_path.display().to_string(),
        );
        let path = create.checkout_path.clone();
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let result = if let Some(parent_dir) = parent_dir {
                std::fs::create_dir_all(&parent_dir)
                    .map_err(|err| err.to_string())
                    .and_then(|()| crate::worktree::run_worktree_command(&command))
            } else {
                crate::worktree::run_worktree_command(&command)
            };
            let _ = event_tx.blocking_send(AppEvent::WorktreeAddFinished(WorktreeAddResult {
                path,
                result,
            }));
        });
    }

    pub(crate) fn handle_worktree_remove_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                if self
                    .state
                    .worktree_remove
                    .as_ref()
                    .is_some_and(|remove| remove.removing)
                {
                    return;
                }
                self.state.worktree_remove = None;
                self.state.mode = if self.state.active.is_some() {
                    Mode::Terminal
                } else {
                    Mode::Navigate
                };
            }
            KeyCode::Enter => self.start_worktree_remove(),
            _ => {}
        }
    }

    pub(crate) fn start_worktree_remove(&mut self) {
        let Some(remove) = &mut self.state.worktree_remove else {
            return;
        };
        if remove.removing {
            return;
        }
        // Kill flow: wait for the merge gate before allowing confirmation, so
        // the user always sees what will actually be deleted.
        if remove.delete_branch && remove.merge_gate.is_none() {
            return;
        }
        remove.removing = true;
        remove.error = None;
        let force = remove.force_confirmation;

        let command =
            crate::worktree::build_worktree_remove_command(&remove.repo_root, &remove.path, force);
        crate::logging::worktree_remove_started(
            &remove.workspace_id.to_string(),
            &remove.path.display().to_string(),
            force,
        );
        let path = remove.path.clone();
        let workspace_id = remove.workspace_id.clone();
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let result = crate::worktree::run_worktree_command(&command);
            let _ =
                event_tx.blocking_send(AppEvent::WorktreeRemoveFinished(WorktreeRemoveResult {
                    workspace_id,
                    path,
                    result,
                }));
        });
    }

    pub(crate) fn handle_worktree_add_finished(&mut self, result: WorktreeAddResult) {
        let Some(create) = &mut self.state.worktree_create else {
            return;
        };
        if create.checkout_path != result.path {
            return;
        }

        match result.result {
            Ok(()) => {
                crate::logging::worktree_add_completed(&create.checkout_path.display().to_string());
                let path = create.checkout_path.clone();
                let branch_name = create.branch.clone();
                let branch_plan = create.branch_plan.clone();
                let source_workspace_id = create.source_workspace_id.clone();
                let source_checkout_path = create.source_checkout_path.clone();
                let source_existing_membership = create.source_existing_membership.clone();
                let repo_key = create.repo_key.clone();
                let repo_name = create.repo_name.clone();
                let source_repo_root = create.source_repo_root.clone();
                let seed_prompt = create.seed.value().to_string();
                self.state.worktree_create = None;
                let created = if let Some(mut plan) = branch_plan {
                    // #106/#159: inject the one-shot pivot prompt as the fork's
                    // first turn. The prompt is the dialog's editable seed
                    // field (pre-filled from `branch_pivot_message`), with
                    // `<branch>` resolved to the final branch name here so the
                    // token tracks any edit to the branch. Empty seed or a
                    // non-claude fork => no-op.
                    let pivot = resolve_seed_prompt(&seed_prompt, &branch_name);
                    crate::agent_resume::append_pivot_message(&mut plan, &pivot);
                    let (rows, cols) = self.state.estimate_pane_size();
                    self.spawn_agent_workspace(path.clone(), rows, cols, &plan.argv, true)
                        .map(|(ws_idx, _, _)| ws_idx)
                        .map_err(|err| match err {
                            super::agents::AgentStartError::SpawnFailed(message) => message,
                            _ => "agent spawn rejected".to_string(),
                        })
                } else {
                    self.create_workspace_with_options(path.clone(), true)
                        .map_err(|err| err.to_string())
                };
                match created {
                    Ok(ws_idx) => {
                        let source_membership = source_existing_membership.unwrap_or(
                            crate::workspace::WorktreeSpaceMembership {
                                key: repo_key.clone(),
                                label: repo_name.clone(),
                                repo_root: source_repo_root.clone(),
                                checkout_path: source_checkout_path,
                                is_linked_worktree: false,
                            },
                        );
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
                                key: repo_key,
                                label: repo_name,
                                repo_root: source_repo_root,
                                checkout_path: path,
                                is_linked_worktree: true,
                            });
                        }
                        self.state.mark_session_dirty();
                    }
                    Err(err) => {
                        self.state.config_diagnostic = Some(format!(
                            "created worktree but failed to open workspace: {err}"
                        ));
                        self.state.mode = Mode::Navigate;
                    }
                }
                self.render_dirty.store(true, Ordering::Release);
                self.render_notify.notify_one();
            }
            Err(message) => {
                crate::logging::worktree_add_failed(
                    &create.checkout_path.display().to_string(),
                    &message,
                );
                create.creating = false;
                create.error = Some(message);
                self.render_dirty.store(true, Ordering::Release);
                self.render_notify.notify_one();
            }
        }
    }
    pub(crate) fn handle_worktree_remove_finished(&mut self, result: WorktreeRemoveResult) {
        let Some(remove) = &mut self.state.worktree_remove else {
            return;
        };
        if remove.workspace_id != result.workspace_id || remove.path != result.path {
            return;
        }

        match result.result {
            Ok(()) => {
                crate::logging::worktree_remove_completed(
                    &result.workspace_id.to_string(),
                    &result.path.display().to_string(),
                );
                let removed_managed = self
                    .state
                    .worktree_remove
                    .as_ref()
                    .is_none_or(|remove| remove.managed);
                let branch_to_delete = self
                    .state
                    .worktree_remove
                    .as_ref()
                    .filter(|remove| {
                        remove.delete_branch
                            && matches!(
                                remove.merge_gate,
                                Some(crate::worktree::WorktreeMergeGate::Merged { .. })
                            )
                    })
                    .and_then(|remove| {
                        remove
                            .branch
                            .clone()
                            .map(|branch| (remove.repo_root.clone(), branch))
                    });
                self.state.worktree_remove = None;
                if let Some((repo_root, branch)) = branch_to_delete {
                    let event_tx = self.event_tx.clone();
                    std::thread::spawn(move || {
                        let result = crate::worktree::delete_local_branch(&repo_root, &branch);
                        let _ = event_tx.blocking_send(AppEvent::WorktreeBranchDeleteFinished(
                            crate::events::WorktreeBranchDeleteResult { branch, result },
                        ));
                    });
                }
                if let Some(ws_idx) = self
                    .state
                    .workspaces
                    .iter()
                    .position(|ws| ws.id == result.workspace_id)
                {
                    let ws = &self.state.workspaces[ws_idx];
                    let still_same_linked_worktree = ws.worktree_space().is_some_and(|space| {
                        space.is_linked_worktree && space.checkout_path == result.path
                    }) || (!removed_managed
                        && ws
                            .git_space()
                            .is_some_and(|space| space.repo_root == result.path));
                    if still_same_linked_worktree {
                        self.state.selected = ws_idx;
                        self.state.close_selected_workspace();
                    }
                }
                self.state.mode = if self.state.active.is_some() {
                    Mode::Terminal
                } else {
                    Mode::Navigate
                };
                self.render_dirty.store(true, Ordering::Release);
                self.render_notify.notify_one();
            }
            Err(message) => {
                crate::logging::worktree_remove_failed(
                    &result.workspace_id.to_string(),
                    &result.path.display().to_string(),
                    &message,
                );
                remove.removing = false;
                if !remove.force_confirmation
                    && crate::worktree::is_dirty_worktree_remove_error(&message)
                {
                    remove.force_confirmation = true;
                    remove.error = None;
                } else {
                    remove.error = Some(message);
                }
                self.render_dirty.store(true, Ordering::Release);
                self.render_notify.notify_one();
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Tests exec real git to prime fixtures — TracedCommand polices product code (logging redesign PR-3).
mod tests {
    use super::*;

    fn unique_temp_path(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("flock-{name}-{}-{nanos}", std::process::id()))
    }

    fn run_git(repo: &std::path::Path, args: &[&str]) {
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

    fn create_committed_repo(name: &str) -> std::path::PathBuf {
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

    fn wait_for_worktree_event(app: &mut App) -> AppEvent {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if let Ok(event) = app.event_rx.try_recv() {
                return event;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("timed out waiting for worktree event");
    }

    fn app_for_worktree_tests() -> App {
        App::new(
            &crate::config::Config::default(),
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        )
    }

    #[test]
    fn open_selected_existing_worktree_focuses_already_open_workspace() {
        let mut app = app_for_worktree_tests();
        app.state.workspaces = vec![
            crate::workspace::Workspace::test_new("main"),
            crate::workspace::Workspace::test_new("issue"),
        ];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.worktree_open = Some(WorktreeOpenState {
            source_workspace_id: app.state.workspaces[0].id.clone(),
            source_existing_membership: None,
            source_checkout_path: "/repo/flock".into(),
            source_repo_root: "/repo/flock".into(),
            repo_key: "repo-key".into(),
            repo_name: "flock".into(),
            entries: vec![WorktreeOpenEntry {
                path: "/repo/flock-issue".into(),
                branch: Some("worktree/issue".into()),
                is_linked_worktree: true,
                already_open_ws_idx: Some(1),
            }],
            selected: 0,
            query: String::new(),
            search_focused: false,
            error: None,
        });

        app.open_selected_existing_worktree();

        assert_eq!(app.state.active, Some(1));
        assert_eq!(app.state.selected, 1);
        assert!(app.state.worktree_open.is_none());
        assert!(app.state.workspaces[0].worktree_space().is_some());
        let target_membership = app.state.workspaces[1].worktree_space().unwrap();
        assert_eq!(target_membership.key, "repo-key");
        assert_eq!(
            target_membership.checkout_path,
            std::path::PathBuf::from("/repo/flock-issue")
        );
        assert!(target_membership.is_linked_worktree);
    }

    #[test]
    fn worktree_open_search_filters_entries() {
        let mut app = app_for_worktree_tests();
        app.state.worktree_open = Some(WorktreeOpenState {
            source_workspace_id: "source".into(),
            source_existing_membership: None,
            source_checkout_path: "/repo/flock".into(),
            source_repo_root: "/repo/flock".into(),
            repo_key: "repo-key".into(),
            repo_name: "flock".into(),
            entries: vec![
                WorktreeOpenEntry {
                    path: "/repo/flock".into(),
                    branch: Some("main".into()),
                    is_linked_worktree: false,
                    already_open_ws_idx: Some(0),
                },
                WorktreeOpenEntry {
                    path: "/repo/fd-cleanup".into(),
                    branch: Some("fd-cleanup".into()),
                    is_linked_worktree: true,
                    already_open_ws_idx: None,
                },
                WorktreeOpenEntry {
                    path: "/repo/bell-forward-macos-bounce".into(),
                    branch: Some("bell-forward-macos-bounce".into()),
                    is_linked_worktree: true,
                    already_open_ws_idx: None,
                },
            ],
            selected: 0,
            query: String::new(),
            search_focused: false,
            error: None,
        });

        app.handle_worktree_open_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('/'),
            crossterm::event::KeyModifiers::empty(),
        ));
        app.handle_worktree_open_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('f'),
            crossterm::event::KeyModifiers::empty(),
        ));
        app.handle_worktree_open_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('d'),
            crossterm::event::KeyModifiers::empty(),
        ));
        app.handle_worktree_open_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('-'),
            crossterm::event::KeyModifiers::empty(),
        ));
        app.handle_worktree_open_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('c'),
            crossterm::event::KeyModifiers::empty(),
        ));
        app.handle_worktree_open_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('l'),
            crossterm::event::KeyModifiers::empty(),
        ));

        let open = app.state.worktree_open.as_ref().unwrap();
        assert_eq!(open.query, "fd-cl");
        assert_eq!(open.filtered_indices(), vec![1]);
        assert_eq!(open.selected_entry_index(), Some(1));
    }

    #[test]
    fn open_existing_worktree_detects_already_open_checkout_from_subdirectory() {
        let repo = create_committed_repo("app-worktree-open-existing-repo");
        let checkout = unique_temp_path("app-worktree-open-existing-checkout");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "worktree/open-existing",
                checkout.to_str().unwrap(),
                "HEAD",
            ],
        );
        let subdir = checkout.join("nested");
        std::fs::create_dir_all(&subdir).unwrap();

        let mut app = app_for_worktree_tests();
        app.state.workspaces = vec![
            crate::workspace::Workspace::test_new("main"),
            crate::workspace::Workspace::test_new("nested"),
        ];
        app.state.workspaces[0].identity_cwd = repo;
        app.state.workspaces[1].identity_cwd = subdir;

        app.open_existing_worktree_dialog(0);

        let open = app.state.worktree_open.as_ref().unwrap();
        let checkout = crate::worktree::canonical_or_original(&checkout);
        let entry = open
            .entries
            .iter()
            .find(|entry| crate::worktree::canonical_or_original(&entry.path) == checkout)
            .unwrap_or_else(|| panic!("missing checkout in entries: {:?}", open.entries));
        assert_eq!(entry.already_open_ws_idx, Some(1));
    }

    #[test]
    fn worktree_create_from_linked_worktree_is_allowed() {
        // #124: branch-from-here on a flock-managed linked worktree opens the
        // new-worktree dialog with that worktree's checkout as the source (it
        // forks from the linked worktree's own HEAD).
        let mut app = app_for_worktree_tests();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("issue")];
        app.state.mode = Mode::Navigate;
        app.state.workspaces[0].worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "flock".into(),
            repo_root: "/repo/flock".into(),
            checkout_path: "/repo/flock-issue".into(),
            is_linked_worktree: true,
        });

        app.open_new_linked_worktree_dialog(0, None);

        assert_eq!(app.state.mode, Mode::NewLinkedWorktree);
        let create = app.state.worktree_create.as_ref().expect("dialog opened");
        assert_eq!(
            create.source_checkout_path,
            std::path::PathBuf::from("/repo/flock-issue")
        );
        assert_eq!(create.base, "HEAD");
        assert!(app.state.action_notice.is_none());
    }

    #[test]
    fn new_worktree_dialog_honors_explicit_base_ref() {
        // #123: the project header passes the project's default branch as the
        // base ref, instead of the source checkout's HEAD.
        let mut app = app_for_worktree_tests();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("main")];
        app.state.mode = Mode::Navigate;
        app.state.workspaces[0].worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "flock".into(),
            repo_root: "/repo/flock".into(),
            checkout_path: "/repo/flock".into(),
            is_linked_worktree: false,
        });

        app.open_new_linked_worktree_dialog(0, Some("main".into()));

        let create = app.state.worktree_create.as_ref().expect("dialog opened");
        assert_eq!(create.base, "main");
    }

    #[test]
    fn sync_worktree_branch_updates_derived_path() {
        let mut app = app_for_worktree_tests();
        app.state.worktree_directory = std::path::PathBuf::from("/w");
        app.state.worktree_create = Some(WorktreeCreateState {
            branch_plan: None,
            source_workspace_id: "source".into(),
            source_checkout_path: std::path::PathBuf::from("/repo/flock"),
            source_existing_membership: None,
            source_repo_root: std::path::PathBuf::from("/repo/flock"),
            repo_key: "repo-key".into(),
            repo_name: "flock".into(),
            branch: "old".into(),
            branch_input: crate::app::line_editor::LineEditor::new("issue/137"),
            base: "HEAD".into(),
            checkout_path: std::path::PathBuf::from("/old"),
            seed: crate::app::line_editor::LineEditor::default(),
            focus: crate::app::state::WorktreeCreateFocus::Branch,
            error: Some("old error".into()),
            creating: false,
        });

        app.sync_worktree_branch_from_input();

        let create = app.state.worktree_create.unwrap();
        assert_eq!(create.branch, "issue/137");
        assert_eq!(
            create.checkout_path,
            std::path::PathBuf::from("/w/flock/issue-137")
        );
        assert_eq!(create.error, None);
    }

    #[test]
    fn start_worktree_add_runs_git_on_worker_and_emits_result() {
        let repo = create_committed_repo("app-worktree-add-repo");
        let worktree_root = unique_temp_path("app-worktree-add-root");
        let branch = "worktree/app-worker";
        let checkout = crate::worktree::default_checkout_path(&worktree_root, "flock", branch);
        let mut app = app_for_worktree_tests();
        app.state.worktree_directory = worktree_root.clone();
        app.state.worktree_create = Some(WorktreeCreateState {
            branch_plan: None,
            source_workspace_id: "source".into(),
            source_checkout_path: repo.clone(),
            source_existing_membership: None,
            source_repo_root: repo.clone(),
            repo_key: "repo-key".into(),
            repo_name: "flock".into(),
            branch: branch.into(),
            base: "HEAD".into(),
            checkout_path: checkout.clone(),
            branch_input: crate::app::line_editor::LineEditor::new(branch),
            seed: crate::app::line_editor::LineEditor::default(),
            focus: crate::app::state::WorktreeCreateFocus::Branch,
            error: None,
            creating: false,
        });

        app.start_worktree_add();

        assert!(app
            .state
            .worktree_create
            .as_ref()
            .is_some_and(|create| create.creating));
        let event = wait_for_worktree_event(&mut app);
        match event {
            AppEvent::WorktreeAddFinished(result) => {
                assert_eq!(result.path, checkout);
                assert_eq!(result.result, Ok(()));
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(checkout.join("README.md").exists());

        let remove = crate::worktree::build_worktree_remove_command(&repo, &checkout, false);
        crate::worktree::run_worktree_command(&remove).unwrap();
        let _ = std::fs::remove_dir_all(worktree_root);
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn start_worktree_add_uses_source_checkout_head_as_base() {
        let repo = create_committed_repo("app-worktree-add-source-repo");
        let source_checkout = unique_temp_path("app-worktree-add-source-checkout");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "worktree/source-base",
                source_checkout.to_str().unwrap(),
                "HEAD",
            ],
        );
        std::fs::write(source_checkout.join("SOURCE.md"), "source branch\n").unwrap();
        run_git(&source_checkout, &["add", "SOURCE.md"]);
        run_git(&source_checkout, &["commit", "--quiet", "-m", "source"]);

        let worktree_root = unique_temp_path("app-worktree-add-from-source-root");
        let branch = "worktree/from-source";
        let checkout = crate::worktree::default_checkout_path(&worktree_root, "flock", branch);
        let mut app = app_for_worktree_tests();
        app.state.worktree_directory = worktree_root.clone();
        app.state.worktree_create = Some(WorktreeCreateState {
            branch_plan: None,
            source_workspace_id: "source".into(),
            source_checkout_path: source_checkout.clone(),
            source_existing_membership: None,
            source_repo_root: repo.clone(),
            repo_key: "repo-key".into(),
            repo_name: "flock".into(),
            branch: branch.into(),
            base: "HEAD".into(),
            checkout_path: checkout.clone(),
            branch_input: crate::app::line_editor::LineEditor::new(branch),
            seed: crate::app::line_editor::LineEditor::default(),
            focus: crate::app::state::WorktreeCreateFocus::Branch,
            error: None,
            creating: false,
        });

        app.start_worktree_add();

        let event = wait_for_worktree_event(&mut app);
        match event {
            AppEvent::WorktreeAddFinished(result) => {
                assert_eq!(result.path, checkout);
                assert_eq!(result.result, Ok(()));
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(checkout.join("SOURCE.md").exists());

        let remove_new = crate::worktree::build_worktree_remove_command(&repo, &checkout, false);
        crate::worktree::run_worktree_command(&remove_new).unwrap();
        let remove_source =
            crate::worktree::build_worktree_remove_command(&repo, &source_checkout, false);
        crate::worktree::run_worktree_command(&remove_source).unwrap();
        let _ = std::fs::remove_dir_all(worktree_root);
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn dirty_worktree_remove_failure_requests_force_confirmation() {
        let path = std::path::PathBuf::from("/w/flock/dirty");
        let mut app = app_for_worktree_tests();
        app.state.worktree_remove = Some(WorktreeRemoveState {
            managed: true,
            workspace_id: "ws".into(),
            repo_root: std::path::PathBuf::from("/repo/flock"),
            path: path.clone(),
            error: None,
            removing: true,
            force_confirmation: false,
            delete_branch: false,
            branch: None,
            merge_gate: None,
            branch_protected: false,
            gate_timed_out: false,
        });

        app.handle_worktree_remove_finished(WorktreeRemoveResult {
            workspace_id: "ws".into(),
            path,
            result: Err(
                "fatal: '/w/flock/dirty' contains modified or untracked files, use --force to delete it"
                    .into(),
            ),
        });

        let remove = app.state.worktree_remove.unwrap();
        assert!(!remove.removing);
        assert!(remove.force_confirmation);
        assert_eq!(remove.error, None);
    }

    #[test]
    fn non_dirty_worktree_remove_failure_keeps_error_message() {
        let path = std::path::PathBuf::from("/w/flock/missing");
        let mut app = app_for_worktree_tests();
        app.state.worktree_remove = Some(WorktreeRemoveState {
            managed: true,
            workspace_id: "ws".into(),
            repo_root: std::path::PathBuf::from("/repo/flock"),
            path: path.clone(),
            error: None,
            removing: true,
            force_confirmation: false,
            delete_branch: false,
            branch: None,
            merge_gate: None,
            branch_protected: false,
            gate_timed_out: false,
        });

        app.handle_worktree_remove_finished(WorktreeRemoveResult {
            workspace_id: "ws".into(),
            path,
            result: Err("fatal: '/w/flock/missing' is not a working tree".into()),
        });

        let remove = app.state.worktree_remove.unwrap();
        assert!(!remove.removing);
        assert!(!remove.force_confirmation);
        assert_eq!(
            remove.error,
            Some("fatal: '/w/flock/missing' is not a working tree".into())
        );
    }

    #[test]
    fn dirty_worktree_remove_retries_with_force_and_closes_workspace() {
        let repo = create_committed_repo("app-worktree-dirty-remove-repo");
        let checkout = unique_temp_path("app-worktree-dirty-remove-checkout");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "worktree/dirty-remove",
                checkout.to_str().unwrap(),
                "HEAD",
            ],
        );
        std::fs::write(checkout.join("README.md"), "dirty\n").unwrap();

        let mut app = app_for_worktree_tests();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("issue")];
        let workspace_id = app.state.workspaces[0].id.clone();
        app.state.workspaces[0].worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "flock".into(),
            repo_root: repo.clone(),
            checkout_path: checkout.clone(),
            is_linked_worktree: true,
        });
        app.state.active = Some(0);
        app.state.selected = 0;
        app.open_remove_linked_worktree_confirmation(0);

        app.start_worktree_remove();
        let safe_event = wait_for_worktree_event(&mut app);
        match safe_event {
            AppEvent::WorktreeRemoveFinished(result) => {
                assert_eq!(result.workspace_id, workspace_id);
                assert_eq!(result.path, checkout);
                assert!(result.result.is_err());
                app.handle_worktree_remove_finished(result);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let remove = app.state.worktree_remove.as_ref().unwrap();
        assert!(!remove.removing);
        assert!(remove.force_confirmation);
        assert!(checkout.exists());

        app.start_worktree_remove();
        let force_event = wait_for_worktree_event(&mut app);
        match force_event {
            AppEvent::WorktreeRemoveFinished(result) => {
                assert_eq!(result.workspace_id, workspace_id);
                assert_eq!(result.path, checkout);
                assert_eq!(result.result, Ok(()));
                app.handle_worktree_remove_finished(result);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        assert!(!checkout.exists());
        assert!(app.state.worktree_remove.is_none());
        assert!(app.state.workspaces.is_empty());

        let _ = std::fs::remove_dir_all(repo);
    }
    #[test]
    fn branch_session_dialog_requires_agent_session() {
        let mut app = app_for_worktree_tests();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("main")];
        app.state.mode = Mode::Navigate;
        app.state.workspaces[0].cached_git_space = Some(crate::workspace::GitSpaceMetadata {
            key: "repo-key".into(),
            checkout_key: "checkout-key".into(),
            label: "flock".into(),
            repo_root: "/repo/flock".into(),
            is_linked_worktree: false,
            project_key: "dir:flock".into(),
        });

        app.open_branch_session_dialog(0);

        assert!(app.state.worktree_create.is_none());
        assert_eq!(app.state.mode, Mode::Navigate);
        assert_eq!(
            app.state.action_notice.as_deref(),
            Some("branch session: focused pane has no resumable agent session")
        );
    }

    #[test]
    fn branch_session_dialog_attaches_fork_plan_from_persisted_session() {
        let mut app = app_for_worktree_tests();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("main")];
        app.state.mode = Mode::Navigate;
        app.state.workspaces[0].cached_git_space = Some(crate::workspace::GitSpaceMetadata {
            key: "repo-key".into(),
            checkout_key: "checkout-key".into(),
            label: "flock".into(),
            repo_root: "/repo/flock".into(),
            is_linked_worktree: false,
            project_key: "dir:flock".into(),
        });

        let ws = &app.state.workspaces[0];
        let pane_id = ws.focused_pane_id().expect("workspace should have a pane");
        let terminal_id = ws
            .pane_state(pane_id)
            .expect("pane state should exist")
            .attached_terminal_id
            .clone();
        let mut terminal =
            crate::terminal::TerminalState::new(terminal_id.clone(), "/repo/flock".into());
        terminal.persisted_agent_session = Some(crate::agent_resume::PersistedAgentSession {
            source: "flock:claude".into(),
            agent: "claude".into(),
            session_ref: crate::agent_resume::AgentSessionRef::id("sess-1")
                .expect("session id should validate"),
        });
        app.state.terminals.insert(terminal_id, terminal);

        app.open_branch_session_dialog(0);

        assert_eq!(app.state.mode, Mode::NewLinkedWorktree);
        let plan = app
            .state
            .worktree_create
            .as_ref()
            .and_then(|create| create.branch_plan.as_ref())
            .expect("branch plan should be attached");
        assert_eq!(
            plan.argv,
            vec!["claude", "--resume", "sess-1", "--fork-session"]
        );
    }

    /// #159: the branch-session dialog pre-fills the editable seed with the
    /// pivot template (verbatim `<branch>` token, resolved at confirm) and
    /// starts focus on the branch field.
    fn app_with_persisted_claude_session() -> App {
        let mut app = app_for_worktree_tests();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("main")];
        app.state.mode = Mode::Navigate;
        app.state.workspaces[0].cached_git_space = Some(crate::workspace::GitSpaceMetadata {
            key: "repo-key".into(),
            checkout_key: "checkout-key".into(),
            label: "flock".into(),
            repo_root: "/repo/flock".into(),
            is_linked_worktree: false,
            project_key: "dir:flock".into(),
        });
        let ws = &app.state.workspaces[0];
        let pane_id = ws.focused_pane_id().expect("pane");
        let terminal_id = ws
            .pane_state(pane_id)
            .expect("pane state")
            .attached_terminal_id
            .clone();
        let mut terminal =
            crate::terminal::TerminalState::new(terminal_id.clone(), "/repo/flock".into());
        terminal.persisted_agent_session = Some(crate::agent_resume::PersistedAgentSession {
            source: "flock:claude".into(),
            agent: "claude".into(),
            session_ref: crate::agent_resume::AgentSessionRef::id("sess-1").expect("session id"),
        });
        app.state.terminals.insert(terminal_id, terminal);
        app
    }

    #[test]
    fn branch_session_dialog_prefills_editable_seed_from_pivot_template() {
        let mut app = app_with_persisted_claude_session();
        app.state.branch_pivot_message = "pivot for <branch>, stay put".into();

        app.open_branch_session_dialog(0);

        let create = app.state.worktree_create.as_ref().expect("dialog open");
        assert_eq!(create.seed.value(), "pivot for <branch>, stay put");
        assert_eq!(create.focus, crate::app::state::WorktreeCreateFocus::Branch);
    }

    #[test]
    fn plain_new_worktree_dialog_has_no_seed() {
        let mut app = app_with_persisted_claude_session();
        app.state.branch_pivot_message = "should not leak into a plain worktree".into();

        // The plain opener (no branch_plan) must leave the seed empty.
        app.open_new_linked_worktree_dialog(0, None);

        let create = app.state.worktree_create.as_ref().expect("dialog open");
        assert!(create.branch_plan.is_none());
        assert!(create.seed.is_empty());
    }

    #[test]
    fn resolve_seed_prompt_substitutes_branch_and_keeps_empty_empty() {
        // Empty stays empty → no seed injected (the opt-out).
        assert_eq!(resolve_seed_prompt("", "issue/9"), "");
        // `<branch>` resolves to the final (trimmed) branch name.
        assert_eq!(
            resolve_seed_prompt("work on <branch> now", "  feat/x  "),
            "work on feat/x now"
        );
        // A custom prompt with no token passes through unchanged.
        assert_eq!(
            resolve_seed_prompt("just do the thing", "feat/x"),
            "just do the thing"
        );
    }

    /// Acceptance: an EDITED seed reaches the fork's argv, and an empty field
    /// opts out. The confirm path is exactly `resolve_seed_prompt` ->
    /// `append_pivot_message`; exercise that composition (a real PTY spawn is
    /// out of scope for a unit test) with a user-edited value.
    #[test]
    fn edited_seed_is_injected_into_the_fork_argv_and_empty_opts_out() {
        let session = crate::agent_resume::AgentSessionRef::id("s").expect("session id");
        let mut plan = crate::agent_resume::branch_plan("flock:claude", "claude", &session)
            .expect("claude fork plan");

        // User edited the seed (kept the <branch> token); confirm resolves +
        // injects it as the fork's opening positional prompt.
        let pivot = resolve_seed_prompt("custom plan: land <branch>", "feat/login");
        crate::agent_resume::append_pivot_message(&mut plan, &pivot);
        assert_eq!(plan.argv.last().unwrap(), "custom plan: land feat/login");

        // Cleared field => nothing appended (the opt-out).
        let mut plan2 = crate::agent_resume::branch_plan("flock:claude", "claude", &session)
            .expect("claude fork plan");
        let before = plan2.argv.clone();
        crate::agent_resume::append_pivot_message(
            &mut plan2,
            &resolve_seed_prompt("", "feat/login"),
        );
        assert_eq!(plan2.argv, before, "empty seed must inject nothing");
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn tab_moves_focus_to_the_seed_field_and_typing_edits_it() {
        use crate::app::state::WorktreeCreateFocus;
        let mut app = app_with_persisted_claude_session();
        app.state.branch_pivot_message = "seed <branch>".into();
        app.open_branch_session_dialog(0);

        // Branch field starts focused: typing appends to the branch editor
        // (cursor starts at the end of the pre-filled slug), not the seed.
        let branch_before = app
            .state
            .worktree_create
            .as_ref()
            .unwrap()
            .branch_input
            .value()
            .to_string();
        app.handle_worktree_create_key(key(KeyCode::Char('x')));
        let create = app.state.worktree_create.as_ref().unwrap();
        assert_eq!(create.branch_input.value(), format!("{branch_before}x"));
        assert_eq!(create.seed.value(), "seed <branch>", "seed untouched");

        // Tab → seed field. Typing appends to the seed, leaving branch alone.
        app.handle_worktree_create_key(key(KeyCode::Tab));
        assert_eq!(
            app.state.worktree_create.as_ref().unwrap().focus,
            WorktreeCreateFocus::Seed
        );
        app.handle_worktree_create_key(key(KeyCode::Char('!')));
        let create = app.state.worktree_create.as_ref().unwrap();
        assert_eq!(create.seed.value(), "seed <branch>!");
        assert_eq!(
            create.branch_input.value(),
            format!("{branch_before}x"),
            "branch untouched"
        );

        // Backspace on the seed deletes its last char.
        app.handle_worktree_create_key(key(KeyCode::Backspace));
        assert_eq!(
            app.state.worktree_create.as_ref().unwrap().seed.value(),
            "seed <branch>"
        );

        // Tab back → branch field again.
        app.handle_worktree_create_key(key(KeyCode::Tab));
        assert_eq!(
            app.state.worktree_create.as_ref().unwrap().focus,
            WorktreeCreateFocus::Branch
        );
    }

    #[test]
    fn tab_is_a_noop_without_a_seed_field() {
        use crate::app::state::WorktreeCreateFocus;
        let mut app = app_with_persisted_claude_session();
        app.open_new_linked_worktree_dialog(0, None); // plain worktree, no seed

        app.handle_worktree_create_key(key(KeyCode::Tab));
        assert_eq!(
            app.state.worktree_create.as_ref().unwrap().focus,
            WorktreeCreateFocus::Branch,
            "a plain worktree has no seed row, so Tab must not move focus"
        );
        // And typing still edits the branch name.
        app.handle_worktree_create_key(key(KeyCode::Char('z')));
        assert!(app
            .state
            .worktree_create
            .as_ref()
            .unwrap()
            .branch_input
            .value()
            .ends_with('z'));
    }

    #[test]
    fn dialog_supports_cursor_movement_and_word_delete() {
        let mut app = app_with_persisted_claude_session();
        app.state.branch_pivot_message = String::new(); // start with an empty seed
        app.open_branch_session_dialog(0);
        app.handle_worktree_create_key(key(KeyCode::Tab)); // focus the seed

        for c in "land the feature".chars() {
            app.handle_worktree_create_key(key(KeyCode::Char(c)));
        }
        let seed = |app: &App| {
            app.state
                .worktree_create
                .as_ref()
                .unwrap()
                .seed
                .value()
                .to_string()
        };
        assert_eq!(seed(&app), "land the feature");

        // ^W deletes the last word (with its trailing space eaten first).
        app.handle_worktree_create_key(ctrl(KeyCode::Char('w')));
        assert_eq!(seed(&app), "land the ");

        // Home moves to the start; insert prepends there (cursor now at 4).
        app.handle_worktree_create_key(key(KeyCode::Home));
        for c in "GO: ".chars() {
            app.handle_worktree_create_key(key(KeyCode::Char(c)));
        }
        assert_eq!(seed(&app), "GO: land the ");

        // Delete removes the char at the cursor ('l').
        app.handle_worktree_create_key(key(KeyCode::Delete));
        assert_eq!(seed(&app), "GO: and the ");

        // ^U deletes from the cursor (still at 4) back to the start.
        app.handle_worktree_create_key(ctrl(KeyCode::Char('u')));
        assert_eq!(seed(&app), "and the ");
    }

    #[test]
    fn dialog_editing_is_unicode_safe() {
        let mut app = app_with_persisted_claude_session();
        app.state.branch_pivot_message = String::new();
        app.open_branch_session_dialog(0);
        app.handle_worktree_create_key(key(KeyCode::Tab)); // seed
        for c in "naïve".chars() {
            app.handle_worktree_create_key(key(KeyCode::Char(c)));
        }
        // Left past 'e' and 'v', then insert before the multi-byte 'ï'... move to
        // just after 'a' (3 lefts from end: e, v, then between ï and v).
        app.handle_worktree_create_key(key(KeyCode::Left)); // before 'e'
        app.handle_worktree_create_key(key(KeyCode::Backspace)); // remove 'v'
        assert_eq!(
            app.state.worktree_create.as_ref().unwrap().seed.value(),
            "naïe"
        );
    }

    #[test]
    fn pure_cursor_movement_does_not_resync_or_clear_error() {
        let mut app = app_with_persisted_claude_session();
        app.open_new_linked_worktree_dialog(0, None);
        // Stamp an error + a known checkout path, then move the cursor only.
        if let Some(create) = app.state.worktree_create.as_mut() {
            create.error = Some("kept".into());
            create.checkout_path = std::path::PathBuf::from("/unchanged");
        }
        for code in [KeyCode::Left, KeyCode::Home, KeyCode::Right, KeyCode::End] {
            app.handle_worktree_create_key(key(code));
        }
        let create = app.state.worktree_create.as_ref().unwrap();
        assert_eq!(
            create.error.as_deref(),
            Some("kept"),
            "movement must not clear the error"
        );
        assert_eq!(
            create.checkout_path,
            std::path::PathBuf::from("/unchanged"),
            "movement must not recompute the checkout path"
        );
    }

    #[test]
    fn editing_the_branch_resyncs_the_checkout_path() {
        let mut app = app_with_persisted_claude_session();
        app.state.worktree_directory = std::path::PathBuf::from("/w");
        app.open_new_linked_worktree_dialog(0, None);

        // Replace the whole branch name via ^U then type a fresh one.
        app.handle_worktree_create_key(ctrl(KeyCode::Char('u')));
        for c in "feat/login".chars() {
            app.handle_worktree_create_key(key(KeyCode::Char(c)));
        }
        let create = app.state.worktree_create.as_ref().unwrap();
        assert_eq!(create.branch, "feat/login");
        assert_eq!(
            create.checkout_path,
            std::path::PathBuf::from("/w/flock/feat-login")
        );
    }

    #[test]
    fn branch_session_dialog_points_at_integration_when_agent_has_no_session_ref() {
        // A detected agent with NO session ref (the classic "flock hook not
        // installed on this host" case, e.g. a read-only nix-managed
        // settings.json) must NOT get the flat "no resumable agent session"
        // notice — it must name the agent and point at `flk integration status`.
        let mut app = app_for_worktree_tests();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("main")];
        app.state.mode = Mode::Navigate;
        app.state.workspaces[0].cached_git_space = Some(crate::workspace::GitSpaceMetadata {
            key: "repo-key".into(),
            checkout_key: "checkout-key".into(),
            label: "flock".into(),
            repo_root: "/repo/flock".into(),
            is_linked_worktree: false,
            project_key: "dir:flock".into(),
        });

        let ws = &app.state.workspaces[0];
        let pane_id = ws.focused_pane_id().expect("workspace should have a pane");
        let terminal_id = ws
            .pane_state(pane_id)
            .expect("pane state should exist")
            .attached_terminal_id
            .clone();
        let mut terminal =
            crate::terminal::TerminalState::new(terminal_id.clone(), "/repo/flock".into());
        // Agent is detected but never reported a session ref.
        terminal.detected_agent = Some(crate::detect::Agent::Claude);
        app.state.terminals.insert(terminal_id, terminal);

        app.open_branch_session_dialog(0);

        assert!(app.state.worktree_create.is_none());
        assert_eq!(
            app.state.action_notice.as_deref(),
            Some("branch session: claude reports no resumable session here — check `flk integration status`")
        );
    }

    #[test]
    fn kill_worktree_confirmation_rejects_non_worktree_workspace() {
        let mut app = app_for_worktree_tests();
        let mut ws = crate::workspace::Workspace::test_new("main");
        // Pin identity away from the test process cwd (which may itself be a
        // linked worktree) and pretend it's a plain main checkout.
        ws.identity_cwd = std::path::PathBuf::from("/plain/dir");
        ws.cached_git_space = None;
        app.state.workspaces = vec![ws];
        app.state.mode = Mode::Navigate;

        app.open_kill_worktree_confirmation(0);

        assert!(app.state.worktree_remove.is_none());
        assert_eq!(
            app.state.action_notice.as_deref(),
            Some("kill worktree: this workspace is not a linked git worktree checkout")
        );
    }

    #[test]
    fn kill_worktree_adopts_unmanaged_linked_checkout() {
        let repo = create_committed_repo("kill-unmanaged-repo");
        let checkout = unique_temp_path("kill-unmanaged-checkout");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "feature/external",
                checkout.to_str().unwrap(),
            ],
        );

        let mut app = app_for_worktree_tests();
        let mut ws = crate::workspace::Workspace::test_new("external");
        ws.identity_cwd = checkout.clone();
        ws.cached_git_space = crate::workspace::git_space_metadata(&checkout);
        assert!(
            ws.cached_git_space
                .as_ref()
                .is_some_and(|space| space.is_linked_worktree),
            "external checkout should be detected as a linked worktree"
        );
        app.state.workspaces = vec![ws];
        app.state.mode = Mode::Navigate;

        app.open_kill_worktree_confirmation(0);

        let remove = app
            .state
            .worktree_remove
            .as_ref()
            .expect("kill should adopt the unmanaged checkout");
        assert!(!remove.managed);
        assert!(remove.delete_branch);
        assert_eq!(
            std::fs::canonicalize(&remove.path).unwrap(),
            std::fs::canonicalize(&checkout).unwrap()
        );
        assert_eq!(
            std::fs::canonicalize(&remove.repo_root).unwrap(),
            std::fs::canonicalize(&repo).unwrap(),
            "git commands must run from the main checkout"
        );
        assert_eq!(app.state.mode, Mode::ConfirmRemoveWorktree);

        let _ = std::fs::remove_dir_all(&checkout);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn kill_gate_event_updates_pending_dialog() {
        let mut app = app_for_worktree_tests();
        app.state.worktree_remove = Some(WorktreeRemoveState {
            managed: true,
            workspace_id: "ws".into(),
            repo_root: std::path::PathBuf::from("/repo/flock"),
            path: std::path::PathBuf::from("/repo/flock-issue"),
            error: None,
            removing: false,
            force_confirmation: false,
            delete_branch: true,
            branch: None,
            merge_gate: None,
            branch_protected: false,
            gate_timed_out: false,
        });

        app.handle_worktree_kill_gate_finished(crate::events::WorktreeKillGateResult {
            workspace_id: "ws".into(),
            path: std::path::PathBuf::from("/repo/flock-issue"),
            branch: Some("feature/x".into()),
            gate: crate::worktree::WorktreeMergeGate::Merged {
                evidence: "PR #7 merged".into(),
            },
            timed_out: false,
        });

        let remove = app.state.worktree_remove.as_ref().unwrap();
        assert_eq!(remove.branch.as_deref(), Some("feature/x"));
        assert_eq!(
            remove.merge_gate,
            Some(crate::worktree::WorktreeMergeGate::Merged {
                evidence: "PR #7 merged".into()
            })
        );
        // A non-default branch with merge evidence keeps the deletion offer.
        assert!(remove.delete_branch);
        assert!(!remove.branch_protected);
        assert!(!remove.gate_timed_out);
    }

    #[test]
    fn kill_gate_protects_default_branch_even_when_merge_gate_passes() {
        let mut app = app_for_worktree_tests();
        app.state.worktree_remove = Some(WorktreeRemoveState {
            managed: true,
            workspace_id: "ws".into(),
            repo_root: std::path::PathBuf::from("/repo/flock"),
            path: std::path::PathBuf::from("/repo/flock-issue"),
            error: None,
            removing: false,
            force_confirmation: false,
            delete_branch: true,
            branch: None,
            merge_gate: None,
            branch_protected: false,
            gate_timed_out: false,
        });

        // The gate says "merged" (the default branch is trivially contained in
        // every downstream remote ref), but it is the repo's default branch —
        // the guard must pin it checkout-only and never build `git branch -D`.
        app.handle_worktree_kill_gate_finished(crate::events::WorktreeKillGateResult {
            workspace_id: "ws".into(),
            path: std::path::PathBuf::from("/repo/flock-issue"),
            branch: Some("main".into()),
            gate: crate::worktree::WorktreeMergeGate::Merged {
                evidence: "contained in origin/latest".into(),
            },
            timed_out: false,
        });

        let remove = app.state.worktree_remove.as_ref().unwrap();
        assert_eq!(remove.branch.as_deref(), Some("main"));
        assert!(
            !remove.delete_branch,
            "default branch must never be flagged for deletion"
        );
        assert!(
            remove.branch_protected,
            "dialog must show the branch is protected"
        );
    }

    #[test]
    fn kill_gate_timeout_marks_dialog_and_keeps_branch() {
        let mut app = app_for_worktree_tests();
        app.state.worktree_remove = Some(WorktreeRemoveState {
            managed: true,
            workspace_id: "ws".into(),
            repo_root: std::path::PathBuf::from("/repo/flock"),
            path: std::path::PathBuf::from("/repo/flock-issue"),
            error: None,
            removing: false,
            force_confirmation: false,
            delete_branch: true,
            branch: None,
            merge_gate: None,
            branch_protected: false,
            gate_timed_out: false,
        });

        // A wedged `gh pr view` degrades to NotMerged + timed_out: the dialog
        // must record the timeout and never keep the branch flagged for delete.
        app.handle_worktree_kill_gate_finished(crate::events::WorktreeKillGateResult {
            workspace_id: "ws".into(),
            path: std::path::PathBuf::from("/repo/flock-issue"),
            branch: Some("feature/x".into()),
            gate: crate::worktree::WorktreeMergeGate::NotMerged,
            timed_out: true,
        });

        let remove = app.state.worktree_remove.as_ref().unwrap();
        assert!(remove.gate_timed_out, "dialog must record the gate timeout");
        assert_eq!(
            remove.merge_gate,
            Some(crate::worktree::WorktreeMergeGate::NotMerged)
        );
    }

    #[test]
    fn esc_dismisses_kill_dialog_while_merge_gate_is_pending() {
        let mut app = app_for_worktree_tests();
        // merge_gate: None == the gate is still running (the wedge window).
        app.state.worktree_remove = Some(WorktreeRemoveState {
            managed: true,
            workspace_id: "ws".into(),
            repo_root: std::path::PathBuf::from("/repo/flock"),
            path: std::path::PathBuf::from("/repo/flock-issue"),
            error: None,
            removing: false,
            force_confirmation: false,
            delete_branch: true,
            branch: None,
            merge_gate: None,
            branch_protected: false,
            gate_timed_out: false,
        });
        app.state.mode = Mode::ConfirmRemoveWorktree;

        app.handle_worktree_remove_key(KeyEvent::from(KeyCode::Esc));

        assert!(
            app.state.worktree_remove.is_none(),
            "Esc must dismiss the dialog even while the gate is still in flight"
        );
    }

    #[test]
    fn start_worktree_remove_waits_for_pending_merge_gate() {
        let mut app = app_for_worktree_tests();
        app.state.worktree_remove = Some(WorktreeRemoveState {
            managed: true,
            workspace_id: "ws".into(),
            repo_root: std::path::PathBuf::from("/repo/flock"),
            path: std::path::PathBuf::from("/repo/flock-issue"),
            error: None,
            removing: false,
            force_confirmation: false,
            delete_branch: true,
            branch: Some("feature/x".into()),
            merge_gate: None,
            branch_protected: false,
            gate_timed_out: false,
        });

        app.start_worktree_remove();

        // Gate unresolved: confirmation is a no-op rather than a blind delete.
        assert!(!app.state.worktree_remove.as_ref().unwrap().removing);

        app.state.worktree_remove.as_mut().unwrap().merge_gate =
            Some(crate::worktree::WorktreeMergeGate::NotMerged);
        app.start_worktree_remove();
        assert!(app.state.worktree_remove.as_ref().unwrap().removing);
    }

    #[test]
    fn remove_finished_deletes_branch_only_with_merged_gate() {
        // Real repo: merged branch is deleted after the checkout removal.
        let repo = create_committed_repo("kill-branch-delete-repo");
        let checkout = unique_temp_path("kill-branch-delete-checkout");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "feature/done",
                checkout.to_str().unwrap(),
            ],
        );
        run_git(&repo, &["merge", "--quiet", "feature/done"]);
        run_git(
            &repo,
            &["worktree", "remove", "--force", checkout.to_str().unwrap()],
        );

        let mut app = app_for_worktree_tests();
        app.state.worktree_remove = Some(WorktreeRemoveState {
            managed: true,
            workspace_id: "ws".into(),
            repo_root: repo.clone(),
            path: checkout.clone(),
            error: None,
            removing: true,
            force_confirmation: false,
            delete_branch: true,
            branch: Some("feature/done".into()),
            merge_gate: Some(crate::worktree::WorktreeMergeGate::Merged {
                evidence: "merged into master".into(),
            }),
            branch_protected: false,
            gate_timed_out: false,
        });

        app.handle_worktree_remove_finished(WorktreeRemoveResult {
            workspace_id: "ws".into(),
            path: checkout.clone(),
            result: Ok(()),
        });

        // Branch deletion runs on a worker thread; poll for it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let out = std::process::Command::new("git")
                .args([
                    "-C",
                    repo.to_str().unwrap(),
                    "branch",
                    "--list",
                    "feature/done",
                ])
                .output()
                .unwrap();
            if String::from_utf8_lossy(&out.stdout).trim().is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "branch was not deleted within the deadline"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn remove_finished_keeps_branch_without_merge_evidence() {
        let repo = create_committed_repo("kill-branch-keep-repo");
        run_git(&repo, &["branch", "feature/wip"]);

        let mut app = app_for_worktree_tests();
        app.state.worktree_remove = Some(WorktreeRemoveState {
            managed: true,
            workspace_id: "ws".into(),
            repo_root: repo.clone(),
            path: std::path::PathBuf::from("/tmp/x"),
            error: None,
            removing: true,
            force_confirmation: false,
            delete_branch: true,
            branch: Some("feature/wip".into()),
            merge_gate: Some(crate::worktree::WorktreeMergeGate::NotMerged),
            branch_protected: false,
            gate_timed_out: false,
        });

        app.handle_worktree_remove_finished(WorktreeRemoveResult {
            workspace_id: "ws".into(),
            path: std::path::PathBuf::from("/tmp/x"),
            result: Ok(()),
        });

        std::thread::sleep(std::time::Duration::from_millis(200));
        let out = std::process::Command::new("git")
            .args([
                "-C",
                repo.to_str().unwrap(),
                "branch",
                "--list",
                "feature/wip",
            ])
            .output()
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&out.stdout).trim().is_empty(),
            "unmerged branch must be kept"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }
    #[test]
    fn adopt_external_worktrees_links_child_and_parent() {
        let repo = create_committed_repo("adopt-external-repo");
        let checkout = unique_temp_path("adopt-external-checkout");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "feature/agent-made",
                checkout.to_str().unwrap(),
            ],
        );

        let mut app = app_for_worktree_tests();
        let mut parent = crate::workspace::Workspace::test_new("repo");
        parent.identity_cwd = repo.clone();
        parent.cached_git_space = crate::workspace::git_space_metadata(&repo);
        let mut child = crate::workspace::Workspace::test_new("external");
        child.identity_cwd = checkout.clone();
        child.cached_git_space = crate::workspace::git_space_metadata(&checkout);
        app.state.workspaces = vec![parent, child];

        assert!(app.state.adopt_external_worktrees());

        let child_space = app.state.workspaces[1]
            .worktree_space()
            .expect("child adopted");
        assert!(child_space.is_linked_worktree);
        assert_eq!(
            std::fs::canonicalize(&child_space.checkout_path).unwrap(),
            std::fs::canonicalize(&checkout).unwrap()
        );
        let parent_space = app.state.workspaces[0]
            .worktree_space()
            .expect("parent linked for grouping");
        assert!(!parent_space.is_linked_worktree);
        assert_eq!(parent_space.key, child_space.key);

        // Second pass is a no-op.
        assert!(!app.state.adopt_external_worktrees());

        let _ = std::fs::remove_dir_all(&checkout);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn adopt_external_worktrees_respects_config_flag() {
        let repo = create_committed_repo("adopt-flag-repo");
        let checkout = unique_temp_path("adopt-flag-checkout");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "feature/x",
                checkout.to_str().unwrap(),
            ],
        );

        let mut app = app_for_worktree_tests();
        let mut child = crate::workspace::Workspace::test_new("external");
        child.identity_cwd = checkout.clone();
        child.cached_git_space = crate::workspace::git_space_metadata(&checkout);
        app.state.workspaces = vec![child];
        app.state.adopt_external_worktrees = false;

        assert!(!app.state.adopt_external_worktrees());
        assert!(app.state.workspaces[0].worktree_space().is_none());

        let _ = std::fs::remove_dir_all(&checkout);
        let _ = std::fs::remove_dir_all(&repo);
    }
}
