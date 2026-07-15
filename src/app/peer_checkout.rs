//! Cross-machine checkout (#125): "check out a peer's branch here".
//!
//! The hub never reaches into a peer's git. It asks the peer's own flock — over
//! the same SSH-invoked verb surface as `peers.summary` — to prepare the branch
//! (`peers.checkout_prepare`: probe, then push to origin). Only then does the
//! hub touch its OWN git: `git fetch origin <branch>` into a local checkout of
//! the same project, add a linked worktree, and open it. Hub-spoke throughout.
//!
//! The flow is a chained async state machine, each leg a worker thread that
//! reports back via an [`AppEvent`]:
//!   request → probe (push=false) → CONFIRM → push (push=true) → fetch+add → open

use std::sync::atomic::Ordering;

use crate::app::state::{Mode, PeerCheckoutState};
use crate::events::AppEvent;

impl crate::app::App {
    /// Consume a deferred "Check out here" request (peer_idx, ws_idx): resolve
    /// the remote target and a LOCAL checkout of the same project, then kick off
    /// the read-only probe. Fails fast (action notice) when the row no longer
    /// resolves or the hub has no local checkout of that project.
    pub(crate) fn begin_peer_checkout(&mut self, peer_idx: usize, ws_idx: usize) {
        let Some(summary) = self.state.peer_summaries.get(peer_idx) else {
            return;
        };
        let Some(peer) = self.state.peers.get(peer_idx).cloned() else {
            return;
        };
        let host = summary.display_name().to_string();
        let Some(remote) = summary.workspaces.get(ws_idx) else {
            return;
        };
        let remote_workspace_id = remote.id.clone();
        if remote_workspace_id.is_empty() {
            self.state.action_notice = Some("peer workspace has no id to check out".to_string());
            return;
        }
        let Some(branch) = remote.branch.clone() else {
            self.state.action_notice = Some(format!(
                "{host}:{} has no branch to check out",
                remote.workspace
            ));
            return;
        };
        let Some(project_key) = remote.project_key.clone() else {
            self.state.action_notice = Some(format!(
                "{host}:{} has no project identity",
                remote.workspace
            ));
            return;
        };

        // The hub must already have a checkout of this project to add a worktree
        // to — match by machine-independent project identity.
        let Some(local) = self.resolve_local_project_target(&project_key) else {
            self.state.action_notice = Some(format!(
                "no local checkout of '{}' to check out into",
                remote.project_label.clone().unwrap_or(project_key)
            ));
            return;
        };

        self.state.peer_checkout_seq += 1;
        self.state.peer_checkout = Some(PeerCheckoutState {
            generation: self.state.peer_checkout_seq,
            peer,
            host,
            remote_workspace_id,
            branch,
            source_repo_root: local.repo_root,
            source_checkout_path: local.checkout_path,
            source_workspace_id: local.workspace_id,
            repo_key: local.key,
            repo_name: local.label,
            report: None,
            busy: true,
            error: None,
        });
        self.spawn_peer_checkout_leg(false, |generation, result| AppEvent::PeerCheckoutProbed {
            generation,
            result,
        });
    }

    /// A local workspace's repo identity matching `project_key`, preferring the
    /// main checkout (non-linked) so the worktree is added to the repo root.
    fn resolve_local_project_target(&self, project_key: &str) -> Option<LocalProjectTarget> {
        // Prefer a non-linked (main) checkout; fall back to any matching row.
        let mut fallback: Option<LocalProjectTarget> = None;
        for ws in &self.state.workspaces {
            let space = ws.git_space();
            let matches = space.map(|s| s.project_key.as_str()) == Some(project_key);
            if !matches {
                continue;
            }
            let Some(checkout_path) = ws.resolved_identity_cwd() else {
                continue;
            };
            let (repo_root, key, label, is_linked) = match (space, ws.worktree_space()) {
                (Some(s), _) => (
                    s.repo_root.clone(),
                    s.key.clone(),
                    s.label.clone(),
                    s.is_linked_worktree,
                ),
                (None, Some(m)) => (
                    m.repo_root.clone(),
                    m.key.clone(),
                    m.label.clone(),
                    m.is_linked_worktree,
                ),
                (None, None) => continue,
            };
            let target = LocalProjectTarget {
                repo_root,
                checkout_path,
                workspace_id: ws.id.clone(),
                key,
                label,
            };
            if !is_linked {
                return Some(target);
            }
            fallback.get_or_insert(target);
        }
        fallback
    }

