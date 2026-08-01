//! Per-check runner state machine (#175 phase 4 commit 3).
//!
//! The runner OWNS the debounce state and scheduling, but never executes
//! actions. It returns [`FireDecision`]s to the App, which folds them into
//! the existing notification / event paths. This is P1 (mechanical gates):
//! the runner's job is deciding WHETHER an episode should fire, based on
//! consecutive outcomes and ack state. It does not know about toasts,
//! sound, or the event hub.
//!
//! State per check:
//! * `next_due` — instant of the next run; the scheduler polls this.
//! * `consecutive_fails` — number of Fire outcomes since the last Pass.
//! * `fired_this_episode` — set once a FireDecision is emitted; cleared on
//!   the next Pass so a fresh trip triggers a new episode.
//! * `in_flight` — a run has been dispatched; blocks re-dispatch until the
//!   outcome comes back. Also participates in the concurrency cap.
//! * `last_outcome` — most recent Outcome (for readback + tests).
//! * `ack_until` — user-visible "silence this check until <instant>". The
//!   runner still runs the check but refuses to fire while acked.
//!
//! Scheduling (§8.4, machine-asleep semantics): after a long sleep, a check
//! whose `next_due` is deep in the past runs AT MOST ONCE — completion sets
//! `next_due = now + interval`, not a naïve accumulation that would replay
//! every missed tick.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::config::{ActionSpec, ChecksConfig, ScriptCheck};
use super::script::Outcome;

/// Per-check runtime state — the runner's authoritative view of one check.
#[derive(Debug, Clone)]
pub(crate) struct CheckRuntimeState {
    pub next_due: Instant,
    pub consecutive_fails: u32,
    pub fired_this_episode: bool,
    pub in_flight: bool,
    pub last_outcome: Option<Outcome>,
    pub ack_until: Option<Instant>,
}

impl CheckRuntimeState {
    fn new(next_due: Instant) -> Self {
        Self {
            next_due,
            consecutive_fails: 0,
            fired_this_episode: false,
            in_flight: false,
            last_outcome: None,
            ack_until: None,
        }
    }

    fn is_acked(&self, now: Instant) -> bool {
        self.ack_until.is_some_and(|until| now < until)
    }
}

/// The runner emits a FireDecision when a check crosses the debounce and
/// hasn't already fired for the current episode. The App is free to look up
/// the check's `on_fire: ActionSpec` and dispatch — but the DECISION is
/// what's tested here, not the dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FireDecision {
    pub name: String,
    pub episode: String,
    pub action: ActionSpec,
}

/// One check ready to run (returned by `next_runnable`). The App spawns the
/// executor with this data; the runner marks it in-flight so nothing else
/// re-dispatches while the work is out.
///
/// The `check` field is unused inside this commit — the App-side dispatch
/// that hands it to `script::run_script` lands in the runner-wiring commit
/// alongside `AppEvent::CheckCompleted`. Kept here so the wiring commit
/// doesn't reshape the runner's public surface.
#[derive(Debug, Clone)]
pub(crate) struct RunnableCheck {
    pub name: String,
    #[allow(
        dead_code,
        reason = "consumed by the App tick in the runner-wiring commit (phase 4 c4)"
    )]
    pub check: ScriptCheck,
}

/// The check runner. Owns per-check state, applies outcomes, and answers
/// scheduling questions. It's a plain struct — no threads, no channels —
/// so tests exercise the state machine directly.
#[derive(Debug, Clone)]
pub(crate) struct CheckRunner {
    /// The configured scripts, keyed by name. Cloned into RunnableCheck when
    /// dispatched so the executor thread doesn't hold a reference into the
    /// runner across the tick.
    scripts: HashMap<String, ScriptCheck>,
    state: HashMap<String, CheckRuntimeState>,
    max_concurrent: usize,
    /// Episode monotonic counter — every FireDecision has a unique id so
    /// the app can correlate CheckFired events with an episode.
    next_episode: u64,
}

