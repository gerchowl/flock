use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

mod agents;
mod checks;
mod digest;
mod fleet;
mod integrations;
mod lineage;
mod messages;
mod panes;
pub(crate) mod peers;
mod responses;
mod revert;
mod tabs;
pub(crate) mod workspaces;
mod worktrees;

use super::{api_helpers::pane_agent_status, App, Mode, OverlayPaneState, ToastKind};
use crate::events::AppEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeExitAction {
    RespawnShell,
    ClosePane,
    /// #175 C3: the pane is hibernated — keep it, do not respawn a shell,
    /// do not tear it down. The next focus / `agent.resume` respawns the
    /// stashed argv.
    HoldHibernated,
}

/// One background `gh pr view` query of the 120s PR poll: a unique
/// (repo, branch) pair plus every workspace its result applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PrPollTarget {
    workspace_ids: Vec<String>,
    repo_root: std::path::PathBuf,
    checkout: std::path::PathBuf,
    branch: String,
}

/// PR-poll targets across ALL workspaces with a branch (#42) — linked
/// worktrees AND primary/standalone checkouts (previously linked-only,
/// which left most sidebar rows without PR state). Deduplicated by
/// (repo root, branch) so shared checkouts cost one gh call per cycle.
fn pr_poll_targets(state: &super::AppState) -> Vec<PrPollTarget> {
    let mut targets: Vec<PrPollTarget> = Vec::new();
    for ws in &state.workspaces {
        let Some(branch) = ws.branch() else {
            continue;
        };
        // #197: the action view, so a diverged workspace's PR poll follows
        // the repo its branch actually lives in.
        let (repo_root, checkout) = if let Some(space) = ws.worktree_space_here() {
            (space.repo_root.clone(), space.checkout_path.clone())
        } else if let Some(git) = ws.git_space() {
            (git.repo_root.clone(), git.repo_root.clone())
        } else {
            continue;
        };
        match targets
            .iter_mut()
            .find(|target| target.repo_root == repo_root && target.branch == branch)
        {
            Some(target) => target.workspace_ids.push(ws.id.clone()),
            None => targets.push(PrPollTarget {
                workspace_ids: vec![ws.id.clone()],
                repo_root,
                checkout,
                branch,
            }),
        }
    }
    targets
}

/// One batched PR-poll round (#294).
///
/// Two spawns per target became one HTTP request for the whole round. The
/// origin remote is resolved once per REPO rather than once per target — it is
/// static for a session, and it was previously the second of the two execs
/// nobody counted.
fn run_pr_poll_round(
    targets: Vec<PrPollTarget>,
) -> Result<Vec<(String, Option<crate::worktree::PrStateInfo>)>, crate::pr_poll::PrPollErrorKind> {
    use std::collections::HashMap;

    let mut repo_for_root: HashMap<std::path::PathBuf, Option<String>> = HashMap::new();
    let mut queries: Vec<crate::pr_poll::PrQuery> = Vec::new();
    // Index alongside the queries so one query's answer can fan out to every
    // workspace sharing that (repo, branch).
    let mut fanout: Vec<(crate::pr_poll::PrQuery, Vec<String>)> = Vec::new();
    let mut unresolved: Vec<String> = Vec::new();

    for target in targets {
        let repo = repo_for_root
            .entry(target.repo_root.clone())
            .or_insert_with(|| crate::worktree::github_repo_for_root(&target.repo_root))
            .clone();
        match repo {
            Some(repo) => {
                let query = crate::pr_poll::PrQuery {
                    repo,
                    branch: target.branch.clone(),
                };
                queries.push(query.clone());
                fanout.push((query, target.workspace_ids));
            }
            // Not a GitHub remote: report no PR, exactly as before. This is a
            // per-target fact, never a round failure.
            None => unresolved.extend(target.workspace_ids),
        }
    }

    let found = crate::pr_poll::fetch_batch(&queries)?;

    let mut results: Vec<(String, Option<crate::worktree::PrStateInfo>)> = Vec::new();
    for (query, workspace_ids) in fanout {
        let state = found.get(&query).copied();
        results.extend(workspace_ids.into_iter().map(|id| (id, state)));
    }
    results.extend(unresolved.into_iter().map(|id| (id, None)));
    Ok(results)
}

