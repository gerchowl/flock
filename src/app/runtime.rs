use std::time::{Duration, Instant};

use crossterm::terminal;

use super::{
    auto_updates_enabled, repeat_key_identity, App, Mode, ANIMATION_INTERVAL,
    AUTO_UPDATE_CHECK_INTERVAL, GIT_REMOTE_STATUS_REFRESH_INTERVAL, MIN_RENDER_INTERVAL,
    RESIZE_POLL_INTERVAL, SELECTION_AUTOSCROLL_INTERVAL,
};
use crate::events::AppEvent;
use crate::workspace::{GitStatusCacheEntry, Workspace, WorkspaceGitStatus};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkspaceGitRefreshItem {
    pub(crate) workspace_id: String,
    pub(crate) resolved_identity_cwd: std::path::PathBuf,
    pub(crate) cache_key: std::path::PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkspaceGitRefreshTarget {
    pub(crate) workspace_id: String,
    pub(crate) resolved_identity_cwd: std::path::PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkspaceGitRefreshJob {
    pub(crate) cache_key: std::path::PathBuf,
    pub(crate) status_cwd: std::path::PathBuf,
    pub(crate) cached: Option<GitStatusCacheEntry>,
    pub(crate) targets: Vec<WorkspaceGitRefreshTarget>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkspaceGitRefreshOutput {
    pub(crate) results: Vec<WorkspaceGitStatus>,
    pub(crate) cache_updates: Vec<(std::path::PathBuf, GitStatusCacheEntry)>,
}

impl App {
    pub(crate) fn shutdown_detached_terminal_runtimes(&mut self) {
        let terminal_ids = std::mem::take(&mut self.state.terminal_runtime_shutdowns);
        for terminal_id in terminal_ids {
            if let Some(runtime) = self.terminal_runtimes.remove(&terminal_id) {
                runtime.shutdown();
            }
        }
    }

    pub(crate) fn drain_api_requests(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.api_rx.try_recv() {
            changed |= self.handle_api_request_message(msg);
            self.shutdown_detached_terminal_runtimes();
        }
        changed
    }

    pub(super) fn handle_api_request_message(
        &mut self,
        msg: crate::api::ApiRequestMessage,
    ) -> bool {
        let previous_mode = self.state.mode;
        let changed = crate::api::request_changes_ui(&msg.request);
        self.current_api_peer_pid = msg.peer_pid;
        let response = self.handle_api_request(msg.request);
        self.current_api_peer_pid = None;
        let _ = msg.respond_to.send(response);
        self.sync_prefix_input_source(previous_mode);
        changed
    }

    pub(super) async fn handle_raw_input_batch(
        &mut self,
        first: crate::raw_input::RawInputEvent,
    ) -> bool {
        let mut changed = self.handle_raw_input_event(first).await;

        while let Some(rx) = self.input_rx.as_mut() {
            match rx.try_recv() {
                Ok(event) => changed |= self.handle_raw_input_event(event).await,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    self.input_rx = None;
                    break;
                }
            }
        }

        changed
    }

    pub(super) async fn handle_raw_input_event(
        &mut self,
        event: crate::raw_input::RawInputEvent,
    ) -> bool {
        let previous_mode = self.state.mode;
        let changed = match event {
            crate::raw_input::RawInputEvent::Key(key) => {
                let key_id = repeat_key_identity(&key);
                match key.kind {
                    crossterm::event::KeyEventKind::Press => {
                        if self.state.mode == Mode::Terminal {
                            self.suppressed_repeat_keys.remove(&key_id);
                        } else {
                            self.suppressed_repeat_keys.insert(key_id);
                        }
                        self.handle_key(key).await;
                        true
                    }
                    crossterm::event::KeyEventKind::Repeat => {
                        if self.state.mode == Mode::Terminal
                            && !self.suppressed_repeat_keys.contains(&key_id)
                        {
                            self.handle_key(key).await;
                            true
                        } else {
                            false
                        }
                    }
                    crossterm::event::KeyEventKind::Release => {
                        self.suppressed_repeat_keys.remove(&key_id);
                        false
                    }
                }
            }
            crate::raw_input::RawInputEvent::Paste(text) => {
                self.handle_paste(text).await;
                true
            }
            crate::raw_input::RawInputEvent::Mouse(mouse) => {
                if self.state.mouse_capture {
                    self.handle_mouse(mouse);
                } else {
                    self.state
                        .handle_pane_mouse_only(&self.terminal_runtimes, mouse);
                }
                true
            }
            crate::raw_input::RawInputEvent::OuterFocusGained => {
                if self.state.redraw_on_focus_gained {
                    self.request_full_redraw();
                }
                self.state.outer_terminal_focus = Some(true);
                self.state.mark_active_tab_seen();
                true
            }
            crate::raw_input::RawInputEvent::OuterFocusLost => {
                self.state.outer_terminal_focus = Some(false);
                false
            }
            crate::raw_input::RawInputEvent::HostDefaultColor { kind, color } => {
                self.update_host_terminal_theme(kind, color)
            }
            crate::raw_input::RawInputEvent::Unsupported => false,
        };
        self.sync_prefix_input_source(previous_mode);
        self.shutdown_detached_terminal_runtimes();
        changed
    }

    fn handle_resize_poll(&mut self) -> bool {
        let Ok(size) = terminal::size() else {
            return false;
        };
        if self.last_terminal_size != Some(size) {
            self.last_terminal_size = Some(size);
            return true;
        }
        false
    }

    pub(crate) fn handle_scheduled_tasks(&mut self, now: Instant, geometry_dirty: bool) -> bool {
        let mut changed = false;
        let mut resized = false;

        self.sync_animation_timer(now);

        if now >= self.next_resize_poll {
            resized = self.handle_resize_poll();
            changed |= resized;
            self.next_resize_poll = now + RESIZE_POLL_INTERVAL;
        }

        if self
            .config_diagnostic_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.config_diagnostic_deadline = None;
            self.state.config_diagnostic = None;
            changed = true;
        }

        if self.toast_deadline.is_some_and(|deadline| now >= deadline) {
            self.toast_deadline = None;
            self.state.toast = None;
            changed = true;
        }

        if self
            .copy_feedback_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.copy_feedback_deadline = None;
            self.state.copy_feedback = None;
            changed = true;
        }

        if self
            .next_animation_tick
            .is_some_and(|deadline| now >= deadline)
        {
            self.state.spinner_tick = self.state.spinner_tick.wrapping_add(1);
            self.next_animation_tick = Some(now + ANIMATION_INTERVAL);
            changed = true;
        }

        if self
            .selection_autoscroll_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.tick_selection_autoscroll(now);
            changed = true;
        }

        changed |= self.clear_due_selection_highlight(now);

        // #130: commit any background completions that have now dwelled past the
        // attention settle (re-arm `●` + fire the completion sound/toast) and
        // broadcast each resulting Idle→Done status change to API subscribers.
        let settled = self
            .state
            .commit_settled_completions(now, &self.terminal_runtimes);
        // #175 M1: queued messages deliver exactly at the settle — mirrored
        // in the headless loop (the #25 dual-loop lesson).
        self.deliver_settled_messages(&settled);
        for update in &settled {
            self.emit_pane_state_update(update);
        }
        changed |= !settled.is_empty();

        self.start_git_status_refresh_if_due(now);

        if self
            .next_auto_update_check
            .is_some_and(|deadline| now >= deadline)
        {
            self.run_auto_update_check();
        }

        if self
            .session_save_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.save_session_now();
        }

        if let Some(deadline) = self
            .agent_metadata_deadline
            .filter(|deadline| now >= *deadline)
        {
            let previous_toast = self.state.toast.clone();
            for update in self.state.expire_agent_metadata_at(deadline, now) {
                self.refresh_new_flock_toast_context_for_update(&update, &previous_toast);
                self.emit_pane_state_update(&update);
            }
            self.sync_agent_metadata_deadline();
            changed = true;
        }

        // #175 phase 4: tick the check runner. dispatch_due_script_checks
        // returns true when it either dispatched a run or emitted a
        // heartbeat — both mean the loop should render (an event landed).
        changed |= self.dispatch_due_script_checks(now);

        if geometry_dirty || resized {
            self.pending_agent_resume_deadline = None;
        } else {
            self.sync_pending_agent_resume_deadline(now);
            changed |= self.start_pending_agent_resumes(self.pending_agent_resume_due(now));
        }
        self.sync_animation_timer(now);
        changed
    }

    /// Clears temporary copied-token highlights, such as after double-click copy.
    pub(crate) fn clear_due_selection_highlight(&mut self, now: Instant) -> bool {
        if self
            .selection_highlight_clear_deadline
            .is_none_or(|deadline| now < deadline)
        {
            return false;
        }

        self.selection_highlight_clear_deadline = None;
        if self
            .state
            .selection
            .as_ref()
            .is_some_and(|selection| !selection.is_in_progress())
        {
            self.state.clear_selection();
            return true;
        }
        false
    }

    pub(crate) fn sync_agent_metadata_deadline(&mut self) {
        self.agent_metadata_deadline = self.state.next_agent_metadata_expiry();
    }

    pub(crate) fn sync_animation_timer(&mut self, now: Instant) {
        self.sync_animation_timer_with_interval(now, ANIMATION_INTERVAL);
    }

    pub(crate) fn sync_headless_animation_timer(&mut self, now: Instant) {
        self.sync_animation_timer_with_interval(now, crate::app::HEADLESS_ANIMATION_INTERVAL);
    }

    fn sync_animation_timer_with_interval(&mut self, now: Instant, interval: Duration) {
        // The idle "flock" (stage-1 bars + stage-2 screensaver) only animates
        // while someone is watching.
        let flock_on_screen = self.has_foreground_viewer
            && (self.state.flock_phase().is_some() || self.state.screensaver_phase().is_some());
        if self.agent_panel_has_animation() || flock_on_screen {
            self.next_animation_tick.get_or_insert(now + interval);
        } else if self.has_foreground_viewer {
            // Nothing animating yet, but an idle stage may wander in — wake the
            // loop at the earliest ENABLED threshold so it can start. None when
            // idle animations are disabled via `[ui.idle]` (#16), so the loop
            // isn't woken for an animation that will never render.
            self.next_animation_tick = self
                .state
                .config
                .ui
                .idle
                .next_idle_wake_after()
                .and_then(|after| self.state.last_interaction.checked_add(after))
                .filter(|due| *due > now);
        } else {
            self.next_animation_tick = None;
        }
    }

    fn agent_panel_has_animation(&self) -> bool {
        match self.state.agent_panel_scope() {
            crate::app::state::AgentPanelScope::CurrentWorkspace => self
                .state
                .active
                .and_then(|idx| self.state.workspaces.get(idx))
                .is_some_and(|ws| ws.has_working_pane(&self.state.terminals)),
            crate::app::state::AgentPanelScope::AllWorkspaces => self
                .state
                .workspaces
                .iter()
                .any(|ws| ws.has_working_pane(&self.state.terminals)),
        }
    }

    pub(crate) fn tick_selection_autoscroll(&mut self, now: Instant) {
        let Some(autoscroll) = self.state.selection_autoscroll.clone() else {
            // Self-heal: state cleared but deadline leaked
            self.selection_autoscroll_deadline = None;
            return;
        };

        // Selection must still be in progress for autoscroll to continue
        let Some(pane_id) = self.state.selection.as_ref().map(|s| s.pane_id) else {
            self.stop_selection_autoscroll();
            return;
        };
        if !self
            .state
            .selection
            .as_ref()
            .is_some_and(|s| s.is_dragging())
        {
            self.stop_selection_autoscroll();
            return;
        }

        // Rect-change detection: if inner_rect changed since drag, stop
        let current_rect = self
            .state
            .pane_info_by_id(pane_id)
            .map(|info| info.inner_rect);
        if current_rect != Some(autoscroll.inner_rect) {
            self.stop_selection_autoscroll();
            return;
        }

        // Scrollback boundary detection via ScrollMetrics — fail-closed if unavailable
        let Some(metrics) = self
            .state
            .pane_scroll_metrics(&self.terminal_runtimes, pane_id)
        else {
            self.stop_selection_autoscroll();
            return;
        };
        match autoscroll.direction {
            crate::app::state::SelectionAutoscrollDirection::Up => {
                let at_top = metrics.offset_from_bottom >= metrics.max_offset_from_bottom;
                if at_top {
                    self.stop_selection_autoscroll();
                    return;
                }
                self.state
                    .scroll_pane_up(&self.terminal_runtimes, pane_id, 1);
            }
            crate::app::state::SelectionAutoscrollDirection::Down => {
                let at_bottom = metrics.offset_from_bottom == 0;
                if at_bottom {
                    self.stop_selection_autoscroll();
                    return;
                }
                self.state
                    .scroll_pane_down(&self.terminal_runtimes, pane_id, 1);
            }
        }

        // Extend selection cursor to last known mouse position
        self.state.update_selection_cursor(
            &self.terminal_runtimes,
            pane_id,
            autoscroll.last_mouse_screen_col,
            autoscroll.last_mouse_screen_row,
        );

        // Reschedule
        self.selection_autoscroll_deadline = Some(now + SELECTION_AUTOSCROLL_INTERVAL);
    }

    pub(crate) fn stop_selection_autoscroll(&mut self) {
        self.state.stop_selection_autoscroll_state();
        self.selection_autoscroll_deadline = None;
    }

    pub(crate) fn can_render_now(&self, now: Instant) -> bool {
        match self.last_render_at {
            Some(last_render_at) => now.duration_since(last_render_at) >= MIN_RENDER_INTERVAL,
            None => true,
        }
    }

    pub(crate) fn run_auto_update_check(&mut self) {
        if !auto_updates_enabled(self.no_session) {
            self.next_auto_update_check = None;
            return;
        }

        self.next_auto_update_check = self
            .state
            .update_available
            .is_none()
            .then_some(Instant::now() + AUTO_UPDATE_CHECK_INTERVAL);

        if self.state.update_available.is_some() {
            return;
        }

        let update_tx = self.event_tx.clone();
        std::thread::spawn(move || crate::update::auto_update(update_tx));
    }

    pub(crate) fn start_git_status_refresh_if_due(&mut self, now: Instant) {
        let Some(deadline) = self.git_refresh_deadline() else {
            return;
        };

        if now < deadline {
            return;
        }

        let workspaces = self.workspace_git_refresh_items();

        if workspaces.is_empty() {
            self.last_git_remote_status_refresh = now;
            return;
        }

        self.git_refresh_in_flight = true;
        let event_tx = self.event_tx.clone();
        let cache = self.git_status_cache.clone();
        std::thread::spawn(move || {
            let output = refresh_workspace_git_statuses_with_cache(workspaces, &cache);
            let _ = event_tx.blocking_send(AppEvent::GitStatusRefreshed {
                results: output.results,
                cache_updates: output.cache_updates,
            });
        });
    }

    pub(crate) fn mark_git_status_refresh_due(&mut self, now: Instant) {
        if self.git_refresh_in_flight {
            self.git_refresh_due_after_in_flight = true;
            return;
        }
        self.last_git_remote_status_refresh = now
            .checked_sub(GIT_REMOTE_STATUS_REFRESH_INTERVAL)
            .unwrap_or(now);
        self.git_refresh_due_after_in_flight = false;
    }

    pub(crate) fn git_refresh_deadline(&self) -> Option<Instant> {
        (!self.git_refresh_in_flight && !self.state.workspaces.is_empty())
            .then_some(self.last_git_remote_status_refresh + GIT_REMOTE_STATUS_REFRESH_INTERVAL)
    }

    pub(crate) fn next_loop_deadline(&self, now: Instant, needs_render: bool) -> Option<Instant> {
        self.next_loop_deadline_with_resize_poll(now, needs_render, true, true)
    }

    pub(crate) fn next_headless_loop_deadline_with_git_refresh(
        &self,
        now: Instant,
        needs_render: bool,
        include_git_refresh: bool,
    ) -> Option<Instant> {
        self.next_loop_deadline_with_resize_poll(now, needs_render, false, include_git_refresh)
    }

    fn next_loop_deadline_with_resize_poll(
        &self,
        now: Instant,
        needs_render: bool,
        include_resize_poll: bool,
        include_git_refresh: bool,
    ) -> Option<Instant> {
        let render_deadline = if needs_render {
            self.last_render_at
                .map(|last_render_at| last_render_at + MIN_RENDER_INTERVAL)
                .filter(|deadline| *deadline > now)
        } else {
            None
        };

        [
            include_resize_poll.then_some(self.next_resize_poll),
            self.config_diagnostic_deadline,
            self.toast_deadline,
            self.copy_feedback_deadline,
            self.next_animation_tick,
            include_git_refresh
                .then(|| self.git_refresh_deadline())
                .flatten(),
            self.next_auto_update_check,
            self.agent_metadata_deadline,
            // #175 phase 4: wake for the next runnable script check (or its
            // heartbeat), so the runner ticks even in an otherwise quiet loop.
            self.checks_next_deadline,
            self.checks_heartbeat_deadline,
            self.pending_agent_resume_deadline,
            self.session_save_deadline,
            self.selection_autoscroll_deadline,
            self.selection_highlight_clear_deadline,
            render_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn workspace_git_refresh_items(&self) -> Vec<WorkspaceGitRefreshItem> {
        self.state
            .workspaces
            .iter()
            .filter_map(|ws| {
                let cwd =
                    ws.resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)?;
                let git_key = crate::workspace::git_status_cache_key(&cwd);
                let cache_key = git_key.unwrap_or_else(|| cwd.clone());
                Some(WorkspaceGitRefreshItem {
                    workspace_id: ws.id.clone(),
                    resolved_identity_cwd: cwd,
                    cache_key,
                })
            })
            .collect()
    }

    pub(crate) fn drain_internal_events(&mut self) -> bool {
        self.drain_internal_events_up_to(super::APP_EVENT_DRAIN_LIMIT)
    }

    pub(crate) fn drain_all_internal_events(&mut self) -> bool {
        let mut had_event = false;
        while self.drain_internal_events_up_to(super::APP_EVENT_DRAIN_LIMIT) {
            had_event = true;
        }
        had_event
    }

    fn drain_internal_events_up_to(&mut self, limit: usize) -> bool {
        let mut had_event = false;
        for _ in 0..limit {
            let Ok(ev) = self.event_rx.try_recv() else {
                break;
            };
            had_event = true;
            self.handle_internal_event_with_prefix_sync(ev);
        }
        had_event
    }

    /// #175 phase 4: scan the runner for due script checks, spawn one worker
    /// per RunnableCheck (the worker sends `AppEvent::CheckCompleted` back
    /// via `event_tx`), refresh the runner's next-due deadline, and emit
    /// `ChecksHeartbeat` when its window elapses. Returns true if either
    /// path emitted a persisted event.
    ///
    /// Mirrored in both the TUI runtime loop and the headless server loop
    /// (the #25 dual-loop rule): this helper is called from
    /// `handle_scheduled_tasks` (TUI) and `handle_scheduled_tasks_headless`
    /// (headless).
    pub(crate) fn dispatch_due_script_checks(&mut self, now: Instant) -> bool {
        if !self.state.config.checks.enable {
            self.checks_next_deadline = None;
            self.checks_heartbeat_deadline = None;
            return false;
        }
        let mut changed = false;

        if self
            .checks_next_deadline
            .is_none_or(|deadline| now >= deadline)
        {
            let runnable = self.checks_runner.next_runnable(now);
            for job in runnable {
                let event_tx = self.event_tx.clone();
                std::thread::spawn(move || {
                    let start = Instant::now();
                    let (outcome, _output) = crate::checks::run_script(&job.check);
                    let duration_ms = start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
                    let _ = event_tx.blocking_send(crate::events::AppEvent::CheckCompleted {
                        name: job.name,
                        outcome,
                        duration_ms,
                    });
                });
                changed = true;
            }
            self.checks_next_deadline = self.checks_runner.next_due(now);
        }

        if self
            .checks_heartbeat_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.event_hub.push(crate::api::schema::EventEnvelope {
                event: crate::api::schema::EventKind::ChecksHeartbeat,
                data: crate::api::schema::EventData::ChecksHeartbeat {
                    runs: self.checks_run_count,
                    errors: self.checks_error_count,
                },
            });
            let heartbeat_secs = self.state.config.checks.heartbeat_secs.max(1);
            self.checks_heartbeat_deadline = Some(now + Duration::from_secs(heartbeat_secs));
            changed = true;
        }

        changed
    }

    /// #175 phase 4: fold one completed script check back into the runner
    /// and emit the durable event pair (`CheckRan` always; `CheckErrored` on
    /// Error, `CheckFired` on a FireDecision). Called from the shared
    /// internal-event handler; the notification-side dispatch of the
    /// FireDecision's `on_fire: ActionSpec` runs here too so both loops
    /// share one code path.
    pub(crate) fn handle_check_completed(
        &mut self,
        name: String,
        outcome: crate::checks::Outcome,
        duration_ms: u64,
    ) {
        self.checks_run_count = self.checks_run_count.saturating_add(1);
        let outcome_label = match &outcome {
            crate::checks::Outcome::Fire => "fire",
            crate::checks::Outcome::Pass => "pass",
            crate::checks::Outcome::Error(_) => "error",
        };
        self.event_hub.push(crate::api::schema::EventEnvelope {
            event: crate::api::schema::EventKind::CheckRan,
            data: crate::api::schema::EventData::CheckRan {
                name: name.clone(),
                outcome: outcome_label.to_string(),
                duration_ms,
            },
        });
        if let crate::checks::Outcome::Error(reason) = &outcome {
            self.checks_error_count = self.checks_error_count.saturating_add(1);
            self.event_hub.push(crate::api::schema::EventEnvelope {
                event: crate::api::schema::EventKind::CheckErrored,
                data: crate::api::schema::EventData::CheckErrored {
                    name: name.clone(),
                    reason: reason.clone(),
                },
            });
        }

        let decision = self.checks_runner.complete(&name, outcome, Instant::now());
        // Whether or not we fire, the runner's next_due changed — refresh
        // the scheduling deadline so the loop sleeps to the right point.
        self.checks_next_deadline = self.checks_runner.next_due(Instant::now());

        if let Some(decision) = decision {
            self.event_hub.push(crate::api::schema::EventEnvelope {
                event: crate::api::schema::EventKind::CheckFired,
                data: crate::api::schema::EventData::CheckFired {
                    name: decision.name.clone(),
                    episode: decision.episode.clone(),
                },
            });
            self.dispatch_check_action(&decision);
        }
    }

    /// Route a `FireDecision.action` through the existing notification path
    /// so `[ui.toast]` delivery policy applies uniformly. `ActionSpec::Event`
    /// is fire-and-forget — the durable `CheckFired` event is already on the
    /// hub by this point; the Event variant just carries a semantic label
    /// external listeners can key on.
    fn dispatch_check_action(&mut self, decision: &crate::checks::runner::FireDecision) {
        use crate::api::schema::NotificationShowParams;
        use crate::checks::ActionSpec;

        match &decision.action {
            ActionSpec::Notify { title, sound } => {
                let title = if title.is_empty() {
                    format!("check fired: {}", decision.name)
                } else {
                    title.clone()
                };
                let params = NotificationShowParams {
                    title,
                    body: Some(format!("episode {}", decision.episode)),
                    position: None,
                    sound: *sound,
                };
                // Reuse the API's notification-show shape so the toast /
                // sound / terminal / system routing lives in ONE place.
                let request_id = format!("check:{}", decision.episode);
                let _ = self.handle_api_request_after_internal_events_drained(
                    crate::api::schema::Request {
                        id: request_id,
                        method: crate::api::schema::Method::NotificationShow(params),
                    },
                );
            }
            ActionSpec::Event { label } => {
                let _ = label;
                // No-op: the CheckFired event above IS the dispatch.
                // A follow-up commit can enrich it with the label.
            }
        }
    }

    /// Helper used by tests to force a heartbeat now.
    #[cfg(test)]
    pub(crate) fn force_checks_heartbeat_now(&mut self) {
        self.checks_heartbeat_deadline = Some(Instant::now());
    }
}