impl CheckRunner {
    /// Build a runner from a config snapshot. Only valid script entries
    /// (those `ChecksConfig::diagnostics()` accepts) are enrolled; the rest
    /// are silently dropped here — the diagnostic path is the surface.
    pub(crate) fn from_config(config: &ChecksConfig, now: Instant) -> Self {
        let max_concurrent = config.max_concurrent.max(1);
        let min_interval_secs = config.min_tick_secs.max(1);
        let mut scripts: HashMap<String, ScriptCheck> = HashMap::new();
        let mut state: HashMap<String, CheckRuntimeState> = HashMap::new();
        for script in &config.scripts {
            let name = script.name.trim();
            if name.is_empty() || scripts.contains_key(name) {
                continue;
            }
            if super::config::accepted_program_path(&script.program).is_none() {
                continue;
            }
            // Clamp the per-script interval to the runner's minimum tick.
            let interval = Duration::from_secs(script.interval_secs.max(min_interval_secs));
            let due = now + interval.min(Duration::from_secs(min_interval_secs));
            // The scheduler kicks the first run one min-tick after startup so
            // the loop is quiet during construction. `next_due(now)` will
            // return this instant; `next_runnable(now)` gates on it.
            let _ = due;
            let _ = interval;
            scripts.insert(name.to_string(), script.clone());
            state.insert(
                name.to_string(),
                CheckRuntimeState::new(now + Duration::from_secs(min_interval_secs)),
            );
        }
        Self {
            scripts,
            state,
            max_concurrent,
            next_episode: 0,
        }
    }

    /// Number of checks currently in flight. Consumed by the App to gate
    /// dispatch; consumed by tests to assert the concurrency cap.
    pub(crate) fn in_flight(&self) -> usize {
        self.state.values().filter(|s| s.in_flight).count()
    }

    /// The nearest deadline the loop must wake for. `None` means the runner
    /// has nothing pending (no checks, or all in-flight with none due). The
    /// App folds this into `next_loop_deadline_with_resize_poll`.
    pub(crate) fn next_due(&self, _now: Instant) -> Option<Instant> {
        self.state
            .values()
            .filter(|s| !s.in_flight)
            .map(|s| s.next_due)
            .min()
    }

    /// Pop up to `max_concurrent - in_flight` checks whose `next_due <= now`.
    /// Each returned RunnableCheck is marked in-flight; the caller MUST call
    /// [`complete`] later or the check will never re-dispatch.
    pub(crate) fn next_runnable(&mut self, now: Instant) -> Vec<RunnableCheck> {
        let slots = self.max_concurrent.saturating_sub(self.in_flight());
        if slots == 0 {
            return Vec::new();
        }
        let mut due_names: Vec<String> = self
            .state
            .iter()
            .filter(|(_, s)| !s.in_flight && s.next_due <= now)
            .map(|(name, _)| name.clone())
            .collect();
        // Stable ordering keeps behaviour deterministic for tests.
        due_names.sort();
        due_names.truncate(slots);
        let mut out = Vec::with_capacity(due_names.len());
        for name in due_names {
            if let (Some(state), Some(script)) =
                (self.state.get_mut(&name), self.scripts.get(&name))
            {
                state.in_flight = true;
                out.push(RunnableCheck {
                    name: name.clone(),
                    check: script.clone(),
                });
            }
        }
        out
    }

    /// Fold one completed run's outcome into the state machine. Returns
    /// `Some(FireDecision)` when this outcome crossed the debounce and the
    /// episode hadn't already fired.
    pub(crate) fn complete(
        &mut self,
        name: &str,
        outcome: Outcome,
        now: Instant,
    ) -> Option<FireDecision> {
        let script = self.scripts.get(name)?.clone();
        let state = self.state.get_mut(name)?;
        state.in_flight = false;
        state.last_outcome = Some(outcome.clone());

        // Machine-asleep semantics: `next_due = now + interval`, not a naïve
        // += interval, so a check that missed 20 ticks during suspend fires
        // once (§8.4).
        let interval = Duration::from_secs(script.interval_secs.max(1));
        state.next_due = now + interval;

        match outcome {
            Outcome::Fire => {
                state.consecutive_fails = state.consecutive_fails.saturating_add(1);
                let debounce = script.debounce.max(1);
                if state.consecutive_fails < debounce
                    || state.fired_this_episode
                    || state.is_acked(now)
                {
                    return None;
                }
                state.fired_this_episode = true;
                self.next_episode = self.next_episode.saturating_add(1);
                Some(FireDecision {
                    name: name.to_string(),
                    episode: format!("{name}:{}", self.next_episode),
                    action: script.on_fire.clone(),
                })
            }
            Outcome::Pass => {
                state.consecutive_fails = 0;
                state.fired_this_episode = false;
                None
            }
            // §8.1: Error leaves the debounce counter untouched — a
            // one-off mechanical failure must not reset a real fire streak.
            Outcome::Error(_) => None,
        }
    }