impl App {
    /// Arm and spawn a transcript read for `pane_id` at the currently
    /// selected panel detail (#246). Idempotent — the terminal's own guard
    /// short-circuits when the same `(session, detail)` pair is already
    /// hydrated, so a burst of hooks (or a hook that fires while a re-read
    /// is in flight) never spawns a reader per event. Off-thread: the file
    /// reaches ~15 MB and this is the UI task.
    pub(crate) fn arm_transcript_read_for(&mut self, pane_id: crate::layout::PaneId) {
        let Some((session_id, detail)) = self.state.take_due_transcript_hydration(pane_id) else {
            return;
        };
        let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
            return;
        };
        let event_tx = self.event_tx.clone();
        let delivered_session = session_id.clone();
        crate::agent_transcript::spawn_load(home, session_id, detail, move |turns| {
            // Always deliver, including the empty/failed case: the receiver
            // re-arms on empty so a transcript the agent had not written yet
            // is retried instead of the pane never hydrating again.
            let _ = event_tx.blocking_send(crate::events::AppEvent::TranscriptHydrated {
                pane_id,
                session_id: delivered_session,
                detail,
                turns,
            });
        });
    }

    pub(crate) fn handle_internal_event(&mut self, ev: AppEvent) {
        if let AppEvent::ClipboardWrite { content } = ev {
            #[cfg(not(test))]
            crate::selection::write_osc52_bytes(&content);
            #[cfg(test)]
            let _ = content;
            self.show_clipboard_feedback();
            return;
        }

        if let AppEvent::GitStatusRefreshed {
            results,
            cache_updates,
        } = ev
        {
            self.git_refresh_in_flight = false;
            // Health (#295): the arrival of this event IS the refresh's
            // success signal. A round that panicked in the worker never
            // reaches here — it is reaped on the next dispatch tick instead.
            self.git_refresh_health
                .mark_success(std::time::Instant::now());
            // #262: the periodic probe is flock's freshness contract for every
            // other git fact it shows, so it is also the right cadence for the
            // memoized filesystem answers the render path reads (repo-family
            // verdicts, cwd-derived labels). Re-deriving them once per round
            // costs O(workspaces) syscalls per 1.5s instead of per frame, and
            // keeps a `git init` or a hand-moved checkout from being invisible
            // until the next worktree op.
            crate::workspace::git::invalidate_path_canonicalization();
            for (key, entry) in cache_updates {
                self.git_status_cache.insert(key, entry);
            }
            if self.git_refresh_due_after_in_flight {
                self.mark_git_status_refresh_due(Instant::now());
                self.git_refresh_due_after_in_flight = false;
            } else {
                self.last_git_remote_status_refresh = Instant::now();
            }
            let adopted = self.state.adopt_external_worktrees();
            if self
                .state
                .apply_workspace_git_statuses(&self.terminal_runtimes, results)
                || adopted
            {
                self.render_dirty.store(true, Ordering::Release);
                self.render_notify.notify_one();
            }
            return;
        }

        if let AppEvent::WorktreeAddFinished(result) = ev {
            self.handle_worktree_add_finished(result);
            return;
        }

        if let AppEvent::WorktreeRemoveFinished(result) = ev {
            self.handle_worktree_remove_finished(result);
            return;
        }

        if let AppEvent::PrStatePollDue = ev {
            // Overlap guard (#294): a round already running means the previous
            // tick has not drained. Skip rather than spawn a second — piling
            // rounds up is precisely what turned a slow host into a collapsing
            // one. Mirrors the peer poller, which has had this all along.
            // Reap a guard whose round cannot still be running before honouring
            // it — otherwise a worker that died without delivering its result
            // latches the guard and the poller stops forever, silently.
            let now = std::time::Instant::now();
            self.pr_poll_health.reap_stuck_round(
                now,
                crate::pr_poll::PR_POLL_MAX_ROUND_SECS,
                crate::pr_poll::PrPollErrorKind::Transport,
            );
            if self.pr_poll_health.in_flight_since.is_some() {
                self.pr_poll_health.mark_skipped();
                return;
            }
            let targets = pr_poll_targets(&self.state);
            if !targets.is_empty() {
                let event_tx = self.event_tx.clone();
                self.pr_poll_health.mark_started(std::time::Instant::now());
                std::thread::spawn(move || {
                    let outcome = run_pr_poll_round(targets);
                    let _ = event_tx.blocking_send(AppEvent::PrStatesUpdated(outcome));
                });
            }
            return;
        }

        if let AppEvent::PrStatesUpdated(outcome) = ev {
            let now = std::time::Instant::now();
            // A failed ROUND leaves the previous values in place rather than
            // clearing them: the data is stale, not known-absent, and blanking
            // every badge on a transient network blip would be a worse lie
            // than showing the last known state marked degraded.
            let results = match outcome {
                Ok(results) => {
                    self.pr_poll_health.mark_success(now);
                    results
                }
                Err(err) => {
                    // An auth failure is the one error a restart would fix, so
                    // drop the cached token and let the next round re-read it
                    // rather than pinning at `broken` until someone notices.
                    if matches!(err, crate::pr_poll::PrPollErrorKind::Auth) {
                        crate::pr_poll::forget_cached_token();
                    }
                    self.pr_poll_health.mark_failure(now, err);
                    return;
                }
            };
            let mut changed = false;
            for (workspace_id, pr_state) in results {
                if let Some(ws) = self
                    .state
                    .workspaces
                    .iter_mut()
                    .find(|ws| ws.id == workspace_id)
                {
                    if ws.pr_state != pr_state {
                        ws.pr_state = pr_state;
                        changed = true;
                    }
                }
            }
            if changed {
                self.render_dirty.store(true, Ordering::Release);
                self.render_notify.notify_one();
            }
            return;
        }

        if let AppEvent::PeerPollDue = ev {
            // #96: one worker per peer, gated by the per-peer overlap guard —
            // a slow ProxyJump peer whose previous SSH fetch is still running
            // is SKIPPED this round instead of stacking a concurrent poll.
            // Per-peer `poll_interval_secs` overrides the global cadence for
            // slow links.
            let now = std::time::Instant::now();
            let gossip = self.state.config.gossip;
            // Health (#295): reap a peer-poll guard that outlived its round
            // so a worker that never delivered `PeerSummaryFetched` (shutdown
            // race, panic) doesn't latch the aggregate in-flight forever.
            self.peer_poll_health.reap_stuck_round(
                now,
                crate::health::PEER_POLL_MAX_ROUND_SECS,
                crate::health::PeerPollErrorKind::Timeout,
            );
            for peer in self.state.peers.clone() {
                let effective = gossip.effective_poll_interval(&peer);
                if !self
                    .peer_poll_tracker
                    .should_poll_now(&peer.name, now, effective)
                {
                    // A round that could not dispatch because its predecessor
                    // was still out is a skip — visible in aggregate so
                    // "the cadence is shorter than the work" is not silent.
                    self.peer_poll_health.mark_skipped();
                    continue;
                }
                // Any dispatch counts as an attempt for the aggregate — the
                // per-peer tracker owns the fine-grained in-flight state,
                // this side just records the fleet-wide "attempted" timestamp
                // so the reap knows what to compare against.
                self.peer_poll_health.mark_started(now);
                let event_tx = self.event_tx.clone();
                std::thread::spawn(move || {
                    // The in-flight guard is released only by the event this
                    // sends, so the fetch must not be able to unwind past it
                    // (see `fetch_with_panic_guard`).
                    //
                    // Note what this does NOT recover: a panic raised while
                    // `peer_stream::request` held the peer's slot mutex leaves
                    // that slot poisoned, so the held connection is dropped and
                    // the peer falls back to one-shot ssh. That is handled
                    // where the poison is observed, in `peer_stream::request`.
                    let peer_name = peer.name.clone();
                    let fetch = crate::peers::fetch_with_panic_guard(&peer_name, || {
                        crate::peers::fetch_peer_summary(&peer)
                    });
                    let _ = event_tx.blocking_send(AppEvent::PeerSummaryFetched(fetch));
                });
            }
            return;
        }

        if let AppEvent::PeerSummaryFetched(fetch) = ev {
            // #96: release the in-flight lock BEFORE we early-return on an
            // unknown peer — otherwise a peer removed mid-flight would be
            // frozen out even if it's re-added by a later config reload.
            self.peer_poll_tracker.mark_finished(&fetch.peer);
            // Health (#295): stamp the aggregate BEFORE the unknown-peer
            // early-return. A round that landed after its peer was removed
            // still proves the local poller is alive — dropping the health
            // update here would silently turn a config edit into a "poller
            // stopped" alert.
            let now = std::time::Instant::now();
            match &fetch.result {
                Ok(_) => self.peer_poll_health.mark_success(now),
                Err(msg) => self
                    .peer_poll_health
                    .mark_failure(now, crate::health::PeerPollErrorKind::classify(msg)),
            }
            let Some(summary) = self
                .state
                .peer_summaries
                .iter_mut()
                .find(|summary| summary.peer == fetch.peer)
            else {
                // Peer was removed from config while the fetch was in flight.
                return;
            };
            match fetch.result {
                Ok(payload) => {
                    // Gossip v3 (#101): merge the polled peer's relayed_fleet
                    // into our cache BEFORE we mutate `summary`. Loop
                    // prevention rides on the origin field: we drop entries
                    // whose origin is us (the ONE full-cycle we could see —
                    // hub A polls hub B, hub B relayed A's own peers back).
                    let self_host = crate::app::api::peers::short_host_name();
                    let self_host_lower = self_host.to_ascii_lowercase();
                    for entry in payload.relayed_fleet {
                        if entry.origin.eq_ignore_ascii_case(&self_host) {
                            continue;
                        }
                        let host_key = entry
                            .host
                            .as_deref()
                            .filter(|host| !host.is_empty())
                            .unwrap_or(&entry.ssh_target)
                            .to_ascii_lowercase();
                        if host_key == self_host_lower {
                            // Never store an entry about ourselves as a
                            // relayed row — the self row lives on the
                            // origin_summary path.
                            continue;
                        }
                        // Freshest-wins across hubs. Compared on the LIVE age
                        // (origin's reading plus however long it has sat here),
                        // not the capture-time reading, so a hub that has gone
                        // quiet cannot keep winning against one still polling.
                        let materialised = crate::peers::relayed_entry_from_wire(entry);
                        let challenger_age = materialised.peer.carried_age_secs();
                        let insert = match self.state.relayed_fleet_cache.get(&host_key) {
                            Some(existing) => {
                                match (existing.peer.carried_age_secs(), challenger_age) {
                                    (Some(cur), Some(new)) => new <= cur,
                                    (None, Some(_)) => true,
                                    (Some(_), None) => false,
                                    (None, None) => true,
                                }
                            }
                            None => true,
                        };
                        if insert {
                            self.state
                                .relayed_fleet_cache
                                .insert(host_key, materialised);
                        }
                    }
                    summary.host = (!payload.host.is_empty()).then_some(payload.host);
                    summary.version = payload.version;
                    summary.protocol = payload.protocol;
                    // #164: the peer's self-declared icon (None when unset or a
                    // v(N-1) peer that never emits it).
                    summary.icon = payload.icon;
                    // Retain the last-known health when a poll omits the system
                    // block, rather than blanking cpu/mem until the next good
                    // poll (#4). Note this is the OPPOSITE policy to `host` above
                    // (which clears on empty): a momentarily-missing system block
                    // is more plausibly a transient sampler gap than real, and
                    // showing slightly-stale cpu/mem beats a blank column. A peer
                    // that goes fully unreachable still degrades via `error` /
                    // staleness. `workspaces` stays unconditional — an empty list
                    // there is meaningful (the peer really has none), so it must
                    // still clear.
                    if payload.system.is_some() {
                        summary.system = payload.system;
                    }
                    summary.latency_ms = Some(payload.latency_ms);
                    summary.workspaces = payload.workspaces;
                    summary.last_ok = Some(std::time::Instant::now());
                    summary.error = None;
                    // Per-poll trace so a "peer row looks stale" report is
                    // diagnosable live (FLOCK_LOG=flock=debug + `flk peers
                    // logs`): shows exactly what each poll applied (#4, #67).
                    crate::logging::peer_summary_applied(
                        &summary.peer,
                        summary.host.as_deref().unwrap_or(""),
                        summary.system.is_some(),
                        summary.workspaces.len(),
                        summary.latency_ms.unwrap_or_default(),
                    );
                }
                Err(error) => summary.error = Some(error),
            }
            // Drop relay rows nothing has refreshed for long enough that they
            // carry no information (#101). Without this the cache only ever
            // grows: a host that leaves the fleet, is renamed, or is
            // re-addressed keeps its row for the process lifetime, keeps
            // rendering, and keeps riding outgoing snapshots to other servers.
            // Only meaningful now that carried freshness decays.
            self.state.evict_expired_relayed_entries();
            self.render_dirty.store(true, Ordering::Release);
            self.render_notify.notify_one();
            return;
        }

        if let AppEvent::PeerCheckoutProbed { generation, result } = ev {
            self.handle_peer_checkout_probed(generation, result);
            return;
        }

        if let AppEvent::PeerCheckoutPushed { generation, result } = ev {
            self.handle_peer_checkout_pushed(generation, result);
            return;
        }

        if let AppEvent::PeerCheckoutWorktreeReady { generation, result } = ev {
            self.handle_peer_checkout_worktree_ready(generation, result);
            return;
        }

        if let AppEvent::WorktreeKillGateFinished(result) = ev {
            self.handle_worktree_kill_gate_finished(result);
            return;
        }

        if let AppEvent::WorktreeBranchDeleteFinished(result) = ev {
            self.handle_worktree_branch_delete_finished(result);
            return;
        }

        if let AppEvent::WorktreeKillAllFinished(result) = ev {
            self.handle_worktree_kill_all_finished(result);
            return;
        }

        if let AppEvent::CheckCompleted {
            name,
            outcome,
            duration_ms,
        } = ev
        {
            self.handle_check_completed(name, outcome, duration_ms);
            return;
        }

        if let AppEvent::PaneDied { pane_id } = &ev {
            // Floating panes live outside the workspace tree: when their
            // process exits, reap the float here (this handler runs in both
            // the App and headless event loops) and skip the workspace pane
            // teardown below entirely.
            if self.state.remove_float_for_pane(*pane_id) {
                self.shutdown_detached_terminal_runtimes();
                self.render_dirty.store(true, Ordering::Release);
                self.render_notify.notify_one();
                return;
            }
            match self.runtime_exit_action(*pane_id) {
                RuntimeExitAction::RespawnShell => {
                    if self.respawn_shell_for_launch_pane(*pane_id) {
                        self.overlay_panes.remove(pane_id);
                        self.render_dirty.store(true, Ordering::Release);
                        self.render_notify.notify_one();
                        return;
                    }
                }
                RuntimeExitAction::HoldHibernated => {
                    // Drop the dead runtime but KEEP the pane — no
                    // PaneClosed, no PaneExited event; the pane sits
                    // hibernated until the next focus/resume.
                    self.shutdown_detached_terminal_runtimes();
                    self.render_dirty.store(true, Ordering::Release);
                    self.render_notify.notify_one();
                    return;
                }
                RuntimeExitAction::ClosePane => {}
            }
        }

        let overlay_state = if let AppEvent::PaneDied { pane_id } = &ev {
            self.overlay_panes.remove(pane_id)
        } else {
            None
        };

        if let AppEvent::PaneDied { pane_id } = &ev {
            if let Some((ws_idx, _)) = self.find_pane(*pane_id) {
                if let Some(public_pane_id) = self.public_pane_id(ws_idx, *pane_id) {
                    self.emit_event(crate::api::schema::EventEnvelope {
                        event: crate::api::schema::EventKind::PaneExited,
                        data: crate::api::schema::EventData::PaneExited {
                            pane_id: public_pane_id,
                            workspace_id: self.public_workspace_id(ws_idx),
                        },
                    });
                }
            }
        }

        let released_agent = if let AppEvent::HookAgentReleased {
            pane_id,
            known_agent,
            ..
        } = &ev
        {
            known_agent.map(|agent| (*pane_id, agent))
        } else {
            None
        };

        let update_ready = if let AppEvent::UpdateReady {
            version,
            install_command,
        } = &ev
        {
            Some((version.clone(), install_command.clone()))
        } else {
            None
        };
        let previous_toast = self.state.toast.clone();
        // A hook report is the first moment a pane's session id is known, so
        // it is where the transcript read is armed (#246). Off-thread: these
        // files reach ~15 MB and this is the UI task.
        if let crate::events::AppEvent::HookPromptReported { pane_id, .. }
        | crate::events::AppEvent::HookRecapReported { pane_id, .. }
        | crate::events::AppEvent::HookReplyReported { pane_id, .. } = &ev
        {
            self.arm_transcript_read_for(*pane_id);
        }
        let pane_updates = self.state.handle_app_event(ev);
        for update in &pane_updates {
            self.refresh_new_flock_toast_context_for_update(update, &previous_toast);
            self.emit_pane_state_update(update);
        }
        self.sync_agent_metadata_deadline();
        if let Some((pane_id, agent)) = released_agent {
            if pane_updates.iter().any(|update| update.pane_id == pane_id) {
                if let Some((ws_idx, _)) = self.find_pane(pane_id) {
                    if let Some(runtime) = self.state.runtime_for_pane_in_workspace(
                        &self.terminal_runtimes,
                        ws_idx,
                        pane_id,
                    ) {
                        runtime.begin_graceful_release(agent);
                    }
                }
            }
        }
        if let Some(overlay) = overlay_state {
            self.restore_overlay_after_exit(overlay);
        }

        if self.local_terminal_notifications
            && matches!(
                self.state.toast_config().delivery,
                crate::config::ToastDelivery::Terminal | crate::config::ToastDelivery::System
            )
        {
            let notify = match self.state.toast_config().delivery {
                crate::config::ToastDelivery::Terminal => crate::terminal_notify::show_notification,
                crate::config::ToastDelivery::System => crate::platform::show_desktop_notification,
                _ => unreachable!("toast delivery was checked above"),
            };

            if let Some((version, install_command)) = update_ready {
                let instruction = crate::update::update_install_instruction(&install_command);
                let _ = notify(&format!("v{version} available"), Some(&instruction));
            } else {
                for update in &pane_updates {
                    let is_active_tab = self
                        .state
                        .pane_is_in_active_tab(update.ws_idx, update.pane_id);
                    let suppress_active_tab_notifications =
                        crate::app::actions::active_tab_suppresses_notifications(
                            is_active_tab,
                            self.state.outer_terminal_focus,
                        );
                    let Some(kind) = crate::app::actions::notification_toast_for_state_change(
                        suppress_active_tab_notifications,
                        update.previous_state,
                        update.state,
                    ) else {
                        continue;
                    };
                    let Some(ws) = self.state.workspaces.get(update.ws_idx) else {
                        continue;
                    };
                    let Some(pane) = ws
                        .tabs
                        .iter()
                        .find_map(|tab| tab.panes.get(&update.pane_id))
                    else {
                        continue;
                    };
                    let Some(agent_label) = self
                        .state
                        .terminals
                        .get(&pane.attached_terminal_id)
                        .and_then(|terminal| terminal.effective_agent_label())
                    else {
                        continue;
                    };
                    let event_text = match kind {
                        ToastKind::NeedsAttention => "needs attention",
                        ToastKind::Finished => "finished",
                        ToastKind::UpdateInstalled => "updated",
                    };
                    let workspace_label =
                        ws.display_name_from(&self.state.terminals, &self.terminal_runtimes);
                    let _ = notify(
                        &format!("{} {}", agent_label, event_text),
                        Some(&crate::app::actions::notification_context(
                            ws,
                            &workspace_label,
                            update.ws_idx,
                            update.pane_id,
                        )),
                    );
                }
            }
        }

        self.sync_toast_deadline(previous_toast);
        self.shutdown_detached_terminal_runtimes();
    }

    pub(crate) fn refresh_new_flock_toast_context_for_update(
        &mut self,
        update: &crate::app::actions::PaneStateUpdate,
        previous_toast: &Option<crate::app::state::ToastNotification>,
    ) {
        if !matches!(
            self.state.toast_config().delivery,
            crate::config::ToastDelivery::Flock
        ) || self.state.toast == *previous_toast
        {
            return;
        }

        let Some(target) = self
            .state
            .toast
            .as_ref()
            .and_then(|toast| toast.target.as_ref())
        else {
            return;
        };
        if target.pane_id != update.pane_id {
            return;
        }
        let Some(ws) = self.state.workspaces.get(update.ws_idx) else {
            return;
        };
        if ws.id != target.workspace_id {
            return;
        }

        let workspace_label = ws.display_name_from(&self.state.terminals, &self.terminal_runtimes);
        let context = crate::app::actions::notification_context(
            ws,
            &workspace_label,
            update.ws_idx,
            update.pane_id,
        );
        if let Some(toast) = self.state.toast.as_mut() {
            toast.context = context;
        }
    }

    pub(crate) fn show_clipboard_feedback(&mut self) {
        if !self.state.toast_config().clipboard.enabled {
            self.state.copy_feedback = None;
            self.copy_feedback_deadline = None;
            return;
        }
        self.state.copy_feedback = Some(crate::app::state::CopyFeedback {
            message: "copied to clipboard".to_string(),
        });
        self.copy_feedback_deadline = Some(Instant::now() + super::COPY_FEEDBACK_DURATION);
    }

    fn restore_overlay_after_exit(&mut self, overlay: OverlayPaneState) {
        // Snapshot the post-exit hook BEFORE we tear the overlay down --
        // it owns paths we need to keep around (backup) past the
        // temp-file cleanup pass below.
        let post_exit = overlay.post_exit.clone();
        for temp_file in &overlay.temp_files {
            let _ = std::fs::remove_file(temp_file);
        }

        if let Some(ws) = self.state.workspaces.get_mut(overlay.ws_idx) {
            if overlay.tab_idx < ws.tabs.len() {
                ws.active_tab = overlay.tab_idx;
                let tab = &mut ws.tabs[overlay.tab_idx];
                if tab.panes.contains_key(&overlay.previous_focus) {
                    tab.layout.focus_pane(overlay.previous_focus);
                }
                tab.zoomed = overlay.previous_zoomed;

                if self.state.active == Some(overlay.ws_idx) {
                    self.state.mode = Mode::Terminal;
                }
            }
        }

        if let Some(crate::app::OverlayPostExit::ConfigEdit { target, backup }) = post_exit {
            self.finish_config_edit(target, backup);
        }
    }

    /// Called after the config-edit overlay's editor pane exited and the
    /// shell already cp'd the temp back over `target`. Reload the live
    /// config; on a Failed apply, restore the pre-edit backup and surface
    /// the diagnostics via the existing toast channel.
    pub(crate) fn finish_config_edit(
        &mut self,
        target: std::path::PathBuf,
        backup: std::path::PathBuf,
    ) {
        let report = self.apply_config_from_disk(true);
        if matches!(report.status, crate::config::ConfigReloadStatus::Failed) {
            if let Ok(backup_content) = std::fs::read_to_string(&backup) {
                if let Err(err) = std::fs::write(&target, backup_content) {
                    crate::logging::config_edit_rollback_write_failed(&target, &err.to_string());
                }
            }
            // Re-apply the now-restored base so the running state is
            // consistent with the backup we just put back on disk.
            let _ = self.apply_config_from_disk(false);
            self.state.toast = Some(crate::app::state::ToastNotification {
                kind: crate::app::state::ToastKind::NeedsAttention,
                title: "config rolled back".to_string(),
                context: crate::config::config_diagnostic_summary(&report.diagnostics)
                    .unwrap_or_else(|| "edit produced an invalid config".to_string()),
                position: None,
                target: None,
            });
        }
        let _ = std::fs::remove_file(&backup);
    }

    fn runtime_exit_action(&self, pane_id: crate::layout::PaneId) -> RuntimeExitAction {
        let Some((_, pane_state)) = self.find_pane(pane_id) else {
            return RuntimeExitAction::ClosePane;
        };
        let Some(terminal) = self.state.terminals.get(&pane_state.attached_terminal_id) else {
            return RuntimeExitAction::ClosePane;
        };

        // #175 C3: hibernation wins over the respawn-shell path. The stashed
        // resume plan means the child died BECAUSE we asked it to; keeping
        // the pane empty is the whole point.
        if terminal.hibernated_resume_plan.is_some() {
            RuntimeExitAction::HoldHibernated
        } else if terminal.respawn_shell_on_exit {
            RuntimeExitAction::RespawnShell
        } else {
            RuntimeExitAction::ClosePane
        }
    }

    fn respawn_shell_for_launch_pane(&mut self, pane_id: crate::layout::PaneId) -> bool {
        let Some((ws_idx, pane_state)) = self.find_pane(pane_id) else {
            return false;
        };
        let terminal_id = pane_state.attached_terminal_id.clone();
        let Some(terminal) = self.state.terminals.get(&terminal_id) else {
            return false;
        };

        let cwd = terminal.cwd.clone();
        let (rows, cols) = self
            .terminal_runtimes
            .get(&terminal_id)
            .map(|runtime| runtime.current_size())
            .unwrap_or_else(|| self.state.estimate_pane_size());
        let runtime = match crate::terminal::TerminalRuntime::spawn(
            pane_id,
            rows,
            cols,
            cwd,
            self.state.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
            crate::pane::PaneShellConfig::new(&self.state.default_shell, self.state.shell_mode),
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        ) {
            Ok(runtime) => runtime,
            Err(err) => {
                crate::logging::pane_respawn_shell_failed(
                    pane_id.raw(),
                    &terminal_id.to_string(),
                    &err.to_string(),
                );
                return false;
            }
        };

        self.terminal_runtimes.insert(terminal_id.clone(), runtime);
        if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
            terminal.clear_agent_runtime_identity_after_respawn();
        }
        self.state.focus_pane_in_workspace(ws_idx, pane_id);
        self.schedule_session_save();
        true
    }

    pub(crate) fn emit_pane_state_update(&self, update: &crate::app::actions::PaneStateUpdate) {
        let Some(pane_id) = self.public_pane_id(update.ws_idx, update.pane_id) else {
            return;
        };
        let workspace_id = self.public_workspace_id(update.ws_idx);

        if update.previous_agent_label != update.agent_label {
            self.emit_event(crate::api::schema::EventEnvelope {
                event: crate::api::schema::EventKind::PaneAgentDetected,
                data: crate::api::schema::EventData::PaneAgentDetected {
                    pane_id: pane_id.clone(),
                    workspace_id: workspace_id.clone(),
                    agent: update.agent_label.clone(),
                },
            });
        }

        let previous_agent_status = pane_agent_status(update.previous_state, update.previous_seen);
        let agent_status = self
            .state
            .workspaces
            .get(update.ws_idx)
            .and_then(|ws| ws.pane_state(update.pane_id))
            .map(|pane| pane_agent_status(update.state, pane.seen))
            .unwrap_or_else(|| pane_agent_status(update.state, update.seen));

        if previous_agent_status != agent_status
            || update.previous_presentation != update.presentation
        {
            let presentation = update.presentation.clone();
            self.emit_event(crate::api::schema::EventEnvelope {
                event: crate::api::schema::EventKind::PaneAgentStatusChanged,
                data: crate::api::schema::EventData::PaneAgentStatusChanged {
                    pane_id,
                    workspace_id,
                    agent_status,
                    agent: update.agent_label.clone(),
                    title: presentation.title,
                    display_agent: presentation.display_agent,
                    custom_status: presentation.custom_status,
                    state_labels: presentation.state_labels,
                },
            });
        }
    }

    pub(crate) fn sync_toast_deadline(
        &mut self,
        previous_toast: Option<crate::app::state::ToastNotification>,
    ) {
        if self.state.toast != previous_toast {
            self.toast_deadline = self.state.toast.as_ref().map(|toast| {
                let duration = match toast.kind {
                    ToastKind::NeedsAttention => Duration::from_secs(8),
                    ToastKind::Finished => Duration::from_secs(5),
                    ToastKind::UpdateInstalled => Duration::from_secs(3),
                };
                Instant::now() + duration
            });
        }
    }

    /// Resolve and publish the lifecycle events the keyboard paths queued.
    ///
    /// Called from the App loop and at the end of every socket request, so a
    /// mutation announces itself exactly once no matter which door it came
    /// through — and a caller that mutates over the socket still sees its own
    /// event published before the response is written.
    pub(crate) fn drain_pending_ui_events(&mut self) {
        if self.state.pending_ui_events.is_empty() {
            return;
        }
        for pending in std::mem::take(&mut self.state.pending_ui_events) {
            let envelope = match pending {
                crate::app::state::PendingUiEvent::PaneCreated {
                    workspace_id,
                    pane_id,
                } => {
                    // Resolved late on purpose: PaneInfo needs App-level
                    // terminal runtimes, and the pane still exists.
                    let Some(ws_idx) = self
                        .state
                        .workspaces
                        .iter()
                        .position(|ws| ws.id == workspace_id)
                    else {
                        continue;
                    };
                    let Some(pane) = self.pane_info(ws_idx, pane_id) else {
                        continue;
                    };
                    crate::api::schema::EventEnvelope {
                        event: crate::api::schema::EventKind::PaneCreated,
                        data: crate::api::schema::EventData::PaneCreated { pane },
                    }
                }
                crate::app::state::PendingUiEvent::PaneClosed { pane_id } => {
                    let workspace_id = pane_id
                        .split_once(':')
                        .map(|(ws, _)| ws.to_string())
                        .unwrap_or_default();
                    crate::api::schema::EventEnvelope {
                        event: crate::api::schema::EventKind::PaneClosed,
                        data: crate::api::schema::EventData::PaneClosed {
                            pane_id,
                            workspace_id,
                        },
                    }
                }
                crate::app::state::PendingUiEvent::TabClosed {
                    workspace_id,
                    tab_id,
                } => crate::api::schema::EventEnvelope {
                    event: crate::api::schema::EventKind::TabClosed,
                    data: crate::api::schema::EventData::TabClosed {
                        tab_id,
                        workspace_id,
                    },
                },
                crate::app::state::PendingUiEvent::WorkspaceClosed { workspace_id } => {
                    crate::api::schema::EventEnvelope {
                        event: crate::api::schema::EventKind::WorkspaceClosed,
                        data: crate::api::schema::EventData::WorkspaceClosed { workspace_id },
                    }
                }
                crate::app::state::PendingUiEvent::WorkspaceRenamed {
                    workspace_id,
                    label,
                } => crate::api::schema::EventEnvelope {
                    event: crate::api::schema::EventKind::WorkspaceRenamed,
                    data: crate::api::schema::EventData::WorkspaceRenamed {
                        workspace_id,
                        label,
                    },
                },
                crate::app::state::PendingUiEvent::TabRenamed {
                    workspace_id,
                    tab_id,
                    label,
                } => crate::api::schema::EventEnvelope {
                    event: crate::api::schema::EventKind::TabRenamed,
                    data: crate::api::schema::EventData::TabRenamed {
                        tab_id,
                        workspace_id,
                        label,
                    },
                },
            };
            self.emit_event(envelope);
        }
    }

    pub(super) fn emit_event(&self, event: crate::api::schema::EventEnvelope) {
        self.event_hub.push(event);
    }

    pub(crate) fn sync_focus_events(&mut self) {
        let current_focus = self.state.active.and_then(|idx| {
            self.state
                .workspaces
                .get(idx)
                .and_then(|ws| ws.focused_pane_id().map(|pane_id| (idx, pane_id)))
        });
        if current_focus == self.last_focus {
            return;
        }

        if let Some((ws_idx, pane_id)) = self.last_focus {
            self.send_pane_focus_event(ws_idx, pane_id, crate::ghostty::FocusEvent::Lost);
        }
        if let Some((ws_idx, pane_id)) = current_focus {
            self.send_pane_focus_event(ws_idx, pane_id, crate::ghostty::FocusEvent::Gained);
            self.emit_event(crate::api::schema::EventEnvelope {
                event: crate::api::schema::EventKind::WorkspaceFocused,
                data: crate::api::schema::EventData::WorkspaceFocused {
                    workspace_id: self.public_workspace_id(ws_idx),
                },
            });
            if let Some(tab_id) =
                self.public_tab_id(ws_idx, self.state.workspaces[ws_idx].active_tab)
            {
                self.emit_event(crate::api::schema::EventEnvelope {
                    event: crate::api::schema::EventKind::TabFocused,
                    data: crate::api::schema::EventData::TabFocused {
                        tab_id,
                        workspace_id: self.public_workspace_id(ws_idx),
                    },
                });
            }
            if let Some(public_pane_id) = self.public_pane_id(ws_idx, pane_id) {
                self.emit_event(crate::api::schema::EventEnvelope {
                    event: crate::api::schema::EventKind::PaneFocused,
                    data: crate::api::schema::EventData::PaneFocused {
                        pane_id: public_pane_id,
                        workspace_id: self.public_workspace_id(ws_idx),
                    },
                });
            }
        }

        self.last_focus = current_focus;
    }

    fn send_pane_focus_event(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        event: crate::ghostty::FocusEvent,
    ) {
        let Some(runtime) = self.state.workspaces.get(ws_idx).and_then(|_| {
            self.state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
        }) else {
            return;
        };
        runtime.try_send_focus_event(event);
    }

    pub(crate) fn handle_api_request(&mut self, request: crate::api::schema::Request) -> String {
        self.drain_all_internal_events();
        self.handle_api_request_after_internal_events_drained(request)
    }

    pub(crate) fn handle_api_request_after_internal_events_drained(
        &mut self,
        request: crate::api::schema::Request,
    ) -> String {
        let response = self.dispatch_api_request(request);
        // Publish anything the handler's state mutation queued (#175
        // ADR-0005) before the response goes out, so a socket caller sees its
        // own event on the stream.
        self.drain_pending_ui_events();
        response
    }

    fn dispatch_api_request(&mut self, request: crate::api::schema::Request) -> String {
        use crate::api::schema::{
            ErrorBody, ErrorResponse, Method, ResponseResult, SuccessResponse,
        };

        let response = match request.method {
            Method::ServerStop(_) => {
                self.state.should_quit = true;
                SuccessResponse {
                    id: request.id,
                    result: ResponseResult::Ok {},
                }
            }
            Method::ServerLiveHandoff(_) => {
                let response = ErrorResponse {
                    id: request.id,
                    error: ErrorBody {
                        code: "unsupported_in_app_mode".into(),
                        message: "live handoff is only supported by the headless server".into(),
                    },
                };
                return serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
            }
            Method::ServerReloadConfig(_) => {
                let report = self.reload_config();
                SuccessResponse {
                    id: request.id,
                    result: ResponseResult::ConfigReload {
                        status: report.status,
                        diagnostics: report.diagnostics,
                    },
                }
            }
            Method::NotificationShow(params) => {
                return self.handle_notification_show(request.id, params);
            }
            Method::PeersSummary(_) => return self.handle_peers_summary(request.id),
            Method::PeersCheckoutPrepare(params) => {
                return self.handle_peers_checkout_prepare(request.id, params)
            }
            Method::WorkspaceList(_) => return self.handle_workspace_list(request.id),
            Method::WorkspaceGet(target) => return self.handle_workspace_get(request.id, target),
            Method::WorkspaceCreate(params) => {
                return self.handle_workspace_create(request.id, params);
            }
            Method::WorkspaceFocus(target) => {
                return self.handle_workspace_focus(request.id, target)
            }
            Method::WorkspaceRename(params) => {
                return self.handle_workspace_rename(request.id, params);
            }
            Method::WorkspaceClose(target) => {
                return self.handle_workspace_close(request.id, target)
            }
            Method::WorktreeList(params) => return self.handle_worktree_list(request.id, params),
            Method::WorktreeCreate(params) => {
                return self.handle_worktree_create(request.id, params);
            }
            Method::WorktreeOpen(params) => return self.handle_worktree_open(request.id, params),
            Method::WorktreeRemove(params) => {
                return self.handle_worktree_remove(request.id, params);
            }
            Method::WorktreeKill(params) => {
                return self.handle_worktree_kill(request.id, params);
            }
            Method::TabList(params) => return self.handle_tab_list(request.id, params),
            Method::TabGet(target) => return self.handle_tab_get(request.id, target),
            Method::TabCreate(params) => return self.handle_tab_create(request.id, params),
            Method::TabFocus(target) => return self.handle_tab_focus(request.id, target),
            Method::TabRename(params) => return self.handle_tab_rename(request.id, params),
            Method::TabClose(target) => return self.handle_tab_close(request.id, target),
            Method::AgentList(_) => return self.handle_agent_list(request.id),
            Method::AgentGet(target) => return self.handle_agent_get(request.id, target),
            Method::AgentFocus(target) => return self.handle_agent_focus(request.id, target),
            Method::AgentRename(params) => return self.handle_agent_rename(request.id, params),
            Method::AgentStart(params) => return self.handle_agent_start(request.id, params),
            Method::AgentFork(params) => return self.handle_agent_fork(request.id, params),
            Method::AgentHibernate(target) => {
                return self.handle_agent_hibernate(request.id, target)
            }
            Method::AgentResume(target) => return self.handle_agent_resume(request.id, target),
            Method::AgentLineage(params) => return self.handle_agent_lineage(request.id, params),
            Method::MsgSend(params) => return self.handle_msg_send(request.id, params),
            Method::MsgReply(params) => return self.handle_msg_reply(request.id, params),
            Method::MsgList(params) => return self.handle_msg_list(request.id, params),
            Method::MsgRead(params) => return self.handle_msg_read(request.id, params),
            Method::MsgStatus(params) => return self.handle_msg_status(request.id, params),
            Method::AgentRead(params) => return self.handle_agent_read(request.id, params),
            Method::AgentSend(params) => return self.handle_agent_send(request.id, params),
            Method::PaneSplit(params) => return self.handle_pane_split(request.id, params),
            Method::PaneMove(params) => return self.handle_pane_move(request.id, params),
            Method::PaneList(params) => return self.handle_pane_list(request.id, params),
            Method::PaneGet(target) => return self.handle_pane_get(request.id, target),
            Method::PaneRename(params) => return self.handle_pane_rename(request.id, params),
            Method::PaneRead(params) => return self.handle_pane_read(request.id, params),
            Method::PaneReportAgent(params) => {
                return self.handle_pane_report_agent(request.id, params);
            }
            Method::PaneReportAgentSession(params) => {
                return self.handle_pane_report_agent_session(request.id, params);
            }
            Method::PaneReportPrompt(params) => {
                return self.handle_pane_report_prompt(request.id, params);
            }
            Method::PaneReportRecap(params) => {
                return self.handle_pane_report_recap(request.id, params);
            }
            Method::PaneReportReply(params) => {
                return self.handle_pane_report_reply(request.id, params);
            }
            Method::PaneReportMetadata(params) => {
                return self.handle_pane_report_metadata(request.id, params);
            }
            Method::PaneSetHeaderField(params) => {
                return self.handle_pane_set_header_field(request.id, params);
            }
            Method::PaneClearHeaderField(params) => {
                return self.handle_pane_clear_header_field(request.id, params);
            }
            Method::PaneClearAgentAuthority(params) => {
                return self.handle_pane_clear_agent_authority(request.id, params);
            }
            Method::PaneReleaseAgent(params) => {
                return self.handle_pane_release_agent(request.id, params);
            }
            Method::PaneSendText(params) => return self.handle_pane_send_text(request.id, params),
            Method::PaneSendInput(params) => {
                return self.handle_pane_send_input(request.id, params)
            }
            Method::PaneClose(target) => return self.handle_pane_close(request.id, target),
            Method::PaneSendKeys(params) => return self.handle_pane_send_keys(request.id, params),
            Method::IntegrationInstall(params) => {
                return self.handle_integration_install(request.id, params);
            }
            Method::IntegrationUninstall(params) => {
                return self.handle_integration_uninstall(request.id, params);
            }
            Method::ChecksList(_) => return self.handle_checks_list(request.id),
            Method::ChecksAck(target) => return self.handle_checks_ack(request.id, target),
            Method::ChecksRun(target) => return self.handle_checks_run(request.id, target),
            Method::DigestRender(params) => return self.handle_digest_render(request.id, params),
            Method::FleetPause(params) => return self.handle_fleet_pause(request.id, params),
            Method::FleetResume(_) => return self.handle_fleet_resume(request.id),
            Method::FleetStatus(_) => return self.handle_fleet_status(request.id),
            Method::RevertRun(params) => return self.handle_revert_run(request.id, params),
            _ => {
                return responses::encode_error(
                    request.id,
                    "not_implemented",
                    "method not implemented yet",
                );
            }
        };

        serde_json::to_string(&response).unwrap()
    }

    fn handle_notification_show(
        &mut self,
        id: String,
        params: crate::api::schema::NotificationShowParams,
    ) -> String {
        use crate::api::schema::{NotificationShowReason, ResponseResult};

        let requested_sound = params.sound;
        let Some(title) = sanitized_notification_text(&params.title, 80) else {
            return responses::encode_error(id, "invalid_params", "notification title is empty");
        };
        let body = params
            .body
            .as_deref()
            .and_then(|body| sanitized_notification_text(body, 240));

        let reason = match self.state.toast_config().delivery {
            crate::config::ToastDelivery::Off => NotificationShowReason::Disabled,
            crate::config::ToastDelivery::Flock => {
                if self.state.toast.is_some() {
                    NotificationShowReason::Busy
                } else if self.api_notification_rate_limited(Instant::now()) {
                    NotificationShowReason::RateLimited
                } else {
                    let previous_toast = self.state.toast.clone();
                    self.mark_api_notification_shown(Instant::now());
                    self.state.toast = Some(crate::app::state::ToastNotification {
                        kind: ToastKind::UpdateInstalled,
                        title,
                        context: body.unwrap_or_default(),
                        position: params.position,
                        target: None,
                    });
                    self.sync_toast_deadline(previous_toast);
                    self.emit_api_notification_sound(requested_sound);
                    NotificationShowReason::Shown
                }
            }
            crate::config::ToastDelivery::Terminal | crate::config::ToastDelivery::System => {
                // In headless mode, terminal/system delivery must be forwarded
                // to the foreground attached client — `local_terminal_notifications`
                // gates whether this process can emit notifications itself. When
                // unset, report NoForegroundClient so the caller can fall back
                // to a different delivery channel.
                if !self.local_terminal_notifications {
                    NotificationShowReason::NoForegroundClient
                } else if self.api_notification_rate_limited(Instant::now()) {
                    NotificationShowReason::RateLimited
                } else {
                    let notify = match self.state.toast_config().delivery {
                        crate::config::ToastDelivery::Terminal => {
                            crate::terminal_notify::show_notification
                        }
                        crate::config::ToastDelivery::System => {
                            crate::platform::show_desktop_notification
                        }
                        _ => unreachable!("notification delivery was checked above"), // guardrails-ok: outer match arm restricts delivery to Terminal | System
                    };
                    match notify(&title, body.as_deref()) {
                        Ok(true) => {
                            self.mark_api_notification_shown(Instant::now());
                            self.emit_api_notification_sound(requested_sound);
                            NotificationShowReason::Shown
                        }
                        Ok(false) | Err(_) => NotificationShowReason::NoForegroundClient,
                    }
                }
            }
        };

        responses::encode_success(
            id,
            ResponseResult::NotificationShow {
                shown: matches!(reason, NotificationShowReason::Shown),
                reason,
            },
        )
    }

    fn emit_api_notification_sound(&self, sound: crate::api::schema::NotificationShowSound) {
        if !self.state.local_sound_playback || !self.state.sound_config().allows(None) {
            return;
        }
        if let Some(sound) = sound.to_sound() {
            crate::sound::play(sound, self.state.sound_config());
        }
    }

    pub(crate) fn api_notification_rate_limited(&self, now: Instant) -> bool {
        self.last_api_notification_at
            .is_some_and(|last| now.duration_since(last) < API_NOTIFICATION_RATE_LIMIT)
    }

    pub(crate) fn mark_api_notification_shown(&mut self, now: Instant) {
        self.last_api_notification_at = Some(now);
    }
}

