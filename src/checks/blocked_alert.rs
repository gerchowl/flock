//! Built-in blocked-alert check (#175 phase 4, C2).
//!
//! A pure fold — no subprocess, no async — over the current pane snapshot.
//! Fires ONCE per episode where an "episode" is a single continuous span of
//! agent `Blocked`-ness on a pane. Episode key = `(pane_id, state_changed_at)`:
//! when the pane leaves `Blocked` its next state change bumps
//! `state_changed_at`, so re-blocking starts a fresh episode and fires again
//! (US-5).
//!
//! Blocked panes are NEVER skipped by any policy in this fold — that would
//! silently swallow the "agent needs a decision, waiting" signal that this
//! check exists to surface. Ack is per-episode: an ack marks every currently
//! Blocked pane's in-flight episode as suppressed; the next fresh episode
//! (post-unblock) can fire again.
//!
//! The fold is CALLED from the checks tick and returns [`BlockedAlertFire`]s
//! that the App dispatches through the same `ActionSpec::Notify` machinery
//! the runner uses for script checks (so `[ui.toast]` policy applies
//! uniformly and the resulting `CheckFired` durable event carries a stable
//! episode string).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::api::schema::AgentStatus;
use crate::layout::PaneId;

use super::config::BlockedAlertConfig;

/// Snapshot of one pane the fold considers. The App builds these once per
/// tick from `pane_details` so the fold stays a pure function over data.
#[derive(Debug, Clone)]
pub(crate) struct BlockedPaneSnapshot {
    pub pane_id: PaneId,
    /// Public pane id (e.g. `w_1:p3`) — surfaces in the notification title
    /// and durable event so operators can jump to the pane.
    pub public_pane_id: String,
    /// Human-readable pane label (agent name or fallback).
    pub label: String,
    pub status: AgentStatus,
    pub state_changed_at: Option<Instant>,
}

/// One fire the App dispatches into the notification / durable-event paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlockedAlertFire {
    pub pane_id: PaneId,
    pub public_pane_id: String,
    pub label: String,
    pub episode: String,
    pub duration_secs: u64,
}

/// The blocked-alert check name — reused by the durable `CheckFired` event,
/// `flk checks list`, and `flk checks ack <name>`.
pub(crate) const BLOCKED_ALERT_CHECK_NAME: &str = "blocked_alert";

#[derive(Debug, Default, Clone)]
pub(crate) struct BlockedAlertFold {
    /// pane_id → `state_changed_at` of the episode we already fired for.
    /// When `state_changed_at` differs on a later tick, that's a new episode.
    fired: HashMap<PaneId, Instant>,
    /// pane_id → `state_changed_at` of the episode the operator acked. Same
    /// key discipline as `fired`: re-blocking mints a new instant, so the ack
    /// naturally lapses at episode boundaries.
    acked: HashMap<PaneId, Instant>,
    /// Monotonic episode counter — the durable `CheckFired.episode` string
    /// uses this so events stay unique across pane reuse.
    next_episode: u64,
}

impl BlockedAlertFold {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Evict episodes for panes that no longer exist. Called with the live
    /// pane-id set at the top of each fold so the state stays bounded.
    pub(crate) fn evict_missing_panes<I>(&mut self, live: I)
    where
        I: IntoIterator<Item = PaneId>,
    {
        let live: std::collections::HashSet<PaneId> = live.into_iter().collect();
        self.fired.retain(|pane_id, _| live.contains(pane_id));
        self.acked.retain(|pane_id, _| live.contains(pane_id));
    }