    /// Spawn one prepare leg over SSH (probe or push), reporting back via
    /// `wrap` (`PeerCheckoutProbed` / `PeerCheckoutPushed`) stamped with the
    /// checkout's generation so a stale return is discarded.
    fn spawn_peer_checkout_leg(
        &self,
        push: bool,
        wrap: fn(u64, Result<crate::peers::PeerCheckoutOutcome, String>) -> AppEvent,
    ) {
        let Some(checkout) = self.state.peer_checkout.as_ref() else {
            return;
        };
        let generation = checkout.generation;
        let peer = checkout.peer.clone();
        let workspace_id = checkout.remote_workspace_id.clone();
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let result = crate::peers::run_checkout_prepare_command(&peer, &workspace_id, push);
            let _ = event_tx.blocking_send(wrap(generation, result));
        });
    }

    /// The current checkout iff it matches `generation` — used by each leg's
    /// handler to drop a stale return (cancelled or superseded mid-flight).
    fn peer_checkout_for(&mut self, generation: u64) -> Option<&mut PeerCheckoutState> {
        self.state
            .peer_checkout
            .as_mut()
            .filter(|checkout| checkout.generation == generation)
    }

    /// Probe (`push=false`) returned: open the confirm dialog with the peer's
    /// dirty / unpushed warnings, or surface the failure and abort. A stale
    /// generation (cancelled / superseded mid-flight) is dropped.
    pub(crate) fn handle_peer_checkout_probed(
        &mut self,
        generation: u64,
        result: Result<crate::peers::PeerCheckoutOutcome, String>,
    ) {
        let Some(checkout) = self.peer_checkout_for(generation) else {
            return;
        };
        match result {
            Ok(outcome) => {
                checkout.branch = outcome.branch.clone();
                checkout.report = Some(outcome);
                checkout.busy = false;
                checkout.error = None;
                self.state.mode = Mode::ConfirmCrossCheckout;
            }
            Err(err) => {
                let host = checkout.host.clone();
                self.state.peer_checkout = None;
                self.state.action_notice = Some(format!("checkout probe failed ({host}): {err}"));
            }
        }
        self.render_dirty.store(true, Ordering::Release);
        self.render_notify.notify_one();
    }

    /// User confirmed the dialog: push the branch on the peer (`push=true`).
    pub(crate) fn confirm_peer_checkout(&mut self) {
        let Some(checkout) = self.state.peer_checkout.as_mut() else {
            return;
        };
        if checkout.busy {
            return;
        }
        checkout.busy = true;
        checkout.error = None;
        self.spawn_peer_checkout_leg(true, |generation, result| AppEvent::PeerCheckoutPushed {
            generation,
            result,
        });
        self.render_dirty.store(true, Ordering::Release);
        self.render_notify.notify_one();
    }

    /// Cancel the dialog (Esc): drop the in-flight checkout. Any leg already
    /// running just lands on `None` and is ignored.
    pub(crate) fn cancel_peer_checkout(&mut self) {
        self.state.peer_checkout = None;
        self.state.mode = if self.state.active.is_some() {
            Mode::Terminal
        } else {
            Mode::Navigate
        };
        self.render_dirty.store(true, Ordering::Release);
        self.render_notify.notify_one();
    }

    /// Push (`push=true`) returned: on success the branch is on origin, so kick
    /// off the LOCAL fetch + worktree add. On failure keep the dialog open with
    /// the error so the user can retry or cancel.
    pub(crate) fn handle_peer_checkout_pushed(
        &mut self,
        generation: u64,
        result: Result<crate::peers::PeerCheckoutOutcome, String>,
    ) {
        let Some(checkout) = self.peer_checkout_for(generation) else {
            return;
        };
        match result {
            Ok(outcome) => {
                checkout.branch = outcome.branch.clone();
                checkout.report = Some(outcome);
                // Stay busy: the local fetch + add leg runs next.
                let repo_dir = checkout.source_checkout_path.clone();
                let repo_name = checkout.repo_name.clone();
                let branch = checkout.branch.clone();
                let worktree_dir = self.state.worktree_directory.clone();
                let event_tx = self.event_tx.clone();
                std::thread::spawn(move || {
                    let result = crate::worktree::fetch_and_add_peer_worktree(
                        &repo_dir,
                        &worktree_dir,
                        &repo_name,
                        &branch,
                    );
                    let _ = event_tx
                        .blocking_send(AppEvent::PeerCheckoutWorktreeReady { generation, result });
                });
            }
            Err(err) => {
                checkout.busy = false;
                checkout.error = Some(err);
            }
        }
        self.render_dirty.store(true, Ordering::Release);
        self.render_notify.notify_one();
    }

    /// Local fetch + worktree add returned: on success open the worktree as a
    /// workspace and stamp membership (mirrors the new-worktree open tail). On
    /// failure keep the dialog open with the error.
    pub(crate) fn handle_peer_checkout_worktree_ready(
        &mut self,
        generation: u64,
        result: Result<std::path::PathBuf, String>,
    ) {
        let Some(checkout) = self
            .state
            .peer_checkout
            .as_ref()
            .filter(|checkout| checkout.generation == generation)
        else {
            return;
        };
        match result {
            Ok(path) => {
                let repo_key = checkout.repo_key.clone();
                let repo_name = checkout.repo_name.clone();
                let source_repo_root = checkout.source_repo_root.clone();
                let source_checkout_path = checkout.source_checkout_path.clone();
                let source_workspace_id = checkout.source_workspace_id.clone();
                match self.create_workspace_with_options(path.clone(), true) {
                    Ok(ws_idx) => {
                        // Stamp the local source row as the non-linked anchor...
                        if let Some(ws) = self
                            .state
                            .workspaces
                            .iter_mut()
                            .find(|ws| ws.id == source_workspace_id)
                        {
                            ws.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
                                key: repo_key.clone(),
                                label: repo_name.clone(),
                                repo_root: source_repo_root.clone(),
                                checkout_path: source_checkout_path,
                                is_linked_worktree: false,
                            });
                        }
                        // ...and the new checkout as a linked worktree member.
                        if let Some(ws) = self.state.workspaces.get_mut(ws_idx) {
                            ws.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
                                key: repo_key,
                                label: repo_name,
                                repo_root: source_repo_root,
                                checkout_path: path,
                                is_linked_worktree: true,
                            });
                        }
                        self.state.peer_checkout = None;
                        self.state.mode = Mode::Terminal;
                        self.state.mark_session_dirty();
                    }
                    Err(err) => {
                        if let Some(checkout) = self.peer_checkout_for(generation) {
                            checkout.busy = false;
                            checkout.error =
                                Some(format!("added worktree but failed to open: {err}"));
                        }
                    }
                }
            }
            Err(err) => {
                if let Some(checkout) = self.peer_checkout_for(generation) {
                    checkout.busy = false;
                    checkout.error = Some(err);
                }
            }
        }
        self.render_dirty.store(true, Ordering::Release);
        self.render_notify.notify_one();
    }
}