const API_NOTIFICATION_RATE_LIMIT: Duration = Duration::from_secs(1);

fn sanitized_notification_text(value: &str, max_chars: usize) -> Option<String> {
    let mut sanitized = String::new();
    let mut previous_space = false;
    for ch in value.chars() {
        let replacement = if ch == '\n' || ch == '\r' || ch == '\t' {
            Some(' ')
        } else if ch.is_control() {
            None
        } else {
            Some(ch)
        };
        let Some(ch) = replacement else {
            continue;
        };
        if ch.is_whitespace() {
            if previous_space {
                continue;
            }
            previous_space = true;
            sanitized.push(' ');
        } else {
            previous_space = false;
            sanitized.push(ch);
        }
        if sanitized.chars().count() >= max_chars {
            break;
        }
    }
    let sanitized = sanitized.trim().to_string();
    (!sanitized.is_empty()).then_some(sanitized)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Tests exec real git to prime fixtures — TracedCommand polices product code (logging redesign PR-3).
mod tests {
    use super::*;
    use crate::detect::{Agent, AgentState};

    fn init_repo(path: &std::path::Path) {
        let status = std::process::Command::new("git")
            .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
            .args(["init", "-q"])
            .current_dir(path)
            .status()
            .unwrap();
        assert!(status.success(), "git init failed for {}", path.display());
    }

    #[tokio::test]
    async fn peer_summary_fetch_merges_into_state() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut config = crate::config::Config::default();
        config.peers = vec![crate::config::PeerConfig {
            name: "anvil".into(),
            ..Default::default()
        }];
        let mut app = App::new(&config, true, None, api_rx, crate::api::EventHub::default());
        assert_eq!(app.state.peer_summaries.len(), 1);
        assert!(app.state.peer_summaries[0].is_stale());

        app.handle_internal_event(AppEvent::PeerSummaryFetched(
            crate::peers::PeerSummaryFetch {
                peer: "anvil".into(),
                result: Ok(crate::peers::PeerSummaryPayload {
                    host: "anvil-host".into(),
                    version: Some("0.6.8".into()),
                    protocol: Some(crate::protocol::PROTOCOL_VERSION),
                    system: Some(crate::api::schema::PeerSystemSummary {
                        cpu_percent: Some(71),
                        mem_used: Some(48_000_000_000),
                        mem_total: Some(64_000_000_000),
                        disk_free: Some(200_000_000_000),
                    }),
                    latency_ms: 34,
                    workspaces: vec![crate::api::schema::PeerWorkspaceSummary {
                        id: "ws_1".into(),
                        workspace: "flock".into(),
                        project_key: Some("github.com/gerchowl/flock".into()),
                        project_label: Some("flock".into()),
                        branch: Some("fix/pty".into()),
                        is_linked_worktree: true,
                        agent: Some("cc".into()),
                        status: crate::api::schema::AgentStatus::Blocked,
                        status_age_secs: Some(840),
                        activity: None,
                        agents: Vec::new(),
                    }],
                    relayed_fleet: Vec::new(),
                    icon: None,
                }),
            },
        ));
        let summary = &app.state.peer_summaries[0];
        assert_eq!(summary.host.as_deref(), Some("anvil-host"));
        assert_eq!(summary.version.as_deref(), Some("0.6.8"));
        assert_eq!(summary.latency_ms, Some(34));
        assert_eq!(
            summary.system.as_ref().and_then(|s| s.cpu_percent),
            Some(71)
        );
        assert_eq!(summary.workspaces.len(), 1);
        assert!(!summary.is_stale());
        assert!(summary.error.is_none());
        assert_eq!(summary.reachability(), crate::peers::PeerReachability::Live);

        // Errors keep the last good data but record the failure.
        app.handle_internal_event(AppEvent::PeerSummaryFetched(
            crate::peers::PeerSummaryFetch {
                peer: "anvil".into(),
                result: Err("ssh: connect timed out".into()),
            },
        ));
        let summary = &app.state.peer_summaries[0];
        assert_eq!(summary.workspaces.len(), 1);
        assert_eq!(summary.error.as_deref(), Some("ssh: connect timed out"));

        // Unknown peers (removed from config mid-flight) are ignored.
        app.handle_internal_event(AppEvent::PeerSummaryFetched(
            crate::peers::PeerSummaryFetch {
                peer: "ghost".into(),
                result: Err("nope".into()),
            },
        ));
        assert_eq!(app.state.peer_summaries.len(), 1);
    }

    #[tokio::test]
    async fn peer_summary_keeps_last_known_system_when_poll_omits_it() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut config = crate::config::Config::default();
        config.peers = vec![crate::config::PeerConfig {
            name: "anvil".into(),
            ..Default::default()
        }];
        let mut app = App::new(&config, true, None, api_rx, crate::api::EventHub::default());

        let workspaces = |n: usize| -> Vec<crate::api::schema::PeerWorkspaceSummary> {
            (0..n)
                .map(|i| crate::api::schema::PeerWorkspaceSummary {
                    id: format!("ws_{i}"),
                    workspace: format!("w{i}"),
                    project_key: None,
                    project_label: None,
                    branch: None,
                    is_linked_worktree: false,
                    agent: None,
                    status: crate::api::schema::AgentStatus::Idle,
                    status_age_secs: None,
                    activity: None,
                    agents: Vec::new(),
                })
                .collect()
        };
        let sys = |cpu: u8| {
            Some(crate::api::schema::PeerSystemSummary {
                cpu_percent: Some(cpu),
                mem_used: None,
                mem_total: None,
                disk_free: None,
            })
        };
        let fetch = |system: Option<crate::api::schema::PeerSystemSummary>,
                     ws: Vec<crate::api::schema::PeerWorkspaceSummary>| {
            AppEvent::PeerSummaryFetched(crate::peers::PeerSummaryFetch {
                peer: "anvil".into(),
                result: Ok(crate::peers::PeerSummaryPayload {
                    host: "anvil-host".into(),
                    version: None,
                    protocol: None,
                    system,
                    latency_ms: 10,
                    workspaces: ws,
                    relayed_fleet: Vec::new(),
                    icon: None,
                }),
            })
        };

        // First poll: full system + 1 workspace.
        app.handle_internal_event(fetch(sys(71), workspaces(1)));
        // Second poll omits the system block but reports 2 workspaces.
        app.handle_internal_event(fetch(None, workspaces(2)));
        let summary = &app.state.peer_summaries[0];
        // Health is retained from the last good poll (not blanked, #4)...
        assert_eq!(
            summary.system.as_ref().and_then(|s| s.cpu_percent),
            Some(71)
        );
        // ...while workspaces (empty IS meaningful) still track live.
        assert_eq!(summary.workspaces.len(), 2);

        // A poll that omits system AND has zero workspaces: system is still
        // retained, but workspaces clears (the guard is system-only).
        app.handle_internal_event(fetch(None, workspaces(0)));
        let summary = &app.state.peer_summaries[0];
        assert_eq!(
            summary.system.as_ref().and_then(|s| s.cpu_percent),
            Some(71)
        );
        assert!(summary.workspaces.is_empty());

        // A later poll WITH a system block updates it.
        app.handle_internal_event(fetch(sys(40), workspaces(1)));
        let summary = &app.state.peer_summaries[0];
        assert_eq!(
            summary.system.as_ref().and_then(|s| s.cpu_percent),
            Some(40)
        );
        assert_eq!(summary.workspaces.len(), 1);
    }

    #[tokio::test]
    async fn peers_summary_reports_workspace_project_and_status() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );

        let mut workspace = crate::workspace::Workspace::test_new("flock");
        workspace.cached_git_branch = Some("feat/peer-federation".into());
        workspace.cached_git_space = Some(crate::workspace::GitSpaceMetadata {
            key: "/repo/flock/.git".into(),
            checkout_key: "/repo/flock".into(),
            label: "flock".into(),
            repo_root: "/repo/flock".into(),
            is_linked_worktree: false,
            project_key: "github.com/gerchowl/flock".into(),
        });
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        let terminal_id = app.state.workspaces[0]
            .terminal_id(app.state.workspaces[0].tabs[0].root_pane)
            .cloned()
            .unwrap();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.detected_agent = Some(Agent::Claude);
        terminal.state = AgentState::Blocked;
        terminal.state_changed_at =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(90));

        let response = app.handle_peers_summary("req_peers".into());
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        let result = &value["result"];
        assert!(result["host"].as_str().is_some_and(|h| !h.is_empty()));
        let summary = &result["workspaces"][0];
        assert_eq!(summary["workspace"], "flock");
        assert_eq!(summary["project_key"], "github.com/gerchowl/flock");
        assert_eq!(summary["project_label"], "flock");
        assert_eq!(summary["branch"], "feat/peer-federation");
        assert_eq!(summary["status"], "blocked");
        assert_eq!(summary["agent"], "cc");
        assert!(summary["status_age_secs"].as_u64().unwrap() >= 90);
    }

    #[tokio::test]
    async fn flock_toast_context_uses_live_root_runtime_cwd_label() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );

        let mut workspace = crate::workspace::Workspace::test_new("stale");
        workspace.custom_name = None;
        let root = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(root).cloned().unwrap();
        let temp_root = std::env::temp_dir().join(format!(
            "flock-toast-context-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let stale_cwd = temp_root.join("__flock_original__");
        let live_cwd = temp_root.join("__flock_projects__");
        std::fs::create_dir_all(&stale_cwd).unwrap();
        std::fs::create_dir_all(&live_cwd).unwrap();
        init_repo(&stale_cwd);
        init_repo(&live_cwd);

        workspace.identity_cwd = stale_cwd.clone();
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.terminals.get_mut(&terminal_id).unwrap().cwd = stale_cwd;
        app.state.active = None;
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        app.state.config.ui.toast.delivery = crate::config::ToastDelivery::Flock;

        let (events, _) = tokio::sync::mpsc::channel(4);
        let runtime = crate::terminal::TerminalRuntime::spawn(
            root,
            24,
            80,
            live_cwd.clone(),
            0,
            crate::terminal_theme::TerminalTheme::default(),
            crate::pane::PaneShellConfig::new("/bin/sh", crate::config::ShellModeConfig::NonLogin),
            events,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while runtime.cwd() != Some(live_cwd.clone()) && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        app.terminal_runtimes.insert(terminal_id.clone(), runtime);

        app.handle_internal_event(AppEvent::StateChanged {
            pane_id: root,
            agent: Some(Agent::Codex),
            state: AgentState::Working,
            activity: None,
            visible_blocker: false,
            visible_idle: false,
            visible_working: false,
            process_exited: false,
            observed_at: std::time::Instant::now(),
        });
        app.handle_internal_event(AppEvent::StateChanged {
            pane_id: root,
            agent: Some(Agent::Codex),
            state: AgentState::Idle,
            activity: None,
            visible_blocker: false,
            visible_idle: false,
            visible_working: false,
            process_exited: false,
            observed_at: std::time::Instant::now(),
        });

        // #130: a background completion's toast is deferred to the settle commit.
        let changed_at = app
            .state
            .terminals
            .get(&terminal_id)
            .unwrap()
            .state_changed_at
            .unwrap();
        app.state.commit_settled_completions(
            changed_at
                + crate::app::actions::ATTENTION_SETTLE
                + std::time::Duration::from_millis(1),
            &app.terminal_runtimes,
        );

        assert_eq!(
            app.state.toast.as_ref().map(|toast| toast.context.as_str()),
            Some("__flock_projects__ · 1")
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
        let _ = std::fs::remove_dir_all(temp_root);
    }

    /// #130 regression: a background completion's `Done` (Idle && !seen) status
    /// is deferred with the `●`, so the settle commit — not the edge — is the
    /// first moment the pane reaches `Done`. The commit must broadcast it, or an
    /// API subscriber (`wait agent-status --status done`) blocks until timeout.
    #[tokio::test]
    async fn deferred_background_completion_broadcasts_done_at_settle() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );

        let workspace = crate::workspace::Workspace::test_new("bg");
        let root = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(root).cloned().unwrap();
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        // Unattended (no active tab): the completion is a *background* completion
        // and so is deferred by #130.
        app.state.active = None;
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;

        for state in [AgentState::Working, AgentState::Idle] {
            app.handle_internal_event(AppEvent::StateChanged {
                pane_id: root,
                agent: Some(Agent::Codex),
                state,
                activity: None,
                visible_blocker: false,
                visible_idle: false,
                visible_working: false,
                process_exited: false,
                observed_at: std::time::Instant::now(),
            });
        }

        let reports_done = |app: &App| {
            app.event_hub.events_after(0).iter().any(|(_, event)| {
                matches!(
                    &event.data,
                    crate::api::schema::EventData::PaneAgentStatusChanged { agent_status, .. }
                        if *agent_status == crate::api::schema::AgentStatus::Done
                )
            })
        };
        // At the deferring edge the pane is Idle but `seen` is still true → it
        // reports `Idle`, never `Done`.
        assert!(
            !reports_done(&app),
            "the deferred edge must not report Done yet"
        );

        // Settle: commit + broadcast, exactly as the runtime/headless tick does.
        let changed_at = app
            .state
            .terminals
            .get(&terminal_id)
            .unwrap()
            .state_changed_at
            .unwrap();
        let settled = app.state.commit_settled_completions(
            changed_at
                + crate::app::actions::ATTENTION_SETTLE
                + std::time::Duration::from_millis(1),
            &app.terminal_runtimes,
        );
        for update in &settled {
            app.emit_pane_state_update(update);
        }

        assert!(
            reports_done(&app),
            "the settled completion must broadcast Done so `wait --status done` unblocks"
        );
    }

    #[tokio::test]
    async fn pane_died_respawns_shell_and_clears_restored_agent_session() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist");
        terminal.respawn_shell_on_exit = true;
        terminal.set_agent_name("codex".into());
        terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
            source: "flock:codex".into(),
            agent: "codex".into(),
            session_ref: crate::agent_resume::AgentSessionRef::id("codex-session")
                .expect("test session id should be valid"),
        });

        app.handle_internal_event(AppEvent::PaneDied { pane_id });

        assert!(
            app.find_pane(pane_id).is_some(),
            "respawnable agent pane should stay attached after the agent process exits"
        );
        let terminal = app
            .state
            .terminals
            .get(&terminal_id)
            .expect("terminal should survive respawn");
        assert!(!terminal.respawn_shell_on_exit);
        assert!(terminal.persisted_agent_session.is_none());
        assert!(terminal.agent_name.is_none());

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[test]
    fn terminal_delivery_does_not_refresh_existing_targeted_toast() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.local_terminal_notifications = false;

        let mut workspace = crate::workspace::Workspace::test_new("stale");
        workspace.custom_name = None;
        workspace.identity_cwd = "/__flock_original__".into();
        let root = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(root).cloned().unwrap();
        let workspace_id = workspace.id.clone();
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.terminals.get_mut(&terminal_id).unwrap().cwd = "/__flock_projects__".into();
        app.state.active = None;
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        app.state.config.ui.toast.delivery = crate::config::ToastDelivery::Terminal;

        app.handle_internal_event(AppEvent::StateChanged {
            pane_id: root,
            agent: Some(Agent::Codex),
            state: AgentState::Working,
            activity: None,
            visible_blocker: false,
            visible_idle: false,
            visible_working: false,
            process_exited: false,
            observed_at: std::time::Instant::now(),
        });
        app.state.toast = Some(crate::app::state::ToastNotification {
            kind: ToastKind::Finished,
            title: "codex finished".into(),
            context: "__flock_original__ · 1".into(),
            position: None,
            target: Some(crate::app::state::ToastTarget {
                workspace_id,
                pane_id: root,
            }),
        });

        app.handle_internal_event(AppEvent::StateChanged {
            pane_id: root,
            agent: Some(Agent::Codex),
            state: AgentState::Idle,
            activity: None,
            visible_blocker: false,
            visible_idle: false,
            visible_working: false,
            process_exited: false,
            observed_at: std::time::Instant::now(),
        });

        assert_eq!(
            app.state.toast.as_ref().map(|toast| toast.context.as_str()),
            Some("__flock_original__ · 1")
        );
    }
    #[tokio::test]
    async fn pr_states_updated_applies_to_matching_workspace() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("wt")];
        let id = app.state.workspaces[0].id.clone();

        app.handle_internal_event(crate::events::AppEvent::PrStatesUpdated(Ok(vec![(
            id,
            Some(crate::worktree::PrStateInfo {
                state: crate::worktree::PrState::Merged,
                number: 6,
            }),
        )])));

        assert_eq!(
            app.state.workspaces[0].pr_state(),
            Some(crate::worktree::PrStateInfo {
                state: crate::worktree::PrState::Merged,
                number: 6,
            })
        );
    }

    /// The 120s PR poll covers ALL workspaces with a branch (#42): linked
    /// worktrees, the primary checkout of a space, and standalone repos —
    /// deduplicated to one gh call per (repo, branch).
    #[test]
    fn pr_poll_targets_cover_linked_primary_and_standalone_checkouts() {
        use crate::workspace::{Workspace, WorktreeSpaceMembership};
        let membership = |idx: usize, linked: bool, root: &str| WorktreeSpaceMembership {
            key: root.into(),
            label: "flock".into(),
            repo_root: root.into(),
            checkout_path: format!("{root}/ws-{idx}").into(),
            is_linked_worktree: linked,
        };

        let mut state = crate::app::state::AppState::test_new();
        // Linked worktree member.
        let mut linked = Workspace::test_new("linked");
        linked.worktree_space = Some(membership(1, true, "/repo/flock"));
        linked.cached_git_branch = Some("feat/a".into());
        // Primary (non-linked) checkout of the same space.
        let mut primary = Workspace::test_new("primary");
        primary.worktree_space = Some(membership(0, false, "/repo/flock"));
        primary.cached_git_branch = Some("master".into());
        // Standalone repo: no worktree space, only live git metadata.
        let mut standalone = Workspace::test_new("solo");
        standalone.cached_git_space = Some(crate::workspace::GitSpaceMetadata {
            key: "/repo/solo/.git".into(),
            checkout_key: "/repo/solo".into(),
            label: "solo".into(),
            repo_root: "/repo/solo".into(),
            is_linked_worktree: false,
            project_key: "github.com/g/solo".into(),
        });
        standalone.cached_git_branch = Some("main".into());
        // Second workspace on the SAME standalone repo+branch: dedupes into
        // the standalone target instead of a second gh call.
        let mut duplicate = Workspace::test_new("solo-2");
        duplicate.cached_git_space = standalone.cached_git_space.clone();
        duplicate.cached_git_branch = Some("main".into());
        // No branch -> never polled.
        let branchless = Workspace::test_new("scratch");

        let ids: Vec<String> = [&linked, &primary, &standalone, &duplicate]
            .map(|ws| ws.id.clone())
            .to_vec();
        state.workspaces = vec![linked, primary, standalone, duplicate, branchless];

        let targets = pr_poll_targets(&state);

        assert_eq!(targets.len(), 3, "{targets:?}");
        assert_eq!(targets[0].workspace_ids, vec![ids[0].clone()]);
        assert_eq!(targets[0].branch, "feat/a");
        assert_eq!(
            targets[0].checkout,
            std::path::PathBuf::from("/repo/flock/ws-1")
        );
        assert_eq!(targets[1].workspace_ids, vec![ids[1].clone()]);
        assert_eq!(targets[1].branch, "master");
        // The standalone pair shares one query; both ids ride its result.
        assert_eq!(
            targets[2].workspace_ids,
            vec![ids[2].clone(), ids[3].clone()]
        );
        assert_eq!(targets[2].repo_root, std::path::PathBuf::from("/repo/solo"));
        assert_eq!(targets[2].checkout, std::path::PathBuf::from("/repo/solo"));
    }

    // --- Health surface (#295): the whole point is that these four questions
    // become answerable from OUTSIDE the process. Every case below is one an
    // operator has to distinguish that today's code cannot.

    fn peers_summary_pollers(app: &mut App) -> serde_json::Value {
        let response = app.handle_peers_summary("req_health".into());
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        value["result"]["pollers"].clone()
    }

    #[tokio::test]
    async fn peers_summary_carries_all_four_poller_health_rows() {
        // A default config has checks enabled but no peers, so peer_poll
        // must be `None` and the other three present. The whole point of the
        // PR: "did that box's git refresh stop?" now has an answer that a
        // fleet watchdog can poll — every present row is a source it can
        // alert on independently.
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let config = crate::config::Config::default();
        let mut app = App::new(&config, true, None, api_rx, crate::api::EventHub::default());

        let pollers = peers_summary_pollers(&mut app);
        assert!(
            pollers["pr"].is_object(),
            "the pre-existing pr row must still be present: {pollers}"
        );
        assert!(
            pollers["git_refresh"].is_object(),
            "the sidebar-driving refresh is the highest-value new row: {pollers}"
        );
        assert!(
            pollers["checks_runner"].is_object(),
            "checks enabled by default -> a runner health row: {pollers}"
        );
        assert!(
            pollers["peer_poll"].is_null(),
            "no peers configured -> peer_poll absent (not `broken` for a job it doesn't have): {pollers}"
        );
    }

    #[tokio::test]
    async fn a_never_succeeded_git_refresh_reads_broken_not_ok() {
        // The empty-poller-must-not-render-healthy rule (#300). A brand-new
        // server hasn't refreshed yet — the answer to "when did it last
        // succeed?" is unknown, and the status is Broken.
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let config = crate::config::Config::default();
        let mut app = App::new(&config, true, None, api_rx, crate::api::EventHub::default());

        let pollers = peers_summary_pollers(&mut app);
        assert_eq!(
            pollers["git_refresh"]["status"], "broken",
            "a poller that has never succeeded must not render as healthy: {pollers}"
        );
        assert!(
            pollers["git_refresh"]["last_success_age_secs"].is_null(),
            "never-succeeded MUST NOT be reported as `0 seconds ago`: {pollers}"
        );
    }

    #[tokio::test]
    async fn a_wedged_git_refresh_shows_a_rising_in_flight_age() {
        // The signal an operator alerts on: `in_flight_age_secs` climbing
        // toward the round ceiling. Simulate a refresh that never
        // completed and confirm the snapshot exposes the growing age.
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let config = crate::config::Config::default();
        let mut app = App::new(&config, true, None, api_rx, crate::api::EventHub::default());

        let start = std::time::Instant::now() - std::time::Duration::from_secs(30);
        app.git_refresh_health.mark_started(start);

        let pollers = peers_summary_pollers(&mut app);
        assert_eq!(pollers["git_refresh"]["in_flight"], true);
        let age = pollers["git_refresh"]["in_flight_age_secs"]
            .as_u64()
            .unwrap();
        assert!(
            (30..=35).contains(&age),
            "in-flight age must reflect elapsed time since dispatch, got {age}"
        );
    }

    #[tokio::test]
    async fn a_git_refresh_failure_streak_degrades_inside_the_freshness_window() {
        // Recent success but a run of failures since — the poller looks
        // fine on age alone right up until it silently crosses the
        // staleness line. The health surface must degrade earlier.
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let config = crate::config::Config::default();
        let mut app = App::new(&config, true, None, api_rx, crate::api::EventHub::default());

        let now = std::time::Instant::now();
        app.git_refresh_health
            .mark_success(now - std::time::Duration::from_secs(5));
        for _ in 0..crate::health::DEGRADED_FAILURE_STREAK {
            app.git_refresh_health
                .mark_failure(now, crate::health::GitRefreshErrorKind::Timeout);
        }

        let pollers = peers_summary_pollers(&mut app);
        assert_eq!(
            pollers["git_refresh"]["status"], "degraded",
            "a failure streak past the threshold must degrade even with a fresh success: {pollers}"
        );
        assert_eq!(
            pollers["git_refresh"]["last_error"], "timeout",
            "the classified error kind is on the wire, never a free-text message: {pollers}"
        );
    }

    #[tokio::test]
    async fn checks_runner_health_is_absent_when_checks_are_disabled() {
        // A `[checks] enable = false` server has no runner tick to have
        // health for. Reporting Broken would misdirect a watchdog.
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut config = crate::config::Config::default();
        config.checks.enable = false;
        let mut app = App::new(&config, true, None, api_rx, crate::api::EventHub::default());

        let pollers = peers_summary_pollers(&mut app);
        assert!(
            pollers["checks_runner"].is_null(),
            "checks disabled -> no runner health row: {pollers}"
        );
    }

    #[tokio::test]
    async fn checks_runner_health_stamps_on_the_tick_including_during_pause() {
        // The runner's health question is liveness — whether the loop's
        // checks branch is still waking. A paused fleet still ticks and
        // early-returns; stamping BEFORE the pause guard proves "paused"
        // and "loop stopped" are distinct.
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let config = crate::config::Config::default();
        let mut app = App::new(&config, true, None, api_rx, crate::api::EventHub::default());

        // Pause the fleet — dispatch_due_script_checks will early-return.
        app.fleet_pause.paused = true;

        // One tick.
        app.dispatch_due_script_checks(std::time::Instant::now());

        let pollers = peers_summary_pollers(&mut app);
        assert_eq!(
            pollers["checks_runner"]["status"], "ok",
            "the tick fired (even while paused), so the runner is alive: {pollers}"
        );
        assert!(
            pollers["checks_runner"]["last_success_age_secs"]
                .as_u64()
                .is_some_and(|age| age <= 1),
            "the last-success stamp came from the pause-guarded tick: {pollers}"
        );
    }

    #[tokio::test]
    async fn peer_poll_health_reflects_a_successful_fetch_and_classified_errors() {
        // Both wire-safety and liveness: a fetch that came back Ok makes
        // the aggregate healthy; a fetch that came back Err makes it
        // degraded with a CLOSED enum kind on the wire — not the raw ssh
        // error text, which routinely names hostnames and paths (#300).
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut config = crate::config::Config::default();
        config.peers = vec![crate::config::PeerConfig {
            name: "anvil".into(),
            ..Default::default()
        }];
        let mut app = App::new(&config, true, None, api_rx, crate::api::EventHub::default());

        // Freshest peer-poll answer is a success.
        app.handle_internal_event(AppEvent::PeerSummaryFetched(
            crate::peers::PeerSummaryFetch {
                peer: "anvil".into(),
                result: Ok(crate::peers::PeerSummaryPayload {
                    host: "anvil-host".into(),
                    version: Some("0.6.8".into()),
                    protocol: Some(crate::protocol::PROTOCOL_VERSION),
                    system: None,
                    latency_ms: 12,
                    workspaces: Vec::new(),
                    relayed_fleet: Vec::new(),
                    icon: None,
                }),
            },
        ));

        let pollers = peers_summary_pollers(&mut app);
        assert_eq!(pollers["peer_poll"]["status"], "ok", "{pollers}");

        // Now a leaky-looking transport failure — the message would name
        // the host if it crossed the wire, which is the #300 disclosure
        // bug. The classification is what ships.
        //
        // Composed at runtime so gitleaks's pre-commit hook can't mistake
        // it for a fixture credential.
        let leaky = format!(
            "ssh: connect to host {}.internal.{} port 22: Connection refused",
            "prod-01", "example.com"
        );
        app.handle_internal_event(AppEvent::PeerSummaryFetched(
            crate::peers::PeerSummaryFetch {
                peer: "anvil".into(),
                result: Err(leaky.clone()),
            },
        ));

        let pollers = peers_summary_pollers(&mut app);
        assert_eq!(
            pollers["peer_poll"]["last_error"], "transport",
            "the wire carries the closed-enum kind, never the message: {pollers}"
        );
        let last_error_string = pollers["peer_poll"]["last_error"].as_str().unwrap();
        assert!(
            !last_error_string.contains("prod-01") && !last_error_string.contains("example"),
            "the wire must not carry the free-text hostname: {pollers}"
        );
    }

    #[tokio::test]
    async fn peer_poll_in_flight_age_projects_the_oldest_peer_across_the_fleet() {
        // The aggregate wedge signal: a single peer whose SSH is stuck for
        // 45s must dominate the reported `in_flight_age_secs`, even if
        // other peers are ticking fine. Otherwise "one peer wedged"
        // averages out into "everything looks fresh".
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut config = crate::config::Config::default();
        config.peers = vec![
            crate::config::PeerConfig {
                name: "anvil".into(),
                ..Default::default()
            },
            crate::config::PeerConfig {
                name: "sage".into(),
                ..Default::default()
            },
        ];
        let mut app = App::new(&config, true, None, api_rx, crate::api::EventHub::default());

        // Pin two peers to controlled in-flight instants — sage 5s in
        // (fresh), anvil 45s in (wedging). The `should_poll_now` public API
        // arms next_due off the passed instant, which would interact with
        // the aggregate age we're trying to assert; the test seam sets the
        // one field this projection reads.
        let now = std::time::Instant::now();
        app.peer_poll_tracker
            .set_in_flight_since_for_test("sage", now - std::time::Duration::from_secs(5));
        app.peer_poll_tracker
            .set_in_flight_since_for_test("anvil", now - std::time::Duration::from_secs(45));

        let pollers = peers_summary_pollers(&mut app);
        assert_eq!(pollers["peer_poll"]["in_flight"], true);
        let age = pollers["peer_poll"]["in_flight_age_secs"].as_u64().unwrap();
        assert!(
            age >= 45,
            "the OLDEST in-flight must dominate the aggregate (got {age}): {pollers}"
        );
    }
}
