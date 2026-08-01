//! Scheduled reap built-in (#175 phase 5, S2).
//!
//! Pure fold from `(now, config, verdict, workspace snapshots)` to a list
//! of reap candidates. The App tick performs the dispatch (Quarantine /
//! CheckoutOnly / KillBranch / ClosePane) so the fold stays testable with
//! no side effects.
//!
//! P1 posture (mechanical gates): the fold NEVER decides that a branch can
//! be deleted based on age; deletion is authorized by the existing merge
//! gate (`WorktreeMergeGate::Merged` evidence) folded into `KillFacts`.
//! Under scheduled reap, the "skip-because-unmerged/dirty" tier maps to
//! `KillAction::Quarantine` (see [`crate::worktree::scheduled_action`]) —
//! atomic move, branch preserved.
//!
//! The half-state gate:
//! * Every tick, the caller runs `verify_integration_manifest()` first.
//! * `HookHalfState` / `Outdated` disables the reap for that tick, emits
//!   ONE `CheckErrored` + toast per hour (throttled via [`ReapFold`]).
//! * `Ok` proceeds to the fold.
//!
//! Selection truth table:
//! * !is_linked_worktree                    → SKIP (protect main)
//! * operator-focused                       → SKIP (never reap active work)
//! * any pane in Working/Blocked/Hibernated → SKIP (agent in flight)
//! * any Done pane (Idle + unseen)          → SKIP (unread completion)
//! * any pane with age < idle_threshold     → SKIP (not yet stale)
//! * else                                   → CANDIDATE

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::api::schema::AgentStatus;
use crate::worktree::{classify_kill_tier, KillFacts, KillTier};

use super::config::ReapConfig;

/// Name of the built-in — surfaces on every emitted `CheckErrored` /
/// `CheckFired`.
pub(crate) const REAP_CHECK_NAME: &str = "reap";

/// One pane's contribution to the reap fold. Everything the selection
/// truth-table needs is captured here.
#[derive(Debug, Clone)]
pub(crate) struct ReapPaneSnapshot {
    pub status: AgentStatus,
    /// True when the operator has cleared attention for this pane (the
    /// pane is `Idle+seen`, not the reap-hostile `Done` = `Idle+unseen`
    /// case).
    pub seen: bool,
    pub state_changed_at: Option<Instant>,
}

/// One workspace's contribution.
#[derive(Debug, Clone)]
pub(crate) struct ReapWorkspaceSnapshot {
    pub workspace_id: String,
    pub is_linked_worktree: bool,
    pub is_operator_focused: bool,
    pub repo_root: PathBuf,
    pub checkout_path: PathBuf,
    pub branch: Option<String>,
    /// Whether the checkout has uncommitted/untracked changes. Optional
    /// (git-probe may fail); the caller resolves — a missing signal is
    /// treated as `dirty=true` by convention (assume-dirty is safe under
    /// scheduled reap because that path becomes Quarantine, never delete).
    pub dirty: Option<bool>,
    /// Whether the branch's work has been merged elsewhere. Same optional
    /// semantics; a missing signal is treated as `merged=false`.
    pub merged: Option<bool>,
    pub panes: Vec<ReapPaneSnapshot>,
}

/// A reap candidate the App tick dispatches through the scheduled kill
/// machinery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReapCandidate {
    pub workspace_id: String,
    pub repo_root: PathBuf,
    pub checkout_path: PathBuf,
    pub branch: Option<String>,
    pub facts: KillFacts,
    pub tier: KillTier,
}

/// Throttle state for the manifest-drift errored emission. One emit per
/// hour so a persistent drift doesn't spam.
#[derive(Debug, Default, Clone)]
pub(crate) struct ReapFold {
    last_drift_emit: Option<Instant>,
}

const DRIFT_THROTTLE: Duration = Duration::from_secs(3600);

impl ReapFold {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Whether the caller should emit a `CheckErrored` + toast this tick.
    /// The FIRST call in each hour returns `true`; subsequent calls in the
    /// same hour return `false`. The caller only invokes this when a
    /// drift has been observed, so the throttle stays sticky as long as
    /// the operator hasn't cleared the drift.
    pub(crate) fn should_emit_drift(&mut self, now: Instant) -> bool {
        let should = self
            .last_drift_emit
            .is_none_or(|t| now.duration_since(t) >= DRIFT_THROTTLE);
        if should {
            self.last_drift_emit = Some(now);
        }
        should
    }

