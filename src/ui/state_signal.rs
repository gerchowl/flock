//! Sidebar state-language SSoT (issue #42): ONE severity→color mapping and
//! ONE join function feeding every state rendering — the servers-band ring
//! medallion, the single-line packed rects, the leading state circles, and
//! the agents-panel icon colors.
//!
//! * [`StateClass`] — an agent state classified into the shared severity
//!   ladder. Its `Ord` IS the attention priority (consolidating the former
//!   `pane_attention_priority` / `workspace_attention_priority` copies), and
//!   [`StateClass::color`] is the one severity→color mapping.
//! * [`join_states`] — the severity-sorted top-3 multiset ("join") of a
//!   scope's agent states. Repetition is meaningful (`[red, red, yellow]` =
//!   two blocked + one working); fewer than three live states yield a
//!   shorter join; no live states yield an empty one.
//! * [`packed_rects`] / [`medallion_rings`] — the join's single-line and
//!   two-line renderings. The leading-circle color is simply the join head
//!   (== the existing aggregate state) through the same mapping.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

use crate::app::state::Palette;
use crate::detect::AgentState;

/// One agent state on the shared severity ladder, worst last so the derived
/// `Ord` doubles as the attention priority: blocked, then done-unseen, then
/// working, then settled idle, then none. Done-unseen outranks working on
/// purpose — a finished agent you have not looked at yet is the thing to
/// surface (the pre-existing aggregate semantics, kept).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum StateClass {
    /// No live agent signal — muted.
    None,
    /// Settled idle (seen) — green.
    Idle,
    /// Working — yellow.
    Working,
    /// Done: idle the user has not seen yet — the unseen accent (teal).
    Done,
    /// Blocked — red.
    Blocked,
}

impl StateClass {
    /// Classify a local agent state. Seen/unseen splits idle into the
    /// settled (green) and done-unseen (teal) classes, matching `state_dot`.
    pub(crate) fn of(state: AgentState, seen: bool) -> Self {
        match (state, seen) {
            (AgentState::Blocked, _) => Self::Blocked,
            (AgentState::Idle, false) => Self::Done,
            (AgentState::Working, _) => Self::Working,
            (AgentState::Idle, true) => Self::Idle,
            (AgentState::Unknown, _) => Self::None,
        }
    }

    /// Classify a federated peer's workspace status (the remote summaries
    /// carry an explicit `Done` instead of idle+unseen).
    pub(crate) fn of_remote(status: crate::api::schema::AgentStatus) -> Self {
        use crate::api::schema::AgentStatus;
        match status {
            AgentStatus::Blocked => Self::Blocked,
            AgentStatus::Done => Self::Done,
            AgentStatus::Working => Self::Working,
            AgentStatus::Idle => Self::Idle,
            AgentStatus::Unknown => Self::None,
        }
    }

    /// THE severity→color mapping: red blocked, yellow working, teal
    /// done-unseen, green settled idle, muted none. Every state rendering
    /// (medallion rings, packed rects, leading circles, agent icons, state
    /// labels) sources its color here.
    pub(crate) fn color(self, p: &Palette) -> Color {
        match self {
            Self::Blocked => p.red,
            Self::Done => p.teal,
            Self::Working => p.yellow,
            Self::Idle => p.green,
            Self::None => p.overlay0,
        }
    }

    /// Color for the per-state COUNT badges (sidebar group headers + servers
    /// band), where the number's color is the ONLY differentiator — the dot /
    /// checkmark icons elsewhere carry shape, so they keep `color` (idle green).
    /// In a bare count, idle's green blurs into done's teal, so idle reads as a
    /// muted grey here; blocked/done/working keep their signal colors.
    pub(crate) fn count_color(self, p: &Palette) -> Color {
        match self {
            Self::Idle => p.overlay1,
            other => other.color(p),
        }
    }
}

/// The join: a scope's live agent states as a severity-sorted multiset,
/// capped at the top three. The empty join means "no live agents".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct StateJoin(Vec<StateClass>);

/// Cap of the join: at most three classes render (medallion rings, packed
/// rect cells); the count below the cap itself signals scale.
const JOIN_CAP: usize = 3;

/// Build the join of `states`: drop `None` (no live signal), sort worst
/// first, keep the top [`JOIN_CAP`]. Repetition is meaningful.
pub(crate) fn join_states(states: impl IntoIterator<Item = StateClass>) -> StateJoin {
    let mut classes: Vec<StateClass> = states
        .into_iter()
        .filter(|class| *class != StateClass::None)
        .collect();
    classes.sort_unstable_by(|a, b| b.cmp(a));
    classes.truncate(JOIN_CAP);
    StateJoin(classes)
}

impl StateJoin {
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The join's classes, severity-sorted worst first (outer→inner).
    pub(crate) fn classes(&self) -> &[StateClass] {
        &self.0
    }

