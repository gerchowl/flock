//! The armed self-compaction: an agent asking to shorten its own context and
//! carry on (see #540).
//!
//! The whole feature is one sequence with two writes and one signal between
//! them, and the signal is the part that cannot be timed. An MCP tool call
//! happens *mid-turn*: the agent is running this code as part of a turn it has
//! not finished, and the harness it is running inside cannot run `/compact`
//! with a turn in flight. So nothing may be typed when the verb is called.
//!
//! ```text
//!   flock_self_compact(continuation)        ← mid-turn, stores, returns
//!   … the agent finishes its turn …
//!   [idle + settled + operator quiet]        ← flock types `/compact`
//!   … the harness compacts …
//!   SessionStart source=compact             ← flock types the continuation
//!   [submit gap]                            ← flock types Enter
//!   the agent's next turn, on a short context, holding its own handoff prompt
//! ```
//!
//! That middle signal is a hook report, not a clock: `session_start_source:
//! compact` is the harness telling flock, in its own words, that the
//! compaction happened. [`SelfCompactPhase::CompactRequested`] is therefore a
//! *wait*, and `arm_compact_timeout_ms` exists to bound it — see
//! [`SessionConfig::self_compact_timeout_ms`](crate::config::model::SessionConfig).
//!
//! Everything in this module is pure. The keystrokes live in
//! `app::self_compact`, which reads this state and asks these questions.

use std::time::{Duration, Instant};

/// Largest continuation prompt accepted, in bytes. The prompt is typed into a
/// pane, so it has to survive a paste, and a handoff prompt longer than this
/// is a document, not a handoff: the agent can put the document in a file and
/// name it in the prompt. Matches `agent.spawn`'s `MAX_PROMPT_BYTES` for the
/// same reason — text that crosses into a session is capped, not trusted to be
/// short.
pub const MAX_CONTINUATION_BYTES: usize = 16 * 1024;

/// The slash command that asks the harness to compact. Claude Code's, and
/// currently the only one: compaction is a harness affordance, so the text is a
/// constant here rather than something an agent gets to choose. An agent that
/// named its own command would be typing arbitrary bytes into its own prompt
/// box, which is exactly what keeping `pane.send_*` off MCP prevents.
pub const COMPACT_COMMAND: &str = "/compact";

/// Where an armed compaction has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfCompactPhase {
    /// Stored, nothing typed. Waiting for this agent's turn to end.
    Armed,
    /// `/compact` has been typed and submitted. Waiting for the harness to
    /// report `session_start_source: compact` back.
    CompactRequested,
    /// The continuation prompt has been typed; its Enter is not due yet.
    ContinueTyped,
}

impl SelfCompactPhase {
    /// Whether the compaction has reached the point where the harness is doing
    /// something we cannot see. Only [`SelfCompactPhase::CompactRequested`]
    /// and [`SelfCompactPhase::ContinueTyped`] have written into the pane, so
    /// only these need the timeout.
    pub fn has_written(self) -> bool {
        !matches!(self, SelfCompactPhase::Armed)
    }
}

/// One armed self-compaction, stored against a pane.
#[derive(Debug, Clone)]
pub struct ArmedSelfCompact {
    /// The agent's own handoff prompt. Held verbatim — never reworded,
    /// trimmed, or summarised — because the whole point is that this is the
    /// agent's words rather than a human's transcription of them.
    pub continuation: String,
    pub phase: SelfCompactPhase,
    /// When the verb was called.
    pub armed_at: Instant,
    /// When the most recent keystroke went out. The submit gap for the *next*
    /// Enter is measured from this, so the two writes of one submission are
    /// always a gap apart and never one read.
    pub typed_at: Option<Instant>,
    /// When the pending Enter is due; `None` once it has been sent.
    pub submit_at: Option<Instant>,
}

impl ArmedSelfCompact {
    pub fn new(continuation: String, now: Instant) -> Self {
        Self {
            continuation,
            phase: SelfCompactPhase::Armed,
            armed_at: now,
            typed_at: None,
            submit_at: None,
        }
    }

    /// Record that `/compact` went out. `submit_gap` is the same gap
    /// `pane run` and the idle wake use: text and a carriage return in one
    /// read are a paste with a newline in it, not a submitted command.
    pub fn compact_requested(&mut self, now: Instant, submit_gap: Duration) {
        self.phase = SelfCompactPhase::CompactRequested;
        self.typed_at = Some(now);
        self.submit_at = Some(now + submit_gap);
    }

    /// Record that the continuation prompt went out. The compaction has
    /// already happened, so the gap here is what keeps the paste from
    /// swallowing its own Enter.
    pub fn continuation_typed(&mut self, now: Instant, submit_gap: Duration) {
        self.phase = SelfCompactPhase::ContinueTyped;
        self.typed_at = Some(now);
        self.submit_at = Some(now + submit_gap);
    }

    /// The pending Enter is due. The *same* last-typed timestamp is the
    /// comparison the idle wake makes, so a pane a human has touched since
    /// flock typed is refused rather than submitted over.
    pub fn submit_due(&self, now: Instant) -> bool {
        self.submit_at.is_some_and(|due| now >= due)
    }