    /// Suppress fires from `name` until `now + interval_secs`. The check
    /// keeps running (so a Pass resets the streak); fires just don't emit.
    /// The interactive `ack` path — a UI action / API verb — lands in a
    /// later phase-4 commit; kept here so the runner's public surface
    /// doesn't reshape when it does.
    #[allow(
        dead_code,
        reason = "consumed by the interactive ack path in a later phase 4 commit"
    )]
    pub(crate) fn ack(&mut self, name: &str, now: Instant) -> bool {
        let Some(script) = self.scripts.get(name) else {
            return false;
        };
        let interval = Duration::from_secs(script.interval_secs.max(1));
        if let Some(state) = self.state.get_mut(name) {
            state.ack_until = Some(now + interval);
            true
        } else {
            false
        }
    }

    #[cfg(test)]
    pub(crate) fn state_for(&self, name: &str) -> Option<&CheckRuntimeState> {
        self.state.get(name)
    }

    #[cfg(test)]
    pub(crate) fn force_due(&mut self, name: &str, at: Instant) {
        if let Some(state) = self.state.get_mut(name) {
            state.next_due = at;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::NotificationShowSound;
    use std::path::PathBuf;

    fn config_with_script(
        name: &str,
        interval_secs: u64,
        debounce: u32,
        max_concurrent: usize,
    ) -> ChecksConfig {
        ChecksConfig {
            max_concurrent,
            min_tick_secs: 1,
            scripts: vec![ScriptCheck {
                name: name.into(),
                program: PathBuf::from("/usr/bin/true"),
                interval_secs,
                timeout_secs: 5,
                debounce,
                on_fire: ActionSpec::Notify {
                    title: format!("{name} fired"),
                    sound: NotificationShowSound::default(),
                },
                ..ScriptCheck::default()
            }],
            ..ChecksConfig::default()
        }
    }

    #[test]
    fn debounce_counts_consecutive_fires_only() {
        let now = Instant::now();
        let mut runner = CheckRunner::from_config(&config_with_script("t", 60, 3, 4), now);

        // Two fires: below the debounce threshold — no decision.
        assert!(runner.complete("t", Outcome::Fire, now).is_none());
        assert!(runner.complete("t", Outcome::Fire, now).is_none());
        // Third fire crosses the debounce — one FireDecision.
        let decision = runner
            .complete("t", Outcome::Fire, now)
            .expect("third fire crosses debounce");
        assert_eq!(decision.name, "t");
        assert!(decision.episode.starts_with("t:"));

        // Fourth fire in the same episode does NOT re-emit.
        assert!(runner.complete("t", Outcome::Fire, now).is_none());
    }

    #[test]
    fn pass_resets_debounce_counter_and_episode_flag() {
        let now = Instant::now();
        let mut runner = CheckRunner::from_config(&config_with_script("t", 60, 2, 4), now);
        runner.complete("t", Outcome::Fire, now);
        runner.complete("t", Outcome::Fire, now); // fires
        runner.complete("t", Outcome::Pass, now);
        assert_eq!(runner.state_for("t").unwrap().consecutive_fails, 0);
        assert!(!runner.state_for("t").unwrap().fired_this_episode);
        // A fresh streak must fire again (new episode).
        runner.complete("t", Outcome::Fire, now);
        let decision = runner
            .complete("t", Outcome::Fire, now)
            .expect("new episode must fire");
        assert!(decision.episode.starts_with("t:"));
    }

    #[test]
    fn error_does_not_advance_debounce() {
        let now = Instant::now();
        let mut runner = CheckRunner::from_config(&config_with_script("t", 60, 2, 4), now);
        runner.complete("t", Outcome::Fire, now);
        runner.complete("t", Outcome::Error("boom".into()), now);
        // The error must not reset the counter to zero...
        assert_eq!(runner.state_for("t").unwrap().consecutive_fails, 1);
        // ...but it must not advance it toward the debounce either — one
        // more Fire crosses (2), not this one (still at 1).
        let decision = runner.complete("t", Outcome::Fire, now);
        assert!(
            decision.is_some(),
            "one more Fire past the debounce should emit"
        );
    }

    #[test]
    fn ack_suppresses_fires_until_the_window_elapses() {
        let now = Instant::now();
        let mut runner = CheckRunner::from_config(&config_with_script("t", 60, 1, 4), now);
        assert!(runner.ack("t", now));
        // Debounce of 1 means every Fire would normally emit; the ack
        // window suppresses it.
        assert!(runner.complete("t", Outcome::Fire, now).is_none());
        // Past the window (interval_secs = 60 → ack_until = now + 60s):
        // a Fire past the window emits.
        let past_window = now + Duration::from_secs(120);
        // The ack blocks the current fire above; a fresh episode starts.
        runner.complete("t", Outcome::Pass, now);
        let decision = runner.complete("t", Outcome::Fire, past_window);
        assert!(decision.is_some(), "past the ack window a fire must emit");
    }

    #[test]
    fn asleep_across_intervals_runs_once() {
        // Machine slept from `origin` to `origin + 3 * interval`. The runner
        // must run the check EXACTLY ONCE at the wake, not three times.
        let now = Instant::now();
        let mut runner = CheckRunner::from_config(&config_with_script("t", 10, 1, 4), now);
        let wake = now + Duration::from_secs(45);
        runner.force_due("t", now + Duration::from_secs(10));
        let runnable = runner.next_runnable(wake);
        assert_eq!(
            runnable.len(),
            1,
            "asleep-across-intervals must dispatch once"
        );
        // Complete it and check that next_due jumps to wake + interval, not
        // to origin + 4*interval (which would re-fire immediately).
        runner.complete("t", Outcome::Fire, wake);
        let state = runner.state_for("t").unwrap();
        assert_eq!(state.next_due, wake + Duration::from_secs(10));
        assert!(!state.in_flight);
        // At wake+eps the check is NOT due again — proving no naïve replay.
        assert!(runner
            .next_runnable(wake + Duration::from_millis(50))
            .is_empty());
    }

    #[test]
    fn max_concurrent_respected() {
        // Three ready checks, max_concurrent=2 → dispatch two now, one on
        // the next call after a completion frees a slot.
        let now = Instant::now();
        let config = ChecksConfig {
            max_concurrent: 2,
            min_tick_secs: 1,
            scripts: (0..3)
                .map(|i| ScriptCheck {
                    name: format!("s{i}"),
                    program: PathBuf::from("/usr/bin/true"),
                    interval_secs: 10,
                    timeout_secs: 5,
                    debounce: 1,
                    on_fire: ActionSpec::Event {
                        label: format!("e{i}"),
                    },
                    ..ScriptCheck::default()
                })
                .collect(),
            ..ChecksConfig::default()
        };
        // Silence the diagnostics check (already-clean shape); no diag runs
        // here.
        let _ = config.diagnostics();

        let mut runner = CheckRunner::from_config(&config, now);
        for name in ["s0", "s1", "s2"] {
            runner.force_due(name, now);
        }
        let batch = runner.next_runnable(now);
        assert_eq!(batch.len(), 2, "cap must gate dispatch to max_concurrent");
        assert_eq!(runner.in_flight(), 2);
        // No more slots: another call yields nothing until a completion.
        assert!(runner.next_runnable(now).is_empty());
        // Complete one; the freed slot lets the third go.
        runner.complete(&batch[0].name, Outcome::Pass, now);
        assert_eq!(runner.in_flight(), 1);
        let batch2 = runner.next_runnable(now);
        assert_eq!(batch2.len(), 1);
    }

    #[test]
    fn next_due_returns_earliest_free_deadline() {
        let now = Instant::now();
        let config = ChecksConfig {
            max_concurrent: 4,
            min_tick_secs: 1,
            scripts: vec![
                ScriptCheck {
                    name: "a".into(),
                    program: PathBuf::from("/usr/bin/true"),
                    interval_secs: 30,
                    timeout_secs: 5,
                    debounce: 1,
                    ..ScriptCheck::default()
                },
                ScriptCheck {
                    name: "b".into(),
                    program: PathBuf::from("/usr/bin/true"),
                    interval_secs: 60,
                    timeout_secs: 5,
                    debounce: 1,
                    ..ScriptCheck::default()
                },
            ],
            ..ChecksConfig::default()
        };
        let _ = config.diagnostics();
        let mut runner = CheckRunner::from_config(&config, now);
        runner.force_due("a", now + Duration::from_secs(5));
        runner.force_due("b", now + Duration::from_secs(2));
        assert_eq!(runner.next_due(now), Some(now + Duration::from_secs(2)));
        // In-flight checks are excluded from next_due — the App's tick
        // won't wake for a check whose outcome hasn't arrived yet.
        runner.force_due("b", now);
        let _ = runner.next_runnable(now);
        assert_eq!(runner.next_due(now), Some(now + Duration::from_secs(5)));
    }
}
