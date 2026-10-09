//! Conservative, continuous evidence that a delegated turn never started.
use std::time::{Duration, Instant};

use crate::api::schema::AgentStatus;
use crate::app::guarded_submit::{composer, composer_contents, Composer};
use crate::detect::Agent;

use super::super::settled::Cursor;

/// Cold Codex startup can queue a prompt for roughly twenty seconds.
const STARTUP_ALLOWANCE: Duration = Duration::from_secs(30);

pub(super) struct StartWatch {
    baseline: Option<Cursor>,
    agent: Option<Agent>,
    submitted: Instant,
    window: Duration,
    empty_since: Option<Instant>,
    contents: Option<String>,
    state_seq: Option<u64>,
}

impl StartWatch {
    pub(super) fn new(
        baseline: Option<Cursor>,
        agent: Option<Agent>,
        submitted: Instant,
        settle: Duration,
    ) -> Self {
        Self {
            baseline: baseline.filter(|cursor| !cursor.working),
            agent,
            submitted,
            window: STARTUP_ALLOWANCE.saturating_add(settle),
            empty_since: None,
            contents: None,
            state_seq: None,
        }
    }

    pub(super) fn interrupted(&mut self) {
        self.empty_since = None;
        self.contents = None;
        self.state_seq = None;
    }

