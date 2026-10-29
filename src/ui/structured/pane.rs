//! Pure seven-region composition (ADR-0036 §10); public placement/input is S10d.
use super::{
    controls::{self, Panel},
    markdown::safe,
    transcript::{self, Transcript},
};
use crate::structured::queue::model::State;
use crate::structured::view::contracts::{Action, Intent, Replay, Snapshot};
use ratatui::{
    layout::{
        Constraint::{Length, Min},
        Layout, Rect,
    },
    style::Style,
    widgets::{Block, Borders},
    Frame,
};

/// Prepared native view. Four-row consent/composer regions stay separate from output.
/// Metadata is clipped to its own rows; exact permission inspection remains an intent.
/// Small panes omit non-fitting controls, retaining the Deny-first policy from S10c2a.
pub(crate) struct Pane {
    pub(in crate::ui) area: Rect,
    pub(in crate::ui) border: Style,
    target: crate::structured::view::Target,
    panels: [Panel; 3],
    transcript: Transcript,
}
impl Pane {
    /// Consume one immutable cut. No cursor progress, draft edits, I/O or authority.
    pub fn compute(s: &Snapshot, area: Rect, border: Style) -> Self {
        let inner = Block::default().borders(Borders::ALL).inner(area);
        let [header, history, body, status, permission, queue, composer] = Layout::vertical([
            Length(3),
            Length(1),
            Min(1),
            Length(2),
            Length(4),
            Length(1),
            Length(4),
        ])
        .areas(inner);
        let mut core = Panel::default();
        let [identity, model, mode] = Layout::vertical([Length(1); 3]).areas(header);
        core.text(
            identity,
            format!(
                "{} | {} | structured",
                label(s.agent.name.as_deref().unwrap_or(&s.agent.agent_id)),
                label(&s.agent.harness)
            ),
        );
        core.text(
            model,
            format!(
                "Model: {} (requested: {})",
                label(s.reported_model.as_deref().unwrap_or("unknown")),
                label(s.agent.model.as_deref().unwrap_or("provider"))
            ),
        );
        core.text(
            mode,
            format!(
                "Mode: {} (requested: {})",
                label(s.reported_permissions.as_deref().unwrap_or("unknown")),
                match s.requested_permissions {
                    crate::structured::launch::PermissionMode::Default => "default",
                }
            ),
        );
        let [indicator, history_actions] = Layout::horizontal([Length(12), Min(0)]).areas(history);
        core.text(
            indicator,
            if s.new_output {
                "New output"
            } else {
                "Transcript"
            }
            .into(),
        );
        let mut buttons = vec![("[Follow tail]", Action::FollowTail)];
        if s.earlier_history.is_some() {
            buttons.push(("[Earlier history]", Action::EarlierHistory));
        }
        core.buttons(history_actions, buttons);
        let [phase, usage] = Layout::vertical([Length(1); 2]).areas(status);
        core.text(
            phase,
            format!(
                "Status: {} | turn: {} | bg: {}",
                phase_label(s.agent.state.phase),
                match s.agent.state.turn {
                    crate::api::agent_state::Turn::Working => "working",
                    crate::api::agent_state::Turn::Idle => "idle",
                    crate::api::agent_state::Turn::Unknown => "unknown",
                },
                match s.agent.state.background {
                    crate::api::agent_state::Background::None => "none".into(),
                    crate::api::agent_state::Background::Running { count } =>
                        format!("running ({count})"),
                    crate::api::agent_state::Background::Unknown => "unknown".into(),
                }
            ),
        );
        let (input, output) = s
            .usage
            .as_ref()
            .map(|u| (u.input_tokens, u.output_tokens))
            .unwrap_or_default();
        let replay = match s.replay {
            Replay::Replaying { .. } => "replay",
            Replay::Synced { .. } => "synced",
            Replay::Lagged { .. } => "lagged",
            Replay::Ended => "ended",
        };
        core.text(
            usage,
            format!(
                "Tokens I/O: {}/{} | age(ms):{} | #{} {replay}",
                observed(input),
                observed(output),
                observed(s.agent.state.held_ms),
                s.applied_cursor
            ),
        );
        let count = |state| s.queue.iter().filter(|i| i.state == state).count();
        core.text(
            queue,
            format!(
                "Queue: {} queued for idle | {} held",
                count(State::Queued),
                count(State::Held)
            ),
        );
        Self {
            target: s.target.clone(),
            area,
            border,
            panels: [
                core,
                controls::permission(s, permission),
                controls::composer(s, composer),
            ],
            transcript: transcript::compute(&s.transcript, &s.collapsed, s.scroll_anchor, body),
        }
    }
    /// All actions come from the same geometry used to paint; dispatch revalidates target.
    pub fn hit(&self, x: u16, y: u16) -> Option<&Action> {
        self.panels
            .iter()
            .find_map(|p| p.hit(x, y))
            .or_else(|| self.transcript.hit(x, y))
    }
    /// Consume this prepared cut into one intent with its original target generation.
    /// Misses and omitted buttons yield nothing. No authority is conferred: the controller
    /// must revalidate generation, permission handles and confirmation before any effect.
    pub fn into_intent(self, x: u16, y: u16) -> Option<Intent> {
        let action = self
            .panels
            .into_iter()
            .flat_map(Panel::into_hits)
            .chain(self.transcript.into_hits())
            .find(|(rect, _)| rect.contains((x, y).into()))
            .map(|(_, action)| action)?;
        Some(Intent {
            target: self.target,
            action,
        })
    }
    /// Enter the native pane paint seam without requiring a terminal runtime.
    pub fn render(&self, f: &mut Frame) {
        crate::ui::panes::render_structured(f, self);
    }
    /// Paint prepared leaf components after the native dispatch draws the border.
    pub(in crate::ui) fn paint_content(&self, f: &mut Frame) {
        self.transcript.render(f);
        for panel in &self.panels {
            panel.render(f);
        }
    }
}
fn phase_label(phase: crate::api::agent_state::Phase) -> &'static str {
    use crate::api::agent_state::Phase;
    match phase {
        Phase::Gone => "gone",
        Phase::Hibernated => "hibernated",
        Phase::Offline => "offline",
        Phase::LimitReached => "limit reached",
        Phase::Blocked => "blocked",
        Phase::NeedsHuman => "needs human",
        Phase::Working => "working",
        Phase::WaitingOnEvent => "waiting on event",
        Phase::FinishedWithBackground => "finished with background",
        Phase::Finished => "finished",
        Phase::Settling => "settling",
        Phase::Unknown => "unknown",
    }
}
fn observed(value: Option<u64>) -> String {
    value.map_or_else(|| "unknown".into(), |v| v.to_string())
}
fn label(value: &str) -> String {
    safe(value).replace('\n', "\\n")
}
#[cfg(test)]
mod tests;