    /// The worst class present — the leading-circle color source. The live
    /// render path reaches it through `Workspace::aggregate_state` (whose
    /// max-by-`StateClass` IS the join head, shape-aware); this accessor
    /// states the equivalence and serves the tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn head(&self) -> Option<StateClass> {
        self.0.first().copied()
    }

    /// Ring/rect colors, outer→inner.
    pub(crate) fn colors(&self, p: &Palette) -> Vec<Color> {
        self.0.iter().map(|class| class.color(p)).collect()
    }
}

/// Full per-class tally of a server's agent states — the data behind the
/// band's count columns (`flock 0 1 2 1`, #42): four columns =
/// blocked / done / working / idle, each kept distinct (done stays teal, idle
/// reads muted-grey in a bare count). Unlike the capped [`StateJoin`], the
/// tally keeps exact counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct StateTally {
    pub(crate) blocked: usize,
    pub(crate) done: usize,
    pub(crate) working: usize,
    pub(crate) idle: usize,
}

pub(crate) fn tally_states(states: impl IntoIterator<Item = StateClass>) -> StateTally {
    let mut tally = StateTally::default();
    for class in states {
        match class {
            StateClass::Blocked => tally.blocked += 1,
            StateClass::Done => tally.done += 1,
            StateClass::Working => tally.working += 1,
            StateClass::Idle => tally.idle += 1,
            StateClass::None => {}
        }
    }
    tally
}

impl StateTally {
    /// Width in digits of the widest column — feeds the band-global digit
    /// alignment (one server hitting 10 widens every row to two digits).
    pub(crate) fn digit_width(&self) -> usize {
        [self.blocked, self.done, self.working, self.idle]
            .into_iter()
            .map(|n| n.max(1).ilog10() as usize + 1)
            .max()
            .unwrap_or(1)
    }

    /// The capped severity join equivalent to this tally (medallion mode).
    /// Done and idle both reduce to the calm `Idle` class for the join.
    pub(crate) fn join(&self) -> StateJoin {
        join_states(
            std::iter::repeat_n(StateClass::Blocked, self.blocked)
                .chain(std::iter::repeat_n(StateClass::Working, self.working))
                .chain(std::iter::repeat_n(StateClass::Idle, self.done + self.idle)),
        )
    }
}

/// The count columns for one band row (rendered after the padded name
/// field): four right-aligned counts — blocked / done / working / idle in
/// red / teal / yellow / muted-grey (idle reads grey in a bare count so it
/// never blurs into done's teal). Zeros muted, every column padded to the
/// band-global `digit_width`, each with a trailing separator space.
pub(crate) fn leading_count_spans(
    tally: &StateTally,
    digit_width: usize,
    ghosted: bool,
    p: &Palette,
) -> Vec<Span<'static>> {
    let column = |count: usize, color: Color| {
        let style = if ghosted {
            // Unreachable rows ghost their last-known counts with the rest
            // of the line: muted + italic, no state colors.
            Style::default()
                .fg(p.overlay0)
                .add_modifier(Modifier::ITALIC)
        } else if count == 0 {
            Style::default().fg(p.overlay0).add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(color)
        };
        Span::styled(format!("{count:>digit_width$} "), style)
    };
    vec![
        column(tally.blocked, StateClass::Blocked.count_color(p)),
        column(tally.done, StateClass::Done.count_color(p)),
        column(tally.working, StateClass::Working.count_color(p)),
        column(tally.idle, StateClass::Idle.count_color(p)),
    ]
}

/// Packed-rect glyphs of the single-line join rendering.
pub(crate) const RECT_FILLED: &str = "\u{25ae}";
pub(crate) const RECT_HOLLOW: &str = "\u{25af}";

/// The join's single-line rendering: one to three closely-packed `▮` cells,
/// severity-sorted; a single hollow muted `▯` when no live agents exist.
pub(crate) fn packed_rects(join: &StateJoin, p: &Palette) -> Vec<Span<'static>> {
    if join.is_empty() {
        return vec![Span::styled(
            RECT_HOLLOW,
            Style::default().fg(StateClass::None.color(p)),
        )];
    }
    join.classes()
        .iter()
        .map(|class| Span::styled(RECT_FILLED, Style::default().fg(class.color(p))))
        .collect()
}

/// The join's medallion rings (outer→inner) for a reachable two-line server
/// row. An empty join still marks presence: a single muted ring ("none" on
/// the severity ladder) rather than a blank.
pub(crate) fn medallion_rings(join: &StateJoin, p: &Palette) -> Vec<Color> {
    if join.is_empty() {
        return vec![StateClass::None.color(p)];
    }
    join.colors(p)
}