    pub(super) fn observe(
        &mut self,
        cursor: &Cursor,
        status: AgentStatus,
        screen: &str,
        now: Instant,
    ) -> bool {
        let Some(before) = &self.baseline else {
            return false;
        };
        // A turn or a different execution permanently invalidates this diagnosis.
        if status == AgentStatus::Working
            || cursor.working
            || before.terminal_id != cursor.terminal_id
            || before.epoch != cursor.epoch
            || before.entries != cursor.entries
        {
            self.baseline = None;
            self.interrupted();
            return false;
        }
        let empty = self
            .agent
            .is_some_and(|agent| composer(agent, screen, "") == Composer::Empty);
        let pending = screen.lines().any(|line| {
            let line = line.trim().to_ascii_lowercase();
            line.contains("waiting for startup")
                || line.contains("tab to queue message")
                || line.starts_with("• queued")
                || line.starts_with("• messages to be submitted after next tool call")
                || line.starts_with("queued:")
        });
        if !super::idle_without_new_turn(before, cursor, status) || !empty || pending {
            self.interrupted();
            return false;
        }
        let contents = self
            .agent
            .and_then(|agent| composer_contents(agent, screen));
        if contents != self.contents || self.state_seq != Some(cursor.seq) {
            self.empty_since = None;
            self.contents = contents;
            self.state_seq = Some(cursor.seq);
        }
        // A late `wait` cannot infer uninterrupted idle from time spent unobserved.
        // The submit timestamp is a lower bound, never a substitute for samples.
        let since = *self.empty_since.get_or_insert(now.max(self.submitted));
        now.saturating_duration_since(since) >= self.window
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY: &str = "› Ask Codex to do anything\n? for shortcuts";

    fn cursor() -> Cursor {
        Cursor::parse("term_fixture:0:0:0:i").unwrap()
    }

    fn watch(now: Instant) -> StartWatch {
        StartWatch::new(
            Some(cursor()),
            Some(Agent::Codex),
            now,
            Duration::from_secs(5),
        )
    }

    #[test]
    fn delegate_not_started_requires_continuous_empty_window_after_submit() {
        let now = Instant::now();
        let mut watch = watch(now);
        assert!(!watch.observe(&cursor(), AgentStatus::Idle, EMPTY, now));
        assert!(!watch.observe(
            &cursor(),
            AgentStatus::Idle,
            EMPTY,
            now + Duration::from_millis(34999)
        ));
        assert!(watch.observe(
            &cursor(),
            AgentStatus::Idle,
            EMPTY,
            now + Duration::from_secs(35)
        ));
        // An old registry timestamp is not evidence about an unobserved interval.
        let mut late = StartWatch::new(
            Some(cursor()),
            Some(Agent::Codex),
            now - Duration::from_secs(60),
            Duration::from_secs(5),
        );
        assert!(!late.observe(&cursor(), AgentStatus::Idle, EMPTY, now));
        assert!(!late.observe(
            &cursor(),
            AgentStatus::Idle,
            EMPTY,
            now + Duration::from_secs(34)
        ));
        assert!(late.observe(
            &cursor(),
            AgentStatus::Idle,
            EMPTY,
            now + Duration::from_secs(35)
        ));
    }

    #[test]
    fn delegate_idle_then_working_cancels_not_started_even_with_late_counter() {
        let now = Instant::now();
        let mut watch = watch(now);
        assert!(!watch.observe(&cursor(), AgentStatus::Idle, EMPTY, now));
        assert!(!watch.observe(
            &cursor(),
            AgentStatus::Idle,
            EMPTY,
            now + Duration::from_secs(20)
        ));
        assert!(!watch.observe(
            &cursor(),
            AgentStatus::Working,
            EMPTY,
            now + Duration::from_secs(21)
        ));
        assert!(!watch.observe(
            &cursor(),
            AgentStatus::Idle,
            EMPTY,
            now + Duration::from_secs(100)
        ));
    }

    #[test]
    fn delegate_queued_startup_or_composer_change_resets_not_started() {
        let now = Instant::now();
        for pending in [
            "Waiting for startup · esc cancel\n› Ask Codex to do anything\n? for shortcuts",
            "• Queued follow-up inputs\n  build it\n› Ask Codex to do anything\n? for shortcuts",
            "• Messages to be submitted after next tool call\n  build it\n› Ask Codex to do anything\n? for shortcuts",
            "› build it\n? for shortcuts",
            "› Ask Codex to do anything\n? for shortcuts · tab to queue message",
            "›\n? for shortcuts",
        ] {
            let mut watch = watch(now);
            assert!(!watch.observe(&cursor(), AgentStatus::Idle, EMPTY, now));
            assert!(!watch.observe(&cursor(), AgentStatus::Idle, pending, now + Duration::from_secs(34)));
            assert!(!watch.observe(&cursor(), AgentStatus::Idle, EMPTY, now + Duration::from_secs(35)));
            assert!(!watch.observe(&cursor(), AgentStatus::Idle, EMPTY, now + Duration::from_secs(69)));
            assert!(watch.observe(&cursor(), AgentStatus::Idle, EMPTY, now + Duration::from_secs(70)));
        }
    }

    #[test]
    fn delegate_queued_during_startup_then_turn_never_reports_not_started() {
        let now = Instant::now();
        let mut watch = watch(now);
        let queued = "Waiting for startup · esc cancel\n› build it\n? for shortcuts";
        for second in [0, 20, 40] {
            assert!(!watch.observe(
                &cursor(),
                AgentStatus::Idle,
                queued,
                now + Duration::from_secs(second)
            ));
        }
        let started = Cursor::parse("term_fixture:0:1:2:i").unwrap();
        assert!(!watch.observe(
            &started,
            AgentStatus::Idle,
            EMPTY,
            now + Duration::from_secs(41)
        ));
        assert!(!watch.observe(
            &started,
            AgentStatus::Idle,
            EMPTY,
            now + Duration::from_secs(100)
        ));
    }

    #[test]
    fn delegate_prompt_hook_before_working_gets_startup_allowance() {
        // The prompt hook confirms submission without incrementing working entries.
        let submitted = Instant::now();
        let mut watch = watch(submitted);
        assert!(!watch.observe(
            &cursor(),
            AgentStatus::Idle,
            EMPTY,
            submitted + Duration::from_millis(200)
        ));
        let working = Cursor::parse("term_fixture:0:1:1:w").unwrap();
        assert!(!watch.observe(
            &working,
            AgentStatus::Working,
            EMPTY,
            submitted + Duration::from_secs(1)
        ));
        assert!(!watch.observe(
            &cursor(),
            AgentStatus::Idle,
            EMPTY,
            submitted + Duration::from_secs(100)
        ));
    }

    #[test]
    fn delegate_sample_gaps_and_unknown_composer_reset_not_started() {
        let now = Instant::now();
        let mut watch = watch(now);
        assert!(!watch.observe(&cursor(), AgentStatus::Idle, EMPTY, now));
        watch.interrupted();
        assert!(!watch.observe(
            &cursor(),
            AgentStatus::Idle,
            EMPTY,
            now + Duration::from_secs(35)
        ));
        assert!(!watch.observe(
            &cursor(),
            AgentStatus::Idle,
            "›",
            now + Duration::from_secs(69)
        ));
        assert!(!watch.observe(
            &cursor(),
            AgentStatus::Idle,
            EMPTY,
            now + Duration::from_secs(70)
        ));
        assert!(watch.observe(
            &cursor(),
            AgentStatus::Idle,
            EMPTY,
            now + Duration::from_secs(105)
        ));
    }
    #[test]
    fn delegate_missed_state_transition_resets_not_started() {
        let now = Instant::now();
        let mut watch = watch(now);
        assert!(!watch.observe(&cursor(), AgentStatus::Idle, EMPTY, now));
        let changed = Cursor::parse("term_fixture:0:0:2:i").unwrap();
        assert!(!watch.observe(
            &changed,
            AgentStatus::Idle,
            EMPTY,
            now + Duration::from_secs(35)
        ));
        assert!(watch.observe(
            &changed,
            AgentStatus::Idle,
            EMPTY,
            now + Duration::from_secs(70)
        ));
    }
}