pub(crate) fn deduplicate_git_refresh_items(
    items: Vec<WorkspaceGitRefreshItem>,
    cache: &HashMap<std::path::PathBuf, GitStatusCacheEntry>,
) -> Vec<WorkspaceGitRefreshJob> {
    let mut indexes = HashMap::<std::path::PathBuf, usize>::new();
    let mut jobs = Vec::<WorkspaceGitRefreshJob>::new();

    for item in items {
        let target = WorkspaceGitRefreshTarget {
            workspace_id: item.workspace_id,
            resolved_identity_cwd: item.resolved_identity_cwd.clone(),
        };
        if let Some(&index) = indexes.get(&item.cache_key) {
            jobs[index].targets.push(target);
            continue;
        }

        let status_cwd = item.cache_key.clone();
        let cached = cache.get(&item.cache_key).cloned();
        indexes.insert(item.cache_key, jobs.len());
        jobs.push(WorkspaceGitRefreshJob {
            cache_key: status_cwd.clone(),
            status_cwd,
            cached,
            targets: vec![target],
        });
    }

    jobs
}

pub(crate) fn refresh_workspace_git_statuses_with_cache(
    items: Vec<WorkspaceGitRefreshItem>,
    cache: &HashMap<std::path::PathBuf, GitStatusCacheEntry>,
) -> WorkspaceGitRefreshOutput {
    let mut results = Vec::new();
    let mut cache_updates = Vec::new();

    for job in deduplicate_git_refresh_items(items, cache) {
        let (snapshot, cache_entry) =
            Workspace::git_status_snapshot_for_cwd_with_cache(&job.status_cwd, job.cached.as_ref());
        if let Some(cache_entry) = cache_entry {
            cache_updates.push((job.cache_key.clone(), cache_entry));
        }
        results.extend(job.targets.into_iter().map(move |target| {
            snapshot
                .clone()
                .into_workspace_status(target.workspace_id, target.resolved_identity_cwd)
        }));
    }

    WorkspaceGitRefreshOutput {
        results,
        cache_updates,
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Tests exec real git to prime fixtures — TracedCommand polices product code (logging redesign PR-3).
mod tests {
    use super::*;
    use crate::app::state;
    use crate::workspace::Workspace;
    use std::path::PathBuf;

    fn test_app_with_pane() -> (super::super::App, crate::layout::PaneId) {
        let mut app = super::super::App::new(
            &crate::config::Config::default(),
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        let ws = Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        app.state.workspaces.push(ws);
        app.state.active = Some(0);
        app.state.view.pane_infos.push(crate::layout::PaneInfo {
            id: pane_id,
            rect: ratatui::layout::Rect::new(0, 0, 80, 24),
            inner_rect: ratatui::layout::Rect::new(0, 0, 80, 24),
            scrollbar_rect: None,
            header_rect: None,
            is_focused: true,
        });
        (app, pane_id)
    }

    #[test]
    fn git_refresh_deduplicates_workspaces_with_same_cache_key() {
        let repo =
            std::env::temp_dir().join(format!("flock-git-refresh-dedupe-{}", std::process::id()));
        let nested = repo.join("nested");
        let other = repo.join("other");
        std::fs::create_dir_all(&nested).expect("create nested dir");
        std::fs::create_dir_all(&other).expect("create other dir");
        std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("init")
            .output()
            .expect("run git init");

        let output = refresh_workspace_git_statuses_with_cache(
            vec![
                WorkspaceGitRefreshItem {
                    workspace_id: "one".into(),
                    resolved_identity_cwd: nested.clone(),
                    cache_key: repo.clone(),
                },
                WorkspaceGitRefreshItem {
                    workspace_id: "two".into(),
                    resolved_identity_cwd: other.clone(),
                    cache_key: repo.clone(),
                },
            ],
            &HashMap::new(),
        );

        assert_eq!(output.cache_updates.len(), 1);
        assert_eq!(output.cache_updates[0].0, repo);
        assert_eq!(output.results.len(), 2);
        assert_eq!(output.results[0].workspace_id, "one");
        assert_eq!(
            output.results[0].resolved_identity_cwd,
            PathBuf::from(&nested)
        );
        assert_eq!(output.results[1].workspace_id, "two");
        assert_eq!(
            output.results[1].resolved_identity_cwd,
            PathBuf::from(&other)
        );

        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn git_refresh_items_use_cwd_cache_key_for_non_git_cwd() {
        let mut app = super::super::App::new(
            &crate::config::Config::default(),
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        let cwd = std::env::temp_dir().join(format!("flock-non-git-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).expect("create temp cwd");
        let mut ws = Workspace::test_new("test");
        ws.identity_cwd = cwd.clone();
        ws.tabs.clear();
        app.state.workspaces.push(ws);

        let items = app.workspace_git_refresh_items();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].cache_key, cwd);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn headless_deadline_can_suppress_git_refresh_timer() {
        let mut app = super::super::App::new(
            &crate::config::Config::default(),
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        app.state.workspaces.push(Workspace::test_new("test"));
        let now = Instant::now();
        app.last_git_remote_status_refresh = now - super::super::GIT_REMOTE_STATUS_REFRESH_INTERVAL;

        assert_eq!(
            app.next_headless_loop_deadline_with_git_refresh(now, false, false),
            None
        );
        assert_eq!(
            app.next_headless_loop_deadline_with_git_refresh(now, false, true),
            Some(now)
        );
    }

    #[test]
    fn git_refresh_due_request_survives_in_flight_refresh() {
        let mut app = super::super::App::new(
            &crate::config::Config::default(),
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        let now = Instant::now();
        app.git_refresh_in_flight = true;

        app.mark_git_status_refresh_due(now);
        assert!(app.git_refresh_due_after_in_flight);

        app.handle_internal_event(crate::events::AppEvent::GitStatusRefreshed {
            results: Vec::new(),
            cache_updates: Vec::new(),
        });

        assert!(!app.git_refresh_in_flight);
        assert!(!app.git_refresh_due_after_in_flight);
        assert_eq!(app.git_refresh_deadline(), None);

        app.state.workspaces.push(Workspace::test_new("test"));
        let deadline = app
            .git_refresh_deadline()
            .expect("refresh should be due once a workspace exists");
        assert!(deadline <= Instant::now());
    }

    #[test]
    fn tick_selection_autoscroll_stops_when_metrics_unavailable() {
        // Without a runtime, pane_scroll_metrics returns None.
        // Fail-closed: stop autoscroll instead of rescheduling forever.
        let (mut app, pane_id) = test_app_with_pane();
        let now = Instant::now();
        let mut sel = crate::selection::Selection::anchor(pane_id, 0, 0, None);
        // Drag to a different cell so it becomes Dragging
        sel.drag(5, 5, ratatui::layout::Rect::new(0, 0, 80, 24), None);
        app.state.selection = Some(sel);
        app.state.selection_autoscroll = Some(state::SelectionAutoscroll {
            direction: state::SelectionAutoscrollDirection::Down,
            last_mouse_screen_col: 5,
            last_mouse_screen_row: 23,
            inner_rect: ratatui::layout::Rect::new(0, 0, 80, 24),
        });
        app.selection_autoscroll_deadline = Some(now);
        app.tick_selection_autoscroll(now);
        // Should stop because no runtime metrics available
        assert!(app.state.selection_autoscroll.is_none());
        assert!(app.selection_autoscroll_deadline.is_none());
    }

    #[test]
    fn tick_selection_autoscroll_stops_when_selection_done() {
        let (mut app, pane_id) = test_app_with_pane();
        let now = Instant::now();
        // Create a selection that is already finished (not in progress)
        let mut sel = crate::selection::Selection::anchor(pane_id, 0, 0, None);
        // Drag to a different cell so it becomes visible, then finish
        sel.drag(5, 5, ratatui::layout::Rect::new(0, 0, 80, 24), None);
        sel.finish(); // now it's Done, not in progress
        app.state.selection = Some(sel);
        app.state.selection_autoscroll = Some(state::SelectionAutoscroll {
            direction: state::SelectionAutoscrollDirection::Down,
            last_mouse_screen_col: 0,
            last_mouse_screen_row: 23,
            inner_rect: ratatui::layout::Rect::new(0, 0, 80, 24),
        });
        app.selection_autoscroll_deadline = Some(now);
        app.tick_selection_autoscroll(now);
        assert!(app.state.selection_autoscroll.is_none());
        assert!(app.selection_autoscroll_deadline.is_none());
    }

    #[test]
    fn tick_selection_autoscroll_stops_when_selection_cleared() {
        let (mut app, _pane_id) = test_app_with_pane();
        let now = Instant::now();
        app.state.selection = None;
        app.state.selection_autoscroll = Some(state::SelectionAutoscroll {
            direction: state::SelectionAutoscrollDirection::Down,
            last_mouse_screen_col: 0,
            last_mouse_screen_row: 23,
            inner_rect: ratatui::layout::Rect::new(0, 0, 80, 24),
        });
        app.selection_autoscroll_deadline = Some(now);
        app.tick_selection_autoscroll(now);
        assert!(app.state.selection_autoscroll.is_none());
        assert!(app.selection_autoscroll_deadline.is_none());
    }

    #[test]
    fn tick_selection_autoscroll_stops_when_selection_anchored() {
        // Anchored (click, no drag) should not keep the timer running.
        let (mut app, pane_id) = test_app_with_pane();
        let now = Instant::now();
        app.state.selection = Some(crate::selection::Selection::anchor(pane_id, 0, 0, None));
        app.state.selection_autoscroll = Some(state::SelectionAutoscroll {
            direction: state::SelectionAutoscrollDirection::Down,
            last_mouse_screen_col: 0,
            last_mouse_screen_row: 23,
            inner_rect: ratatui::layout::Rect::new(0, 0, 80, 24),
        });
        app.selection_autoscroll_deadline = Some(now);
        app.tick_selection_autoscroll(now);
        assert!(app.state.selection_autoscroll.is_none());
        assert!(app.selection_autoscroll_deadline.is_none());
    }

    /// Creates an app with a real TerminalRuntime (no PTY) so scroll_metrics
    /// returns meaningful data. Uses test_with_scrollback_bytes.
    fn test_app_with_runtime(
        cols: u16,
        rows: u16,
        bytes: &[u8],
    ) -> (super::super::App, crate::layout::PaneId) {
        let mut app = super::super::App::new(
            &crate::config::Config::default(),
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        let mut ws = Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        let runtime =
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(cols, rows, 0, bytes);
        ws.tabs[0].runtimes.insert(pane_id, runtime);
        app.state.workspaces.push(ws);
        app.state.active = Some(0);
        app.state.view.pane_infos.push(crate::layout::PaneInfo {
            id: pane_id,
            rect: ratatui::layout::Rect::new(0, 0, cols, rows),
            inner_rect: ratatui::layout::Rect::new(0, 0, cols, rows),
            scrollbar_rect: None,
            header_rect: None,
            is_focused: true,
        });
        (app, pane_id)
    }

    #[tokio::test]
    async fn tick_selection_autoscroll_stops_at_scrollback_top() {
        // Create a runtime with no scrollback content — we're already at
        // the top (offset_from_bottom == max_offset_from_bottom).
        let (mut app, pane_id) = test_app_with_runtime(80, 24, &[]);
        let now = Instant::now();
        let mut sel = crate::selection::Selection::anchor(pane_id, 5, 5, None);
        sel.drag(0, 0, ratatui::layout::Rect::new(0, 0, 80, 24), None);
        app.state.selection = Some(sel);
        app.state.selection_autoscroll = Some(state::SelectionAutoscroll {
            direction: state::SelectionAutoscrollDirection::Up,
            last_mouse_screen_col: 0,
            last_mouse_screen_row: 0,
            inner_rect: ratatui::layout::Rect::new(0, 0, 80, 24),
        });
        app.selection_autoscroll_deadline = Some(now);
        app.tick_selection_autoscroll(now);
        // At scrollback top, can't scroll further up — should stop
        assert!(app.state.selection_autoscroll.is_none());
        assert!(app.selection_autoscroll_deadline.is_none());
    }

    #[tokio::test]
    async fn tick_selection_autoscroll_stops_at_scrollback_bottom() {
        // Create a runtime with no scrollback content — we're already at
        // the bottom (offset_from_bottom == 0).
        let (mut app, pane_id) = test_app_with_runtime(80, 24, &[]);
        let now = Instant::now();
        let mut sel = crate::selection::Selection::anchor(pane_id, 0, 0, None);
        sel.drag(5, 5, ratatui::layout::Rect::new(0, 0, 80, 24), None);
        app.state.selection = Some(sel);
        app.state.selection_autoscroll = Some(state::SelectionAutoscroll {
            direction: state::SelectionAutoscrollDirection::Down,
            last_mouse_screen_col: 5,
            last_mouse_screen_row: 23,
            inner_rect: ratatui::layout::Rect::new(0, 0, 80, 24),
        });
        app.selection_autoscroll_deadline = Some(now);
        app.tick_selection_autoscroll(now);
        // At scrollback bottom, can't scroll further down — should stop
        assert!(app.state.selection_autoscroll.is_none());
        assert!(app.selection_autoscroll_deadline.is_none());
    }

    #[tokio::test]
    async fn raw_input_batch_does_not_start_pending_agent_resume_before_render() {
        let (mut app, pane_id) = test_app_with_pane();
        app.state.ensure_test_terminals();
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .cloned()
            .expect("test pane should have a terminal");
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
            dedupe_key: "flock:codex\0codex\0Id\0codex-session".into(),
        });

        assert!(
            app.handle_raw_input_batch(crate::raw_input::RawInputEvent::HostDefaultColor {
                kind: crate::terminal_theme::DefaultColorKind::Foreground,
                color: crate::terminal_theme::RgbColor {
                    r: 220,
                    g: 220,
                    b: 220,
                },
            })
            .await
        );
        assert!(
            app.terminal_runtimes.get(&terminal_id).is_none(),
            "raw input can mutate active geometry; pending resumes must wait for render to refresh pane_infos"
        );
        assert!(app
            .state
            .terminals
            .get(&terminal_id)
            .expect("test terminal should still exist")
            .pending_agent_resume_plan
            .is_some());
    }

    #[tokio::test]
    async fn scheduled_tasks_do_not_start_pending_agent_resume_when_geometry_dirty() {
        let (mut app, pane_id) = test_app_with_pane();
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
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .cloned()
            .expect("test pane should have a terminal");
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
            dedupe_key: "flock:codex\0codex\0Id\0codex-session".into(),
        });
        app.pending_agent_resume_deadline = Some(Instant::now() - Duration::from_millis(1));

        assert!(!app.handle_scheduled_tasks(Instant::now(), true));
        assert!(app.terminal_runtimes.get(&terminal_id).is_none());
        assert!(app
            .state
            .terminals
            .get(&terminal_id)
            .expect("test terminal should still exist")
            .pending_agent_resume_plan
            .is_some());
        assert!(app.pending_agent_resume_deadline.is_none());
    }

    /// #175 phase 4 end-to-end: a `[[checks.script]]` fixture wired through
    /// the App tick emits `CheckRan { outcome = "fire" }` and (with
    /// `debounce = 1`) a `CheckFired` on the event hub.
    #[tokio::test]
    async fn check_runner_ticks_emit_ran_and_fired_events_on_hub() {
        use std::os::unix::fs::PermissionsExt;

        // Fixture script: exit 0 → Fire on the runner's classification.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "flock-checks-runtime-{}-{}",
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script_path = dir.join("fire.sh");
        std::fs::write(&script_path, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();

        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut config = crate::config::Config::default();
        config.checks.min_tick_secs = 1;
        config.checks.heartbeat_secs = 3600;
        config.checks.scripts = vec![crate::checks::config::ScriptCheck {
            name: "e2e".into(),
            program: script_path.clone(),
            interval_secs: 1,
            timeout_secs: 5,
            debounce: 1,
            on_fire: crate::checks::ActionSpec::Event {
                label: "e2e-fired".into(),
            },
            ..crate::checks::config::ScriptCheck::default()
        }];
        let event_hub = crate::api::EventHub::default();
        let mut app = super::super::App::new(&config, true, None, api_rx, event_hub.clone());

        // Force the runner past its first-tick warmup.
        app.checks_runner
            .force_due("e2e", Instant::now() - Duration::from_millis(1));
        app.checks_next_deadline = Some(Instant::now() - Duration::from_millis(1));

        // Tick: dispatches the check on a worker thread.
        assert!(app.dispatch_due_script_checks(Instant::now()));

        // Wait for the worker's `AppEvent::CheckCompleted` (fixture script
        // is `exit 0` — should return within a few hundred ms). We poll the
        // App's event channel and hand each event to the shared handler,
        // stopping once the hub carries the CheckRan event.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(ev) = app.event_rx.try_recv() {
                app.handle_internal_event(ev);
            }
            let events = app.event_hub.events_after(0);
            let has_ran = events.iter().any(|(_, envelope)| {
                matches!(
                    &envelope.data,
                    crate::api::schema::EventData::CheckRan { name, outcome, .. }
                        if name == "e2e" && outcome == "fire"
                )
            });
            if has_ran {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let events = app.event_hub.events_after(0);
        assert!(
            events.iter().any(|(_, envelope)| matches!(
                &envelope.data,
                crate::api::schema::EventData::CheckRan { name, outcome, .. }
                    if name == "e2e" && outcome == "fire"
            )),
            "expected CheckRan(fire) on the hub, got: {:?}",
            events
        );
        assert!(
            events.iter().any(|(_, envelope)| matches!(
                &envelope.data,
                crate::api::schema::EventData::CheckFired { name, .. } if name == "e2e"
            )),
            "expected CheckFired(e2e) on the hub, got: {:?}",
            events
        );
        assert_eq!(app.checks_run_count, 1);
        assert_eq!(app.checks_error_count, 0);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Heartbeat: when the window elapses, `ChecksHeartbeat` lands on the
    /// hub carrying the current run/error counts.
    #[tokio::test]
    async fn check_runner_heartbeat_emits_when_window_elapses() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut config = crate::config::Config::default();
        config.checks.heartbeat_secs = 1;
        let event_hub = crate::api::EventHub::default();
        let mut app = super::super::App::new(&config, true, None, api_rx, event_hub.clone());

        app.force_checks_heartbeat_now();
        assert!(app.dispatch_due_script_checks(Instant::now()));

        let events = app.event_hub.events_after(0);
        assert!(
            events.iter().any(|(_, envelope)| matches!(
                &envelope.data,
                crate::api::schema::EventData::ChecksHeartbeat { .. }
            )),
            "expected ChecksHeartbeat on the hub, got: {:?}",
            events
        );
    }

    /// Kill switch: `checks.enable = false` clears both deadlines and never
    /// spawns work.
    #[test]
    fn check_runner_disabled_never_ticks() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut config = crate::config::Config::default();
        config.checks.enable = false;
        let mut app =
            super::super::App::new(&config, true, None, api_rx, crate::api::EventHub::default());
        app.checks_next_deadline = Some(Instant::now());
        app.checks_heartbeat_deadline = Some(Instant::now());
        assert!(!app.dispatch_due_script_checks(Instant::now()));
        assert!(app.checks_next_deadline.is_none());
        assert!(app.checks_heartbeat_deadline.is_none());
    }
}
