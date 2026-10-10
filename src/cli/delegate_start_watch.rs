//! Conservative, continuous evidence that a delegated turn never started.
use std::time::{Duration, Instant};

use crate::api::schema::AgentStatus;
use crate::app::guarded_submit::{composer_contents, composer_unfaint, Composer};
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

    #[cfg(test)]
    pub(super) fn observe(
        &mut self,
        cursor: &Cursor,
        status: AgentStatus,
        screen: &str,
        now: Instant,
    ) -> bool {
        self.observe_unfaint(cursor, status, screen, screen, now)
    }

    /// `unfaint` is `screen` with faint cells blanked, so a Claude box showing
    /// only its own suggestion reads as empty (#892).
    pub(super) fn observe_unfaint(
        &mut self,
        cursor: &Cursor,
        status: AgentStatus,
        screen: &str,
        unfaint: &str,
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
            .is_some_and(|agent| composer_unfaint(agent, screen, unfaint, "") == Composer::Empty);
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
            .and_then(|agent| composer_contents(agent, screen, unfaint));
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

    /// A real Claude 2.1.295 box after a lost paste: its `Try "…"` suggestion
    /// painted faint (SGR 2), and the same words typed (#892).
    fn claude_box(typed: &str) -> crate::terminal::TerminalRuntime {
        let rule = "─".repeat(40);
        let bytes = format!("\x1b[2J\x1b[H{rule}\r\n❯ {typed}\r\n{rule}\r\n? for shortcuts");
        crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
            80,
            24,
            65_536,
            bytes.as_bytes(),
        )
    }

    /// Answer reads the way the server does, from one styled read.
    fn serve(
        pane: &crate::terminal::TerminalRuntime,
    ) -> impl FnMut(
        crate::api::schema::Method,
        Option<Instant>,
    ) -> Result<serde_json::Value, super::super::BoundedError>
           + '_ {
        |method, _| {
            let crate::api::schema::Method::PaneRead(params) = method else {
                panic!("expected pane read")
            };
            assert_eq!(
                params.source,
                crate::api::schema::ReadSource::DetectionUnfaint
            );
            let (text, unfaint) = pane.detection_text_and_unfaint();
            Ok(serde_json::json!({"result": {"read": {"text": text, "unfaint": unfaint}}}))
        }
    }

    /// Whether two samples a full window apart report NOT_STARTED.
    fn not_started(
        read: &mut dyn FnMut(
            crate::api::schema::Method,
            Option<Instant>,
        ) -> Result<serde_json::Value, super::super::BoundedError>,
    ) -> bool {
        let now = Instant::now();
        let mut watch = StartWatch::new(
            Some(cursor()),
            Some(Agent::Claude),
            now,
            Duration::from_secs(5),
        );
        [now, now + Duration::from_secs(35)].into_iter().any(|at| {
            let (screen, unfaint) =
                super::super::detection_screens_with("fixture:p1", None, &mut *read).expect("read");
            watch.observe_unfaint(&cursor(), AgentStatus::Idle, &screen, &unfaint, at)
        })
    }

    #[tokio::test]
    async fn a_claude_box_showing_only_its_faint_suggestion_is_not_started() {
        let pane = claude_box("\x1b[2mTry \"refactor check-ssot.sh\"\x1b[0m");
        assert!(not_started(&mut serve(&pane)));
        let pane = claude_box("\x1b[2mwatch CI on #891 and merge when green\x1b[0m");
        assert!(not_started(&mut serve(&pane)));
    }

    #[tokio::test]
    async fn a_claude_box_holding_typed_text_is_never_not_started() {
        let pane = claude_box("Try \"refactor check-ssot.sh\"");
        assert!(!not_started(&mut serve(&pane)));
    }

    #[tokio::test]
    async fn a_server_without_the_unfaint_read_falls_back_to_the_plain_screen() {
        let pane = claude_box("\x1b[2mTry \"refactor check-ssot.sh\"\x1b[0m");
        let mut sources = Vec::new();
        let mut old_server = |method, _| {
            let crate::api::schema::Method::PaneRead(params) = method else {
                panic!("expected pane read")
            };
            sources.push(params.source);
            Ok(match params.source {
                crate::api::schema::ReadSource::Detection => {
                    serde_json::json!({"result": {"read": {"text": pane.detection_text()}}})
                }
                _ => serde_json::json!({"error": {"code": "invalid_params"}}),
            })
        };
        let (screen, unfaint) =
            super::super::detection_screens_with("fixture:p1", None, &mut old_server)
                .expect("read");
        assert_eq!(screen, pane.detection_text());
        assert_eq!(unfaint, screen);
        assert_eq!(
            sources,
            [
                crate::api::schema::ReadSource::DetectionUnfaint,
                crate::api::schema::ReadSource::Detection
            ]
        );
    }
}
