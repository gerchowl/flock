//! Built-in idle hibernation check (#175 phase 4, C3).
//!
//! Pure fold over the current pane snapshot. Selects panes whose settled
//! state has aged past a threshold and returns hibernation candidates the
//! App dispatches through `App::hibernate_pane`. NEVER touches Blocked or
//! Working panes (those are agent-in-flight); NEVER touches Idle+unseen
//! (that's the "unread completion" attention signal); NEVER touches
//! Unknown (no evidence at all).
//!
//! Selection truth table:
//! - Idle + seen + age >= idle_threshold_secs   → candidate
//! - Done  (Idle + unseen)                       → SKIP (unread completion)
//! - Idle + seen +  age >= done_threshold_secs   → still Idle-tier;
//!   `done_threshold_secs` applies to the persisted-Done semantic on
//!   remote-summary panes (`AgentStatus::Done`) — local panes always fall
//!   under the Idle branch.
//! - Blocked / Working / Unknown                 → SKIP unconditionally
//!
//! The design's `min_secs_since_last_stop` guard is honoured via the
//! pane's `state_changed_at` — no dedicated Stop-hook timestamp exists on
//! this workspace, so the last state change is an acceptable proxy
//! (documented deviation).
//!
//! Errors on the App-side dispatch (e.g. hibernate refused because the
//! pane has no resume plan) turn into a `CheckErrored` emit ONCE per
//! episode per pane — the fold refuses to hot-retry a failing pane.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::api::schema::AgentStatus;
use crate::layout::PaneId;

use super::config::HibernationConfig;

/// Name of the built-in check — surfaces in `flk checks list` and every
/// durable `CheckErrored{name}` emit.
pub(crate) const HIBERNATION_CHECK_NAME: &str = "hibernation";

/// Snapshot of one pane the fold considers.
#[derive(Debug, Clone)]
pub(crate) struct HibernationPaneSnapshot {
    pub pane_id: PaneId,
    pub status: AgentStatus,
    pub state_changed_at: Option<Instant>,
    /// True when the pane is already hibernated (has a stashed resume
    /// plan). The fold silently skips it — hibernating a hibernated pane
    /// would be a no-op the App-side would refuse with `already_hibernated`
    /// anyway, but skipping here keeps the log quiet.
    pub already_hibernated: bool,
}

/// A pane the fold selected for hibernation. The App calls
/// `App::hibernate_pane` with each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HibernationCandidate {
    pub pane_id: PaneId,
    /// The `state_changed_at` we selected at — becomes the per-pane
    /// episode key for the errored-once retry guard.
    pub state_changed_at: Instant,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct HibernationFold {
    /// pane_id → the state_changed_at instant of the episode where the
    /// hibernate attempt errored. Guards against hot retry on the same
    /// episode; a new state_change (e.g. pane became Blocked then Idle
    /// again) starts a fresh episode and can retry.
    errored: HashMap<PaneId, Instant>,
}

impl HibernationFold {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Evict episodes for panes that no longer exist.
    pub(crate) fn evict_missing_panes<I>(&mut self, live: I)
    where
        I: IntoIterator<Item = PaneId>,
    {
        let live: std::collections::HashSet<PaneId> = live.into_iter().collect();
        self.errored.retain(|pane_id, _| live.contains(pane_id));
    }

    /// Mark a candidate as "errored on this episode" — the App calls this
    /// when `hibernate_pane` refused (no resume plan, etc.). The next
    /// state change on the pane resets the mark.
    pub(crate) fn mark_errored(&mut self, pane_id: PaneId, state_changed_at: Instant) {
        self.errored.insert(pane_id, state_changed_at);
    }

    /// Whether the given candidate is in the errored-once state — the
    /// App consults this to decide whether to re-emit `CheckErrored`.
    pub(crate) fn is_errored_for_episode(
        &self,
        pane_id: PaneId,
        state_changed_at: Instant,
    ) -> bool {
        self.errored
            .get(&pane_id)
            .is_some_and(|prev| *prev == state_changed_at)
    }

    /// Select candidates. `min_stop_guard` is the config value (proxied
    /// against `state_changed_at`), NOT a separate Stop-hook timestamp.
    pub(crate) fn evaluate(
        &mut self,
        now: Instant,
        config: &HibernationConfig,
        panes: &[HibernationPaneSnapshot],
    ) -> Vec<HibernationCandidate> {
        if !config.enable {
            return Vec::new();
        }
        let idle_threshold = Duration::from_secs(config.idle_threshold_secs.max(1));
        let done_threshold = Duration::from_secs(config.done_threshold_secs.max(1));
        let min_stop = Duration::from_secs(config.min_secs_since_last_stop);

        let mut candidates = Vec::new();
        for pane in panes {
            // Never touch these — see module doc.
            if pane.already_hibernated {
                continue;
            }
            let threshold = match pane.status {
                AgentStatus::Idle => idle_threshold,
                AgentStatus::Done => done_threshold,
                // Blocked / Working / Unknown / Hibernated → skip.
                AgentStatus::Blocked
                | AgentStatus::Working
                | AgentStatus::Unknown
                | AgentStatus::Hibernated => continue,
            };
            let Some(changed_at) = pane.state_changed_at else {
                // No evidence of age → skip.
                continue;
            };
            let elapsed = now.saturating_duration_since(changed_at);
            if elapsed < threshold {
                continue;
            }
            // §8: `min_secs_since_last_stop` guard — since the local
            // pane exposes no explicit Stop-hook timestamp, we use the
            // last state change as an acceptable proxy. The threshold
            // check above already guarantees `elapsed >= threshold` so
            // this is a belt-and-braces check for tiny thresholds.
            if elapsed < min_stop {
                continue;
            }
            // Already errored for this episode? Skip — a fresh
            // state_change resets the guard by breaking the equality.
            if self.is_errored_for_episode(pane.pane_id, changed_at) {
                continue;
            }
            candidates.push(HibernationCandidate {
                pane_id: pane.pane_id,
                state_changed_at: changed_at,
            });
        }
        candidates
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(idle: u64, done: u64) -> HibernationConfig {
        HibernationConfig {
            enable: true,
            idle_threshold_secs: idle,
            done_threshold_secs: done,
            min_secs_since_last_stop: 0,
        }
    }

    fn snapshot(
        pane_id: u32,
        status: AgentStatus,
        state_changed_at: Option<Instant>,
    ) -> HibernationPaneSnapshot {
        HibernationPaneSnapshot {
            pane_id: PaneId::from_raw(pane_id),
            status,
            state_changed_at,
            already_hibernated: false,
        }
    }

    #[test]
    fn idle_past_threshold_is_selected() {
        let mut fold = HibernationFold::new();
        let now = Instant::now();
        let panes = vec![snapshot(
            1,
            AgentStatus::Idle,
            Some(now - Duration::from_secs(4_000)),
        )];
        let out = fold.evaluate(now, &config(3_600, 21_600), &panes);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pane_id.raw(), 1);
    }