    /// Ack every currently Blocked episode so no more fires land for it until
    /// each pane leaves + re-enters Blocked. Returns the number of episodes
    /// acked (0 when nothing was blocked). Consumed by `flk checks ack` in a
    /// later phase-4 commit — the wiring is intentionally landing in commit D
    /// alongside the runner-side ack call site so both wake at the same time.
    #[allow(
        dead_code,
        reason = "consumed by `flk checks ack` in the phase 4 CLI commit"
    )]
    pub(crate) fn ack_current_episodes(&mut self, panes: &[BlockedPaneSnapshot]) -> usize {
        let mut acked = 0;
        for pane in panes {
            if pane.status != AgentStatus::Blocked {
                continue;
            }
            let Some(changed_at) = pane.state_changed_at else {
                continue;
            };
            self.acked.insert(pane.pane_id, changed_at);
            acked += 1;
        }
        acked
    }

    /// Run the fold. Returns one fire per pane that crossed the threshold in
    /// a new episode; mutates internal state so the same episode doesn't fire
    /// again until it ends.
    pub(crate) fn evaluate(
        &mut self,
        now: Instant,
        config: &BlockedAlertConfig,
        panes: &[BlockedPaneSnapshot],
    ) -> Vec<BlockedAlertFire> {
        if !config.enable {
            return Vec::new();
        }
        let threshold = Duration::from_secs(config.threshold_secs.max(1));
        let mut fires = Vec::new();
        for pane in panes {
            if pane.status != AgentStatus::Blocked {
                // Episode ended (or never was) — drop stale bookkeeping so a
                // future re-block starts fresh.
                self.fired.remove(&pane.pane_id);
                self.acked.remove(&pane.pane_id);
                continue;
            }
            let Some(changed_at) = pane.state_changed_at else {
                // No `state_changed_at` means we can't age the episode — the
                // fold refuses to fire without evidence of duration rather
                // than emitting stale-time-1970 alerts.
                continue;
            };
            // Episode boundary check: if the pane's `state_changed_at` moved
            // (unblock + re-block) since our last bookkeeping, this is a NEW
            // episode; forget the old fired/acked marks so it can fire again.
            if self
                .fired
                .get(&pane.pane_id)
                .is_some_and(|prev| *prev != changed_at)
            {
                self.fired.remove(&pane.pane_id);
            }
            if self
                .acked
                .get(&pane.pane_id)
                .is_some_and(|prev| *prev != changed_at)
            {
                self.acked.remove(&pane.pane_id);
            }
            let elapsed = now.saturating_duration_since(changed_at);
            if elapsed < threshold {
                continue;
            }
            if self.fired.contains_key(&pane.pane_id) {
                continue;
            }
            if self.acked.get(&pane.pane_id) == Some(&changed_at) {
                continue;
            }
            self.fired.insert(pane.pane_id, changed_at);
            self.next_episode = self.next_episode.saturating_add(1);
            fires.push(BlockedAlertFire {
                pane_id: pane.pane_id,
                public_pane_id: pane.public_pane_id.clone(),
                label: pane.label.clone(),
                episode: format!(
                    "{BLOCKED_ALERT_CHECK_NAME}:{}:{}",
                    pane.pane_id.raw(),
                    self.next_episode
                ),
                duration_secs: elapsed.as_secs(),
            });
        }
        fires
    }
}