    /// Why this arming is giving up, if it is.
    ///
    /// Two ways out, and the distinction matters to whoever armed it. A
    /// compaction that timed out having written nothing lost nothing: the
    /// agent's prompt box is untouched and its handoff prompt is still in the
    /// transcript to copy from. One that timed out *after* typing `/compact`
    /// may have left that command sitting in the box, which is why the caller
    /// is told which of the two happened.
    pub fn timeout_reason(&self, now: Instant, timeout: Duration) -> Option<&'static str> {
        // Matched on the phase rather than on `has_written`, because the two
        // halves have to be told apart to say which one was lost — and only
        // one of the two written phases is still waiting on the harness.
        if now.saturating_duration_since(self.armed_at) < timeout {
            return None;
        }
        match self.phase {
            SelfCompactPhase::Armed => None,
            SelfCompactPhase::CompactRequested => Some("compact_timeout"),
            SelfCompactPhase::ContinueTyped => Some("continuation_timeout"),
        }
    }
}

/// Why a compaction arming was dropped. Every value is a case where flock chose
/// not to type into a pane, so they are all worth a log line and none of them
/// is worth a user-facing notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfCompactAbort {
    /// The pane is gone.
    PaneGone,
    /// The agent left `Idle` between the write and its Enter.
    LeftIdle,
    /// A human typed into the pane after flock did. The sentence stays in the
    /// box, which is recoverable; submitting a prompt someone was editing is
    /// not.
    OperatorActive,
    /// The harness reported a session start that was not a compaction, so the
    /// `/compact` we typed did not do what it was supposed to (the operator
    /// cancelled it, or the harness started a new session instead).
    UnexpectedSessionStart,
    /// The bound above elapsed.
    Timeout(&'static str),
    /// The write itself failed.
    WriteFailed,
}

impl SelfCompactAbort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PaneGone => "pane_gone",
            Self::LeftIdle => "left_idle",
            Self::OperatorActive => "operator_active",
            Self::UnexpectedSessionStart => "unexpected_session_start",
            Self::Timeout(reason) => reason,
            Self::WriteFailed => "write_failed",
        }
    }
}

/// Whether an armed compaction may fire on an agent of this kind.
///
/// Claude only, and for the same reason the idle wake is: compaction is a
/// harness affordance reached by a harness-specific slash command, and typing
/// `/compact` into an agent that has no such command would put a stray line in
/// a prompt box. An agent that cannot compact itself is refused by name at
/// arm time rather than being armed and then silently doing nothing, so the
/// agent learns why instead of waiting for a continuation that never comes.
pub fn agent_can_self_compact(agent: Option<crate::detect::Agent>) -> bool {
    matches!(agent, Some(crate::detect::Agent::Claude))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn armed() -> ArmedSelfCompact {
        ArmedSelfCompact::new("carry on".to_string(), Instant::now())
    }

    #[test]
    fn a_fresh_arming_has_written_nothing() {
        let armed = armed();
        assert_eq!(armed.phase, SelfCompactPhase::Armed);
        assert!(!armed.phase.has_written());
        assert!(armed.typed_at.is_none());
        assert!(armed.submit_at.is_none());
    }

    #[test]
    fn submit_is_never_due_before_the_gap_elapses() {
        let now = Instant::now();
        let mut armed = ArmedSelfCompact::new("x".to_string(), now);
        armed.compact_requested(now, Duration::from_millis(50));
        assert!(!armed.submit_due(now + Duration::from_millis(49)));
        assert!(armed.submit_due(now + Duration::from_millis(50)));
    }

    #[test]
    fn a_sent_enter_is_not_due_again() {
        let now = Instant::now();
        let mut armed = ArmedSelfCompact::new("x".to_string(), now);
        armed.compact_requested(now, Duration::from_millis(10));
        assert!(armed.submit_due(now + Duration::from_millis(10)));
        armed.submit_at = None;
        assert!(!armed.submit_due(now + Duration::from_secs(60)));
    }

    #[test]
    fn an_arming_that_wrote_nothing_never_times_out() {
        let now = Instant::now();
        let armed = ArmedSelfCompact::new("x".to_string(), now);
        assert_eq!(armed.timeout_reason(now, Duration::from_secs(1)), None);
        assert_eq!(
            armed.timeout_reason(now + Duration::from_secs(3_600), Duration::from_secs(1)),
            None,
            "an agent that armed and never reached idle must stay armed, \
             not be silently dropped — it may be mid-turn for minutes"
        );
    }

    #[test]
    fn the_timeout_names_which_half_was_lost() {
        let now = Instant::now();
        let mut armed = ArmedSelfCompact::new("x".to_string(), now);
        armed.compact_requested(now, Duration::from_millis(1));
        assert_eq!(
            armed.timeout_reason(now + Duration::from_secs(2), Duration::from_secs(1)),
            Some("compact_timeout")
        );
        armed.continuation_typed(now, Duration::from_millis(1));
        assert_eq!(
            armed.timeout_reason(now + Duration::from_secs(2), Duration::from_secs(1)),
            Some("continuation_timeout")
        );
    }

    #[test]
    fn only_claude_can_self_compact() {
        assert!(agent_can_self_compact(Some(crate::detect::Agent::Claude)));
        assert!(!agent_can_self_compact(Some(
            crate::detect::Agent::OpenCode
        )));
        assert!(!agent_can_self_compact(None));
    }

    #[test]
    fn every_abort_reason_has_a_stable_name() {
        for (abort, name) in [
            (SelfCompactAbort::PaneGone, "pane_gone"),
            (SelfCompactAbort::LeftIdle, "left_idle"),
            (SelfCompactAbort::OperatorActive, "operator_active"),
            (
                SelfCompactAbort::UnexpectedSessionStart,
                "unexpected_session_start",
            ),
            (
                SelfCompactAbort::Timeout("compact_timeout"),
                "compact_timeout",
            ),
            (SelfCompactAbort::WriteFailed, "write_failed"),
        ] {
            assert_eq!(abort.as_str(), name);
        }
    }
}