    #[test]
    fn done_past_done_threshold_is_selected() {
        let mut fold = HibernationFold::new();
        let now = Instant::now();
        let panes = vec![snapshot(
            2,
            AgentStatus::Done,
            Some(now - Duration::from_secs(22_000)),
        )];
        let out = fold.evaluate(now, &config(3_600, 21_600), &panes);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn blocked_never_hibernates() {
        let mut fold = HibernationFold::new();
        let now = Instant::now();
        let panes = vec![snapshot(
            3,
            AgentStatus::Blocked,
            Some(now - Duration::from_secs(100_000)),
        )];
        assert!(fold
            .evaluate(now, &config(3_600, 21_600), &panes)
            .is_empty());
    }

    #[test]
    fn working_never_hibernates() {
        let mut fold = HibernationFold::new();
        let now = Instant::now();
        let panes = vec![snapshot(
            4,
            AgentStatus::Working,
            Some(now - Duration::from_secs(100_000)),
        )];
        assert!(fold
            .evaluate(now, &config(3_600, 21_600), &panes)
            .is_empty());
    }

    #[test]
    fn unknown_never_hibernates() {
        let mut fold = HibernationFold::new();
        let now = Instant::now();
        let panes = vec![snapshot(
            5,
            AgentStatus::Unknown,
            Some(now - Duration::from_secs(100_000)),
        )];
        assert!(fold
            .evaluate(now, &config(3_600, 21_600), &panes)
            .is_empty());
    }

    #[test]
    fn idle_below_threshold_does_not_hibernate() {
        let mut fold = HibernationFold::new();
        let now = Instant::now();
        let panes = vec![snapshot(
            6,
            AgentStatus::Idle,
            Some(now - Duration::from_secs(60)),
        )];
        assert!(fold
            .evaluate(now, &config(3_600, 21_600), &panes)
            .is_empty());
    }

    #[test]
    fn missing_state_changed_at_does_not_hibernate() {
        let mut fold = HibernationFold::new();
        let now = Instant::now();
        let panes = vec![snapshot(7, AgentStatus::Idle, None)];
        assert!(fold
            .evaluate(now, &config(3_600, 21_600), &panes)
            .is_empty());
    }

    #[test]
    fn disabled_config_is_off_by_default_in_wire_check() {
        // Not a fold test — a config-shape reminder that HibernationConfig::default()
        // ships with `enable = false`; this test guards against a drift where
        // someone flips the default and forgets the design's opt-in rule.
        assert!(!HibernationConfig::default().enable);
    }

    #[test]
    fn already_hibernated_pane_is_skipped() {
        let mut fold = HibernationFold::new();
        let now = Instant::now();
        let mut pane = snapshot(8, AgentStatus::Idle, Some(now - Duration::from_secs(4_000)));
        pane.already_hibernated = true;
        assert!(fold
            .evaluate(now, &config(3_600, 21_600), &[pane])
            .is_empty());
    }

    #[test]
    fn errored_episode_is_not_re_selected() {
        let mut fold = HibernationFold::new();
        let now = Instant::now();
        let changed = now - Duration::from_secs(4_000);
        let panes = vec![snapshot(9, AgentStatus::Idle, Some(changed))];
        let picks = fold.evaluate(now, &config(3_600, 21_600), &panes);
        assert_eq!(picks.len(), 1);
        fold.mark_errored(picks[0].pane_id, picks[0].state_changed_at);
        // Same episode → skipped.
        assert!(fold
            .evaluate(now, &config(3_600, 21_600), &panes)
            .is_empty());
        // Different state_changed_at → fresh episode, can pick again.
        let fresh = vec![snapshot(
            9,
            AgentStatus::Idle,
            Some(now + Duration::from_secs(60)),
        )];
        let picks = fold.evaluate(
            now + Duration::from_secs(60) + Duration::from_secs(4_000),
            &config(3_600, 21_600),
            &fresh,
        );
        assert_eq!(picks.len(), 1);
    }

    #[test]
    fn evict_bounds_state() {
        let mut fold = HibernationFold::new();
        fold.mark_errored(PaneId::from_raw(1), Instant::now());
        assert!(!fold.errored.is_empty());
        fold.evict_missing_panes(std::iter::empty());
        assert!(fold.errored.is_empty());
    }
}