    /// Select reap candidates. Callers only invoke this when
    /// `verify_integration_manifest()` returned `Ok`; a drifted verdict
    /// yields ZERO candidates via early return in the caller.
    pub(crate) fn evaluate(
        &mut self,
        now: Instant,
        cfg: &ReapConfig,
        workspaces: &[ReapWorkspaceSnapshot],
    ) -> Vec<ReapCandidate> {
        if !cfg.enable {
            return Vec::new();
        }
        let threshold = Duration::from_secs(cfg.idle_threshold_secs.max(1));
        let mut out = Vec::new();
        for ws in workspaces {
            if !ws.is_linked_worktree || ws.is_operator_focused {
                continue;
            }
            if ws.panes.is_empty() {
                // Nothing to key off of; skip. (An empty workspace with a
                // stale checkout is a rare edge; wire an explicit fold if
                // the design ever needs it.)
                continue;
            }
            let all_ready = ws.panes.iter().all(|p| {
                if matches!(
                    p.status,
                    AgentStatus::Working
                        | AgentStatus::Blocked
                        | AgentStatus::Hibernated
                        | AgentStatus::Unknown
                ) {
                    return false;
                }
                if !p.seen {
                    // Done = Idle+unseen = unread completion; do not reap.
                    return false;
                }
                match p.state_changed_at {
                    Some(t) => now.duration_since(t) >= threshold,
                    None => false,
                }
            });
            if !all_ready {
                continue;
            }
            let facts = KillFacts {
                // We already gated on `is_linked_worktree`; is_main is
                // structurally false here. Kept explicit so a future
                // change to allow main-cleanup is a one-line edit.
                is_main: false,
                working_agent: false, // gated above
                dirty: ws.dirty.unwrap_or(true),
                merged: ws.merged.unwrap_or(false),
            };
            let tier = classify_kill_tier(facts);
            out.push(ReapCandidate {
                workspace_id: ws.workspace_id.clone(),
                repo_root: ws.repo_root.clone(),
                checkout_path: ws.checkout_path.clone(),
                branch: ws.branch.clone(),
                facts,
                tier,
            });
        }
        // Stable ordering for deterministic dispatch + tests.
        out.sort_by(|a, b| a.workspace_id.cmp(&b.workspace_id));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::{scheduled_action, KillAction};

    fn snapshot(
        workspace_id: &str,
        is_linked_worktree: bool,
        is_operator_focused: bool,
        panes: Vec<ReapPaneSnapshot>,
        dirty: Option<bool>,
        merged: Option<bool>,
    ) -> ReapWorkspaceSnapshot {
        ReapWorkspaceSnapshot {
            workspace_id: workspace_id.into(),
            is_linked_worktree,
            is_operator_focused,
            repo_root: PathBuf::from("/tmp/repo"),
            checkout_path: PathBuf::from(format!("/tmp/checkout-{workspace_id}")),
            branch: Some("feature-x".into()),
            dirty,
            merged,
            panes,
        }
    }

    fn idle_seen_at(secs_ago: u64, now: Instant) -> ReapPaneSnapshot {
        ReapPaneSnapshot {
            status: AgentStatus::Idle,
            seen: true,
            state_changed_at: Some(now - Duration::from_secs(secs_ago)),
        }
    }

    #[test]
    fn default_off_yields_no_candidates() {
        let mut fold = ReapFold::new();
        let cfg = ReapConfig::default(); // enable = false
        let now = Instant::now();
        let ws = vec![snapshot(
            "ws-1",
            true,
            false,
            vec![idle_seen_at(999_999, now)],
            Some(false),
            Some(true),
        )];
        assert!(fold.evaluate(now, &cfg, &ws).is_empty());
    }

    #[test]
    fn skips_non_linked_and_focused_workspaces() {
        let mut fold = ReapFold::new();
        let cfg = ReapConfig {
            enable: true,
            cadence_secs: 60,
            idle_threshold_secs: 10,
        };
        let now = Instant::now();
        let ws = vec![
            // main checkout — never reaped
            snapshot(
                "main",
                false,
                false,
                vec![idle_seen_at(100, now)],
                Some(false),
                Some(true),
            ),
            // linked but operator-focused — never reaped
            snapshot(
                "focused",
                true,
                true,
                vec![idle_seen_at(100, now)],
                Some(false),
                Some(true),
            ),
        ];
        assert!(fold.evaluate(now, &cfg, &ws).is_empty());
    }

    #[test]
    fn done_pane_blocks_reap() {
        let mut fold = ReapFold::new();
        let cfg = ReapConfig {
            enable: true,
            cadence_secs: 60,
            idle_threshold_secs: 10,
        };
        let now = Instant::now();
        // Done = Idle + unseen — unread completion, must not reap.
        let done = ReapPaneSnapshot {
            status: AgentStatus::Idle,
            seen: false,
            state_changed_at: Some(now - Duration::from_secs(100)),
        };
        let ws = vec![snapshot(
            "ws-done",
            true,
            false,
            vec![done],
            Some(false),
            Some(true),
        )];
        assert!(fold.evaluate(now, &cfg, &ws).is_empty());
    }

    #[test]
    fn working_or_blocked_pane_blocks_reap() {
        let mut fold = ReapFold::new();
        let cfg = ReapConfig {
            enable: true,
            cadence_secs: 60,
            idle_threshold_secs: 10,
        };
        let now = Instant::now();
        for status in [
            AgentStatus::Working,
            AgentStatus::Blocked,
            AgentStatus::Hibernated,
        ] {
            let live = ReapPaneSnapshot {
                status,
                seen: true,
                state_changed_at: Some(now - Duration::from_secs(1_000_000)),
            };
            let ws = vec![snapshot(
                "ws",
                true,
                false,
                vec![live],
                Some(false),
                Some(true),
            )];
            assert!(
                fold.evaluate(now, &cfg, &ws).is_empty(),
                "status {status:?} must block reap"
            );
        }
    }

    #[test]
    fn adversarial_dirty_and_unpushed_map_to_quarantine_via_scheduled_action() {
        // §8.5 adversarial rows: a workspace that's Idle+seen past
        // threshold, dirty AND unmerged, must classify as
        // SkipUnmergedDirty which the scheduled path converts to
        // Quarantine — never a delete.
        let mut fold = ReapFold::new();
        let cfg = ReapConfig {
            enable: true,
            cadence_secs: 60,
            idle_threshold_secs: 10,
        };
        let now = Instant::now();
        let ws = vec![snapshot(
            "dirty",
            true,
            false,
            vec![idle_seen_at(100, now)],
            Some(true),  // dirty
            Some(false), // unmerged
        )];
        let candidates = fold.evaluate(now, &cfg, &ws);
        assert_eq!(candidates.len(), 1);
        let c = &candidates[0];
        assert_eq!(c.tier, KillTier::SkipUnmergedDirty);
        assert_eq!(
            scheduled_action(c.tier),
            KillAction::Quarantine,
            "SkipUnmergedDirty must Quarantine under scheduled reap"
        );
    }

    #[test]
    fn merge_gated_delete_only_with_positive_evidence() {
        let mut fold = ReapFold::new();
        let cfg = ReapConfig {
            enable: true,
            cadence_secs: 60,
            idle_threshold_secs: 10,
        };
        let now = Instant::now();
        // Clean tree + merged branch → KillBranch { dirty: false } which
        // scheduled_action passes through unchanged (evidence present).
        let ws = vec![snapshot(
            "merged",
            true,
            false,
            vec![idle_seen_at(100, now)],
            Some(false),
            Some(true),
        )];
        let candidates = fold.evaluate(now, &cfg, &ws);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].tier, KillTier::KillBranch { dirty: false });
        assert_eq!(
            scheduled_action(candidates[0].tier),
            KillAction::KillBranch { dirty: false }
        );
        // Same shape but NO evidence: never a delete.
        let ws = vec![snapshot(
            "unproven",
            true,
            false,
            vec![idle_seen_at(100, now)],
            Some(false),
            Some(false),
        )];
        let candidates = fold.evaluate(now, &cfg, &ws);
        assert_eq!(candidates[0].tier, KillTier::CheckoutOnly);
        // CheckoutOnly is "remove the checkout, keep the branch". No
        // branch deletion, matching the invariant.
        assert_eq!(
            scheduled_action(candidates[0].tier),
            KillAction::CheckoutOnly
        );
    }

    #[test]
    fn threshold_gates_reap() {
        let mut fold = ReapFold::new();
        let cfg = ReapConfig {
            enable: true,
            cadence_secs: 60,
            idle_threshold_secs: 100,
        };
        let now = Instant::now();
        // 50s idle < 100s threshold → not yet reap-eligible.
        let ws = vec![snapshot(
            "young",
            true,
            false,
            vec![idle_seen_at(50, now)],
            Some(false),
            Some(true),
        )];
        assert!(fold.evaluate(now, &cfg, &ws).is_empty());
        // 200s idle >= threshold → candidate.
        let ws = vec![snapshot(
            "old",
            true,
            false,
            vec![idle_seen_at(200, now)],
            Some(false),
            Some(true),
        )];
        assert_eq!(fold.evaluate(now, &cfg, &ws).len(), 1);
    }

    #[test]
    fn drift_throttle_emits_once_per_hour() {
        let mut fold = ReapFold::new();
        let now = Instant::now();
        assert!(fold.should_emit_drift(now), "first tick after boot fires");
        assert!(
            !fold.should_emit_drift(now + Duration::from_secs(30 * 60)),
            "same 30m later is throttled"
        );
        assert!(
            fold.should_emit_drift(now + Duration::from_secs(3600)),
            "one hour later fires again"
        );
    }
}