/// Human-friendly duration for the notification title ("blocked 30m: <pane>").
pub(crate) fn format_blocked_duration(secs: u64) -> String {
    if secs >= 3_600 {
        let hours = secs / 3_600;
        let minutes = (secs % 3_600) / 60;
        if minutes == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h{minutes}m")
        }
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn snapshot(
        pane_id: PaneId,
        status: AgentStatus,
        state_changed_at: Option<Instant>,
    ) -> BlockedPaneSnapshot {
        BlockedPaneSnapshot {
            pane_id,
            public_pane_id: format!("w_1:p{}", pane_id.raw()),
            label: format!("agent-{}", pane_id.raw()),
            status,
            state_changed_at,
        }
    }

    fn config(threshold_secs: u64) -> BlockedAlertConfig {
        BlockedAlertConfig {
            enable: true,
            threshold_secs,
        }
    }

    #[test]
    fn fires_once_per_episode() {
        let mut fold = BlockedAlertFold::new();
        let now = Instant::now();
        let pane = PaneId::from_raw(7);
        let changed_at = now - Duration::from_secs(120);
        let panes = vec![snapshot(pane, AgentStatus::Blocked, Some(changed_at))];

        let first = fold.evaluate(now, &config(60), &panes);
        assert_eq!(first.len(), 1);
        assert!(first[0].episode.contains(":7:"));
        assert_eq!(first[0].duration_secs, 120);

        // Same episode, another tick — nothing.
        assert!(fold
            .evaluate(now + Duration::from_secs(1), &config(60), &panes)
            .is_empty());
    }

    #[test]
    fn reblock_is_a_new_episode_and_fires_again() {
        let mut fold = BlockedAlertFold::new();
        let now = Instant::now();
        let pane = PaneId::from_raw(7);
        let first_episode = now - Duration::from_secs(120);
        let first_snapshot = vec![snapshot(pane, AgentStatus::Blocked, Some(first_episode))];
        assert_eq!(fold.evaluate(now, &config(60), &first_snapshot).len(), 1);

        // Pane goes Idle — the fold cleans up its episode bookkeeping.
        let idle = vec![snapshot(pane, AgentStatus::Idle, Some(now))];
        assert!(fold
            .evaluate(now + Duration::from_secs(30), &config(60), &idle)
            .is_empty());

        // Re-block with a fresh state_changed_at: new episode fires.
        let second_episode = now + Duration::from_secs(60);
        let second_snapshot = vec![snapshot(pane, AgentStatus::Blocked, Some(second_episode))];
        let fires = fold.evaluate(
            second_episode + Duration::from_secs(120),
            &config(60),
            &second_snapshot,
        );
        assert_eq!(fires.len(), 1, "re-block must fire a new episode");
    }

    #[test]
    fn ack_suppresses_current_episode_only() {
        let mut fold = BlockedAlertFold::new();
        let now = Instant::now();
        let pane = PaneId::from_raw(7);
        let changed_at = now - Duration::from_secs(120);
        let panes = vec![snapshot(pane, AgentStatus::Blocked, Some(changed_at))];

        // Ack BEFORE the first fire — the ack tracks the current episode key.
        assert_eq!(fold.ack_current_episodes(&panes), 1);
        assert!(fold.evaluate(now, &config(60), &panes).is_empty());

        // Re-block: ack lapses at the episode boundary.
        let second_episode = now + Duration::from_secs(60);
        let second_snapshot = vec![snapshot(pane, AgentStatus::Blocked, Some(second_episode))];
        let fires = fold.evaluate(
            second_episode + Duration::from_secs(120),
            &config(60),
            &second_snapshot,
        );
        assert_eq!(fires.len(), 1, "new episode after ack fires");
    }

    #[test]
    fn below_threshold_does_not_fire() {
        let mut fold = BlockedAlertFold::new();
        let now = Instant::now();
        let pane = PaneId::from_raw(7);
        let changed_at = now - Duration::from_secs(30);
        let panes = vec![snapshot(pane, AgentStatus::Blocked, Some(changed_at))];
        assert!(fold.evaluate(now, &config(60), &panes).is_empty());

        // Later — past the threshold — it fires.
        let later = now + Duration::from_secs(60);
        let fires = fold.evaluate(later, &config(60), &panes);
        assert_eq!(fires.len(), 1);
    }

    #[test]
    fn disabled_config_never_fires() {
        let mut fold = BlockedAlertFold::new();
        let now = Instant::now();
        let panes = vec![snapshot(
            PaneId::from_raw(7),
            AgentStatus::Blocked,
            Some(now - Duration::from_secs(3_600)),
        )];
        let mut cfg = config(60);
        cfg.enable = false;
        assert!(fold.evaluate(now, &cfg, &panes).is_empty());
    }

    #[test]
    fn blocked_panes_are_never_skipped_by_other_policies() {
        // Even for a pane that never had a hook and shows up with a stale
        // state_changed_at from a peer sync, blocked-alert must fire past the
        // threshold — swallowing it silently is the anti-goal.
        let mut fold = BlockedAlertFold::new();
        let now = Instant::now();
        let pane = PaneId::from_raw(7);
        let panes = vec![snapshot(
            pane,
            AgentStatus::Blocked,
            Some(now - Duration::from_secs(7_200)),
        )];
        let fires = fold.evaluate(now, &config(60), &panes);
        assert_eq!(fires.len(), 1);
    }

    #[test]
    fn non_blocked_status_does_not_fire() {
        let mut fold = BlockedAlertFold::new();
        let now = Instant::now();
        let pane = PaneId::from_raw(7);
        for status in [
            AgentStatus::Idle,
            AgentStatus::Working,
            AgentStatus::Done,
            AgentStatus::Unknown,
        ] {
            let panes = vec![snapshot(
                pane,
                status,
                Some(now - Duration::from_secs(3_600)),
            )];
            assert!(fold.evaluate(now, &config(60), &panes).is_empty());
        }
    }

    #[test]
    fn missing_state_changed_at_does_not_fire() {
        let mut fold = BlockedAlertFold::new();
        let now = Instant::now();
        let panes = vec![snapshot(PaneId::from_raw(7), AgentStatus::Blocked, None)];
        assert!(fold.evaluate(now, &config(60), &panes).is_empty());
    }

    #[test]
    fn evict_missing_panes_bounds_the_state() {
        let mut fold = BlockedAlertFold::new();
        let now = Instant::now();
        let pane = PaneId::from_raw(7);
        let panes = vec![snapshot(
            pane,
            AgentStatus::Blocked,
            Some(now - Duration::from_secs(120)),
        )];
        assert_eq!(fold.evaluate(now, &config(60), &panes).len(), 1);
        assert!(fold.fired.contains_key(&pane));
        fold.evict_missing_panes(std::iter::empty());
        assert!(!fold.fired.contains_key(&pane));
    }

    #[test]
    fn duration_formatter_shapes_the_title() {
        assert_eq!(format_blocked_duration(15), "15s");
        assert_eq!(format_blocked_duration(120), "2m");
        assert_eq!(format_blocked_duration(3_600), "1h");
        assert_eq!(format_blocked_duration(3_720), "1h2m");
    }
}