/// A local checkout of the project the hub will add the worktree to.
struct LocalProjectTarget {
    repo_root: std::path::PathBuf,
    checkout_path: std::path::PathBuf,
    workspace_id: String,
    key: String,
    label: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{AgentStatus, PeerWorkspaceSummary};
    use crate::app::App;
    use crate::peers::{PeerCheckoutOutcome, PeerSummaryState};

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

    fn peer_with_workspace(project_key: Option<&str>, branch: Option<&str>) -> PeerSummaryState {
        PeerSummaryState {
            peer: "anvil".into(),
            ssh_target: "lars@anvil".into(),
            host: Some("anvil".into()),
            version: None,
            protocol: None,
            system: None,
            latency_ms: Some(10),
            workspaces: vec![PeerWorkspaceSummary {
                id: "ws_3".into(),
                workspace: "proj".into(),
                project_key: project_key.map(str::to_string),
                project_label: Some("proj".into()),
                branch: branch.map(str::to_string),
                is_linked_worktree: false,
                agent: None,
                status: AgentStatus::Working,
                status_age_secs: None,
                activity: None,
            }],
            last_ok: Some(std::time::Instant::now()),
            error: None,
            origin_last_ok_secs: None,
            proxy_jump: None,
            icon: None,
        }
    }

    fn sample_state() -> PeerCheckoutState {
        PeerCheckoutState {
            generation: 1,
            peer: crate::config::PeerConfig {
                name: "anvil".into(),
                ..Default::default()
            },
            host: "anvil".into(),
            remote_workspace_id: "ws_3".into(),
            branch: "feature-x".into(),
            source_repo_root: "/repo".into(),
            source_checkout_path: "/repo".into(),
            source_workspace_id: "w_local".into(),
            repo_key: "/repo/.git".into(),
            repo_name: "proj".into(),
            report: None,
            busy: true,
            error: None,
        }
    }