/// Compact PR-state glyph + color, shared by the pane-header HUD and the
/// sidebar workspace rows: `⊙` open, `◐` draft, `✓` merged, `✗` closed.
pub(crate) fn pr_state_glyph(
    state: crate::worktree::PrState,
    p: &Palette,
) -> (&'static str, Color) {
    use crate::worktree::PrState;
    match state {
        PrState::Open => ("\u{2299}", p.accent),
        PrState::Draft => ("\u{25d0}", p.overlay0),
        PrState::Merged => ("\u{2713}", p.mauve),
        PrState::Closed => ("\u{2717}", p.red),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> Palette {
        crate::app::state::AppState::test_new().palette
    }

    #[test]
    fn severity_order_is_blocked_done_working_idle_none() {
        assert!(StateClass::Blocked > StateClass::Done);
        assert!(StateClass::Done > StateClass::Working);
        assert!(StateClass::Working > StateClass::Idle);
        assert!(StateClass::Idle > StateClass::None);
    }

    #[test]
    fn mapping_classifies_and_colors_every_state() {
        let p = palette();
        for (state, seen, class, color) in [
            (AgentState::Blocked, true, StateClass::Blocked, p.red),
            (AgentState::Blocked, false, StateClass::Blocked, p.red),
            (AgentState::Working, true, StateClass::Working, p.yellow),
            (AgentState::Idle, false, StateClass::Done, p.teal),
            (AgentState::Idle, true, StateClass::Idle, p.green),
            (AgentState::Unknown, true, StateClass::None, p.overlay0),
        ] {
            assert_eq!(StateClass::of(state, seen), class);
            assert_eq!(class.color(&p), color);
        }
    }

    #[test]
    fn remote_statuses_map_onto_the_same_classes() {
        use crate::api::schema::AgentStatus;
        assert_eq!(
            StateClass::of_remote(AgentStatus::Blocked),
            StateClass::Blocked
        );
        assert_eq!(StateClass::of_remote(AgentStatus::Done), StateClass::Done);
        assert_eq!(
            StateClass::of_remote(AgentStatus::Working),
            StateClass::Working
        );
        assert_eq!(StateClass::of_remote(AgentStatus::Idle), StateClass::Idle);
        assert_eq!(
            StateClass::of_remote(AgentStatus::Unknown),
            StateClass::None
        );
    }

    #[test]
    fn join_sorts_the_multiset_by_severity_and_keeps_repetition() {
        let join = join_states([StateClass::Idle, StateClass::Blocked, StateClass::Working]);
        assert_eq!(
            join.classes(),
            [StateClass::Blocked, StateClass::Working, StateClass::Idle]
        );
        // Repetition is meaningful: two blocked among done reads r·r·g.
        let join = join_states([StateClass::Idle, StateClass::Blocked, StateClass::Blocked]);
        assert_eq!(
            join.classes(),
            [StateClass::Blocked, StateClass::Blocked, StateClass::Idle]
        );
        assert_eq!(join.head(), Some(StateClass::Blocked));
    }

    #[test]
    fn join_caps_at_top_three() {
        let join = join_states([
            StateClass::Idle,
            StateClass::Working,
            StateClass::Blocked,
            StateClass::Idle,
            StateClass::Done,
        ]);
        assert_eq!(
            join.classes(),
            [StateClass::Blocked, StateClass::Done, StateClass::Working]
        );
    }

    #[test]
    fn join_drops_none_and_can_be_empty() {
        assert!(join_states([]).is_empty());
        assert!(join_states([StateClass::None, StateClass::None]).is_empty());
        assert_eq!(join_states([]).head(), None);
        let join = join_states([StateClass::None, StateClass::Working]);
        assert_eq!(join.classes(), [StateClass::Working]);
    }

    #[test]
    fn fewer_states_yield_a_shorter_join() {
        assert_eq!(join_states([StateClass::Idle]).classes().len(), 1);
        assert_eq!(
            join_states([StateClass::Idle, StateClass::Working])
                .classes()
                .len(),
            2
        );
    }

    #[test]
    fn packed_rects_render_the_join_and_hollow_for_empty() {
        let p = palette();
        let spans = packed_rects(&join_states([StateClass::Blocked, StateClass::Working]), &p);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, RECT_FILLED);
        assert_eq!(spans[0].style.fg, Some(p.red));
        assert_eq!(spans[1].style.fg, Some(p.yellow));

        let hollow = packed_rects(&join_states([]), &p);
        assert_eq!(hollow.len(), 1);
        assert_eq!(hollow[0].content, RECT_HOLLOW);
        assert_eq!(hollow[0].style.fg, Some(p.overlay0));
    }

    #[test]
    fn medallion_rings_follow_the_join_with_a_muted_presence_fallback() {
        let p = palette();
        let rings = medallion_rings(
            &join_states([StateClass::Idle, StateClass::Blocked, StateClass::Working]),
            &p,
        );
        assert_eq!(rings, vec![p.red, p.yellow, p.green]);
        // Reachable but no live agents: a single muted ring, not a blank.
        assert_eq!(medallion_rings(&join_states([]), &p), vec![p.overlay0]);
    }

    #[test]
    fn pr_glyphs_match_the_header_hud_language() {
        use crate::worktree::PrState;
        let p = palette();
        assert_eq!(pr_state_glyph(PrState::Open, &p), ("\u{2299}", p.accent));
        assert_eq!(pr_state_glyph(PrState::Draft, &p), ("\u{25d0}", p.overlay0));
        assert_eq!(pr_state_glyph(PrState::Merged, &p), ("\u{2713}", p.mauve));
        assert_eq!(pr_state_glyph(PrState::Closed, &p), ("\u{2717}", p.red));
    }
}
