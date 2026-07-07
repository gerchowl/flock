use crate::detect::{Agent, AgentState};
use crate::terminal::TerminalId;

/// A background completion (`Working|Blocked → Idle` while the user is elsewhere)
/// that has been observed at its edge but not yet committed to the attention
/// surface. Held for `ATTENTION_SETTLE` so a pane that flaps straight back to
/// Working never lights the `●` (or plays the completion sound / toast). Carries
/// the edge context needed to reproduce those effects at commit time (#130).
pub struct CompletionPending {
    pub previous_state: AgentState,
    pub known_agent: Option<Agent>,
    pub agent_label: Option<String>,
}

/// Viewport state for a pane.
///
/// Terminal identity, cwd, labels, and agent metadata live in TerminalState.
pub struct PaneState {
    pub attached_terminal_id: TerminalId,
    /// Whether the user has seen this pane since its last state change to Idle.
    /// False = "Done" (agent finished while user was in another workspace).
    pub seen: bool,
    /// A background completion awaiting its settle window before it re-arms
    /// `seen` and fires the completion sound/toast (#130). `Some` from the
    /// `→Idle` edge until either the settle elapses (commit) or the pane leaves
    /// Idle (cancel). `seen` stays a plain bool for the sidebar sort key, so the
    /// settle never makes the #102 frozen ordering time-varying.
    pub completion_pending: Option<CompletionPending>,
}

impl PaneState {
    pub fn new(attached_terminal_id: TerminalId) -> Self {
        Self {
            attached_terminal_id,
            seen: true,
            completion_pending: None,
        }
    }
}