    #[tokio::test]
    async fn begin_peer_checkout_without_local_checkout_notices_and_does_not_spawn() {
        let mut app = test_app();
        app.state.peers = vec![crate::config::PeerConfig {
            name: "anvil".into(),
            ..Default::default()
        }];
        app.state.peer_summaries = vec![peer_with_workspace(
            Some("github.com/x/proj"),
            Some("feature-x"),
        )];

        // No local workspace matches the project — fail fast, no in-flight state.
        app.begin_peer_checkout(0, 0);
        assert!(app.state.peer_checkout.is_none());
        assert!(app
            .state
            .action_notice
            .as_deref()
            .unwrap()
            .contains("no local checkout"));
    }

    #[tokio::test]
    async fn begin_peer_checkout_without_branch_notices() {
        let mut app = test_app();
        app.state.peers = vec![crate::config::PeerConfig {
            name: "anvil".into(),
            ..Default::default()
        }];
        app.state.peer_summaries = vec![peer_with_workspace(Some("github.com/x/proj"), None)];

        app.begin_peer_checkout(0, 0);
        assert!(app.state.peer_checkout.is_none());
        assert!(app
            .state
            .action_notice
            .as_deref()
            .unwrap()
            .contains("no branch"));
    }

    #[tokio::test]
    async fn probe_ok_opens_confirm_with_report() {
        let mut app = test_app();
        app.state.peer_checkout = Some(sample_state());

        app.handle_peer_checkout_probed(
            1,
            Ok(PeerCheckoutOutcome {
                branch: "feature-x".into(),
                was_dirty: true,
                was_unpushed: true,
                pushed: false,
            }),
        );

        assert_eq!(app.state.mode, Mode::ConfirmCrossCheckout);
        let checkout = app.state.peer_checkout.as_ref().unwrap();
        assert!(!checkout.busy);
        assert!(checkout.report.as_ref().unwrap().was_dirty);
    }

    #[tokio::test]
    async fn stale_probe_is_dropped_and_does_not_corrupt_a_new_checkout() {
        let mut app = test_app();
        // A NEW checkout (generation 2) is in flight...
        let mut current = sample_state();
        current.generation = 2;
        current.branch = "current-branch".into();
        app.state.peer_checkout = Some(current);

        // ...when a STALE leg from a cancelled checkout (generation 1) returns.
        app.handle_peer_checkout_probed(
            1,
            Ok(PeerCheckoutOutcome {
                branch: "stale-branch".into(),
                was_dirty: true,
                was_unpushed: true,
                pushed: false,
            }),
        );

        // The stale leg is discarded: the new checkout is untouched, no dialog.
        assert_ne!(app.state.mode, Mode::ConfirmCrossCheckout);
        let checkout = app.state.peer_checkout.as_ref().unwrap();
        assert_eq!(checkout.branch, "current-branch");
        assert!(checkout.report.is_none());
    }

    #[tokio::test]
    async fn probe_error_aborts_with_notice() {
        let mut app = test_app();
        app.state.peer_checkout = Some(sample_state());

        app.handle_peer_checkout_probed(1, Err("ssh down".into()));

        assert!(app.state.peer_checkout.is_none());
        assert_ne!(app.state.mode, Mode::ConfirmCrossCheckout);
        assert!(app
            .state
            .action_notice
            .as_deref()
            .unwrap()
            .contains("ssh down"));
    }

    #[tokio::test]
    async fn cancel_drops_in_flight_checkout() {
        let mut app = test_app();
        app.state.peer_checkout = Some(sample_state());
        app.state.mode = Mode::ConfirmCrossCheckout;

        app.cancel_peer_checkout();
        assert!(app.state.peer_checkout.is_none());
        assert_eq!(app.state.mode, Mode::Navigate);
    }
}
