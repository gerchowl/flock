//! Derived supervisor events (ADR-0028 §2, #1027): `agent.phase_changed`,
//! and `agent.needs_human`, `agent.limit_reached`, `agent.gone` as kinds of
//! their own, computed once in the server from each local agent's `state`.
//!
//! Event-driven, never a poll (ADR-0028 §5). The server re-reads one agent's
//! `state` (the one `agent.get` reports) only when something that feeds its
//! axes happened: a detector `StateChanged`, a hook report, or a pane
//! lifecycle event. A phase is announced once it has held for the settle
//! window (§1, `SETTLE_MS`), so the only timers are per agent: the close of a
//! pending phase's window, and the end of a `settling` turn. With no phase
//! pending the loop has no deadline here. A phase that flips back to the
//! announced one inside the window cancels its deadline and emits nothing.
//!
//! The window is applied once (§1): `phase` derives `finished`,
//! `finished_with_background` and `waiting_on_event` only after the idle
//! turn has held for the window, so entering one of them from a non-idle
//! phase is announced as soon as it is read. A move between two of them (a
//! background shell or a registered wait coming and going) is not a turn
//! end the window already covered, so it must hold for the window itself.
//!
//! `settling` and `unknown` are not supervisor phases: they never become a
//! candidate and never interrupt one. An agent is `gone` at once, since
//! there is no agent left to flip back, when no pane carries its id any more,
//! or when this server read it as an agent and its pane stopped being one
//! (it left `agent.list`).
//!
//! Restarts and live handoffs. `begin_agent_phases` runs where the server
//! loop starts: restore or handoff import has re-created every pane by then,
//! and on a handoff the exporting server has stopped writing the log. It
//! seeds the last phase the log announced for each agent, so that phase is
//! not repeated, and announces `gone` for a seeded agent no pane carries.
//! A restored agent whose detection has not reported yet (a hand-launched
//! one is not in `agent.list` until it does) is never `gone` for that: its
//! pane still holds its id, so it keeps its seeded phase and emits nothing
//! until it is read again.
//!
//! Known limits. An agent whose last announcement rotated out of the log is
//! announced again with `from: unknown`. Codex and opencode read their
//! `background` axis from the process tree, which raises no event: a job
//! that ends while nothing else on the pane changes is seen at the pane's
//! next state change (#1163).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::api::agent_state::{AgentState, Lifecycle, Phase, SETTLE_MS};
use crate::api::schema::{AgentPhaseEvent, EventData, EventEnvelope, EventKind};

/// The settle window a phase must hold before it is announced: the one
/// `phase` itself uses. Making it `[events] settle_ms` is #1152.
pub(crate) const SETTLE: Duration = Duration::from_millis(SETTLE_MS);

/// One local agent's `state`, as `agent.get` reports it.
pub(crate) struct Sample {
    pub agent_id: String,
    pub pane: String,
    pub state: AgentState,
}

struct Tracked {
    pane: String,
    announced: Phase,
    candidate: Phase,
    since: Instant,
    state: AgentState,
    /// When this agent's state must be read again with no event: the close
    /// of its candidate's window, or the end of its `settling` turn.
    due: Option<Instant>,
    /// Whether this server has read the agent's state, rather than only
    /// seeded it from the log.
    observed: bool,
}

/// What happened since the last tick. Written from `emit_event`, which only
/// borrows the app.
#[derive(Default)]
struct Touched {
    /// Public pane ids whose agent's state may have changed.
    panes: HashSet<String>,
    /// An agent may have left without a pane to read.
    rescan: bool,
}

#[derive(Default)]
pub(crate) struct AgentPhaseTracker {
    agents: HashMap<String, Tracked>,
    touched: RefCell<Touched>,
    /// Whether the first tick after start has read every agent.
    started: bool,
}

/// What the next tick must read.
pub(crate) enum Work {
    /// Every local agent: the first tick after start.
    All,
    /// These panes, then a rescan for agents that left.
    Panes(Vec<String>),
}

/// Whether a phase is one a supervisor branches on.
fn is_supervisor_phase(phase: Phase) -> bool {
    !matches!(phase, Phase::Settling | Phase::Unknown)
}

fn is_idle_phase(phase: Phase) -> bool {
    matches!(
        phase,
        Phase::Finished | Phase::FinishedWithBackground | Phase::WaitingOnEvent
    )
}

/// How long `to` must hold, coming from `announced`, before it is announced.
fn window(announced: Phase, to: Phase, settle: Duration) -> Duration {
    if is_idle_phase(to) && !is_idle_phase(announced) {
        Duration::ZERO
    } else {
        settle
    }
}

impl AgentPhaseTracker {
    /// Seed the last phase the durable log announced for each agent it has
    /// not seen go. Each seeded agent is tracked as announced, so a restart
    /// does not repeat that phase, and the first tick announces `gone` for
    /// one that did not come back.
    pub(crate) fn seed_from_events<'a>(&mut self, events: impl Iterator<Item = &'a EventEnvelope>) {
        let now = Instant::now();
        for envelope in events {
            let EventData::AgentPhaseChanged(event) = &envelope.data else {
                continue;
            };
            if event.to == Phase::Gone {
                self.agents.remove(&event.agent_id);
                continue;
            }
            self.agents.insert(
                event.agent_id.clone(),
                Tracked {
                    pane: event.pane.clone(),
                    announced: event.to,
                    candidate: event.to,
                    since: now,
                    state: event.state.clone(),
                    due: None,
                    observed: false,
                },
            );
        }
    }

    /// Note an event the server emitted that may change an agent's state or
    /// remove it.
    pub(crate) fn touch_event(&self, envelope: &EventEnvelope) {
        let mut touched = self.touched.borrow_mut();
        match &envelope.data {
            EventData::PaneAgentStatusChanged { pane_id, .. }
            | EventData::PaneAgentDetected { pane_id, .. }
            | EventData::AgentHibernated { pane_id, .. }
            | EventData::AgentResumedFromHibernation { pane_id, .. }
            | EventData::AgentRestart { pane_id, .. }
            | EventData::PaneClosed { pane_id, .. }
            | EventData::PaneExited { pane_id, .. } => {
                touched.panes.insert(pane_id.clone());
            }
            EventData::AgentRemoved { .. }
            | EventData::WorkspaceClosed { .. }
            | EventData::TabClosed { .. } => touched.rescan = true,
            _ => {}
        }
    }

    /// Note a pane whose detector or hook reported a change.
    pub(crate) fn touch_pane(&self, pane: String) {
        self.touched.borrow_mut().panes.insert(pane);
    }

    /// When the loop must wake for this tracker: at once when something was
    /// touched, else at the earliest agent deadline, else never. The first
    /// tick needs no wake: the loop ticks on its first iteration.
    pub(crate) fn next_deadline(&self, now: Instant) -> Option<Instant> {
        let touched = self.touched.borrow();
        if touched.rescan || !touched.panes.is_empty() {
            return Some(now);
        }
        self.agents.values().filter_map(|tracked| tracked.due).min()
    }

    /// What to read at `now`, or `None` when nothing is touched or due.
    pub(crate) fn take_work(&mut self, now: Instant) -> Option<Work> {
        if !self.started {
            self.started = true;
            *self.touched.borrow_mut() = Touched::default();
            return Some(Work::All);
        }
        let Touched { mut panes, rescan } = std::mem::take(&mut *self.touched.borrow_mut());
        for tracked in self.agents.values_mut() {
            if tracked.due.is_some_and(|due| due <= now) {
                tracked.due = None;
                panes.insert(tracked.pane.clone());
            }
        }
        (rescan || !panes.is_empty()).then(|| Work::Panes(panes.into_iter().collect()))
    }

    /// Fold one agent's current `state` and return what it settles.
    pub(crate) fn observe(
        &mut self,
        now: Instant,
        settle: Duration,
        sample: Sample,
    ) -> Vec<EventEnvelope> {
        let phase = sample.state.phase;
        let tracked = self
            .agents
            .entry(sample.agent_id.clone())
            .or_insert_with(|| Tracked {
                pane: sample.pane.clone(),
                announced: Phase::Unknown,
                candidate: Phase::Unknown,
                since: now,
                state: sample.state.clone(),
                due: None,
                observed: true,
            });
        tracked.pane = sample.pane;
        tracked.due = None;
        tracked.observed = true;
        if is_supervisor_phase(phase) {
            if phase != tracked.candidate {
                tracked.candidate = phase;
                tracked.since = now;
            }
            tracked.state = sample.state;
        } else if phase == Phase::Settling {
            // The turn settles with no event: read it again when it has.
            let held = Duration::from_millis(sample.state.held_ms.unwrap_or(0));
            tracked.due = Some(now + settle.saturating_sub(held));
        }
        if tracked.candidate == tracked.announced {
            return Vec::new();
        }
        let wait = window(tracked.announced, tracked.candidate, settle);
        let held = now.saturating_duration_since(tracked.since);
        if held < wait {
            let close = tracked.since + wait;
            tracked.due = Some(tracked.due.map_or(close, |due| due.min(close)));
            return Vec::new();
        }
        if !is_supervisor_phase(phase) {
            // The window closed while the turn reads `settling`: the
            // candidate no longer holds.
            return Vec::new();
        }
        announce(&sample.agent_id, tracked, phase, held)
    }

    /// Announce `gone` for every tracked agent no pane carries (`on_panes`),
    /// and for every agent this server read that left `agent.list`
    /// (`listed`). An agent only seeded from the log is judged by its pane
    /// alone: detection not having reported yet is not an absence.
    pub(crate) fn retain_present(
        &mut self,
        on_panes: &HashSet<String>,
        listed: &HashSet<String>,
    ) -> Vec<EventEnvelope> {
        let gone: Vec<String> = self
            .agents
            .iter()
            .filter(|(agent_id, tracked)| {
                !on_panes.contains(*agent_id) || (tracked.observed && !listed.contains(*agent_id))
            })
            .map(|(agent_id, _)| agent_id.clone())
            .collect();
        let mut events = Vec::new();
        for agent_id in gone {
            if let Some(mut tracked) = self.agents.remove(&agent_id) {
                tracked.state.lifecycle = Lifecycle::Gone;
                tracked.state.phase = Phase::Gone;
                tracked.state.held_ms = None;
                events.extend(announce(
                    &agent_id,
                    &mut tracked,
                    Phase::Gone,
                    Duration::ZERO,
                ));
            }
        }
        events
    }
}

/// The `agent.phase_changed` for a settled phase, followed by the kind of
/// its own that `needs_human`, `limit_reached` and `gone` also carry.
fn announce(
    agent_id: &str,
    tracked: &mut Tracked,
    to: Phase,
    held: Duration,
) -> Vec<EventEnvelope> {
    // An idle phase is first read as its window closes: its own `held_ms`
    // knows how long it held before this read did.
    let held = tracked
        .state
        .held_ms
        .map_or(held, |ms| held.max(Duration::from_millis(ms)));
    let event = AgentPhaseEvent {
        agent_id: agent_id.to_string(),
        pane: tracked.pane.clone(),
        from: tracked.announced,
        to,
        state: tracked.state.clone(),
        held_ms: held.as_millis().min(u128::from(u64::MAX)) as u64,
    };
    tracked.announced = to;
    let mut events = vec![EventEnvelope {
        event: EventKind::AgentPhaseChanged,
        data: EventData::AgentPhaseChanged(event.clone()),
    }];
    let own = match to {
        Phase::NeedsHuman => Some((
            EventKind::AgentNeedsHuman,
            EventData::AgentNeedsHuman(event),
        )),
        Phase::LimitReached => Some((
            EventKind::AgentLimitReached,
            EventData::AgentLimitReached(event),
        )),
        Phase::Gone => Some((EventKind::AgentGone, EventData::AgentGone(event))),
        _ => None,
    };
    events.extend(own.map(|(event, data)| EventEnvelope { event, data }));
    events
}

impl super::App {
    /// Read the agents something touched or whose deadline came due, and
    /// emit the derived events that settled. Mirrored in the headless and
    /// TUI loops (#25).
    pub(crate) fn tick_agent_phases(&mut self, now: Instant) {
        let Some(work) = self.agent_phases.take_work(now) else {
            return;
        };
        let (local, on_panes) = self.local_agent_panes();
        let panes: Vec<(usize, crate::layout::PaneId)> = match work {
            Work::All => local.iter().map(|(_, at)| *at).collect(),
            Work::Panes(panes) => panes
                .iter()
                .filter_map(|pane| self.parse_pane_id(pane))
                .collect(),
        };
        let mut events = Vec::new();
        for (ws_idx, pane_id) in panes {
            let Some((agent_id, state)) = self.agent_state_of(ws_idx, pane_id) else {
                continue;
            };
            let Some(pane) = self.public_pane_id(ws_idx, pane_id) else {
                continue;
            };
            let sample = Sample {
                agent_id,
                pane,
                state,
            };
            events.extend(self.agent_phases.observe(now, SETTLE, sample));
        }
        let listed = local.into_iter().map(|(agent_id, _)| agent_id).collect();
        events.extend(self.agent_phases.retain_present(&on_panes, &listed));
        for event in events {
            self.emit_event(event);
        }
    }

    /// Once per start, where the server loop begins: seed the last announced
    /// phases from the final log `restored`, and announce `gone` for each
    /// seeded agent no restored pane carries.
    pub(crate) fn begin_agent_phases(
        &mut self,
        restored: &[(u64, u64, crate::api::schema::EventEnvelope)],
    ) {
        self.agent_phases
            .seed_from_events(restored.iter().map(|(_, _, envelope)| envelope));
        let (_, on_panes) = self.local_agent_panes();
        for event in self.agent_phases.retain_present(&on_panes, &on_panes) {
            self.emit_event(event);
        }
    }

    /// Every listed local agent's id and pane, and the agent id of every
    /// pane's terminal whether or not it reads as an agent yet, from the
    /// terminal table alone.
    #[allow(clippy::type_complexity)]
    fn local_agent_panes(
        &self,
    ) -> (
        Vec<(String, (usize, crate::layout::PaneId))>,
        HashSet<String>,
    ) {
        let mut listed = Vec::new();
        let mut on_panes = HashSet::new();
        for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
            for tab in &ws.tabs {
                for pane_id in tab.layout.pane_ids() {
                    let Some(terminal) = ws
                        .pane_state(pane_id)
                        .and_then(|pane| self.state.terminals.get(&pane.attached_terminal_id))
                    else {
                        continue;
                    };
                    let agent_id = terminal.agent_id.to_string();
                    if terminal.is_agent_terminal() {
                        listed.push((agent_id.clone(), (ws_idx, pane_id)));
                    }
                    on_panes.insert(agent_id);
                }
            }
        }
        (listed, on_panes)
    }

    /// Note a detector or hook report on a pane, before it is applied.
    pub(super) fn touch_agent_phase(&self, pane_id: crate::layout::PaneId) {
        let pane = self
            .find_pane(pane_id)
            .and_then(|(ws_idx, _)| self.public_pane_id(ws_idx, pane_id));
        if let Some(pane) = pane {
            self.agent_phases.touch_pane(pane);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::agent_state::{Attention, BlockedReason, HumanReason};

    const WIN: Duration = Duration::from_secs(20);

    fn state(phase: Phase) -> AgentState {
        let attention = match phase {
            Phase::NeedsHuman => Attention::NeedsHuman {
                reason: HumanReason::Permission,
            },
            Phase::LimitReached => Attention::Blocked {
                reason: BlockedReason::ProviderLimit,
                retry_after_ms: None,
            },
            _ => Attention::None,
        };
        AgentState {
            phase,
            attention,
            lifecycle: Lifecycle::Live,
            ..AgentState::unknown()
        }
    }

    fn sample(phase: Phase) -> Sample {
        Sample {
            agent_id: "agent_a".into(),
            pane: "w1:p1".into(),
            state: state(phase),
        }
    }

    fn kinds(events: Vec<EventEnvelope>) -> Vec<(EventKind, Phase, Phase)> {
        events
            .into_iter()
            .map(|envelope| {
                let (EventData::AgentPhaseChanged(event)
                | EventData::AgentNeedsHuman(event)
                | EventData::AgentLimitReached(event)
                | EventData::AgentGone(event)) = envelope.data
                else {
                    panic!("unexpected {:?}", envelope.event);
                };
                (envelope.event, event.from, event.to)
            })
            .collect()
    }

    /// A tracker past its first tick, as the loop leaves it.
    fn started(at: Instant) -> AgentPhaseTracker {
        let mut tracker = AgentPhaseTracker::default();
        assert!(matches!(tracker.take_work(at), Some(Work::All)));
        tracker
    }

    /// Feed `(seconds after start, phase)` reads of one agent, `None` for a
    /// rescan without it, and return what was emitted.
    fn run(
        tracker: &mut AgentPhaseTracker,
        start: Instant,
        steps: &[(u64, Option<Phase>)],
    ) -> Vec<(EventKind, Phase, Phase)> {
        let mut out = Vec::new();
        for (at, phase) in steps {
            let now = start + Duration::from_secs(*at);
            out.extend(kinds(match phase {
                Some(phase) => tracker.observe(now, WIN, sample(*phase)),
                None => tracker.retain_present(&HashSet::new(), &HashSet::new()),
            }));
        }
        out
    }

    #[test]
    fn a_phase_that_flips_back_inside_the_window_emits_nothing_and_cancels_its_deadline() {
        use Phase::*;
        let start = Instant::now();
        let mut tracker = started(start);
        let first = run(
            &mut tracker,
            start,
            &[(0, Some(Working)), (20, Some(Working))],
        );
        assert_eq!(first, [(EventKind::AgentPhaseChanged, Unknown, Working)]);
        run(&mut tracker, start, &[(21, Some(NeedsHuman))]);
        assert_eq!(
            tracker.next_deadline(start),
            Some(start + Duration::from_secs(41))
        );
        // The prompt is answered inside the window: no deadline is left.
        assert_eq!(run(&mut tracker, start, &[(30, Some(Working))]), []);
        assert_eq!(tracker.next_deadline(start), None);
        assert!(tracker.take_work(start + Duration::from_secs(41)).is_none());
        let flips = run(
            &mut tracker,
            start,
            &[
                (45, Some(Unknown)),
                (60, Some(Working)),
                (90, Some(Working)),
            ],
        );
        assert_eq!(flips, []);
    }

    #[test]
    fn a_candidate_deadline_fires_once_and_emits_once() {
        let start = Instant::now();
        let at = |secs| start + Duration::from_secs(secs);
        let mut tracker = started(start);
        assert!(tracker
            .observe(at(0), WIN, sample(Phase::Working))
            .is_empty());
        assert_eq!(tracker.next_deadline(at(1)), Some(at(20)));
        assert!(tracker.take_work(at(19)).is_none());
        let Some(Work::Panes(panes)) = tracker.take_work(at(20)) else {
            panic!("the window's close is due");
        };
        assert_eq!(panes, ["w1:p1"]);
        let emitted = tracker.observe(at(20), WIN, sample(Phase::Working));
        assert_eq!(emitted.len(), 1);
        assert_eq!(tracker.next_deadline(at(21)), None);
        assert!(tracker.take_work(at(60)).is_none());
    }

    #[test]
    fn a_settling_turn_is_read_again_when_its_window_closes() {
        let start = Instant::now();
        let mut tracker = started(start);
        let mut settling = sample(Phase::Settling);
        settling.state.held_ms = Some(5_000);
        assert!(tracker.observe(start, WIN, settling).is_empty());
        assert_eq!(
            tracker.next_deadline(start),
            Some(start + Duration::from_secs(15))
        );
    }

    /// ADR-0028 §1 applies the window once: entering an idle phase is a turn
    /// end the window already covered, a background shell or a wait coming
    /// and going between idle phases is not.
    #[test]
    fn a_move_between_idle_phases_must_hold_the_window() {
        use EventKind as K;
        use Phase::*;
        let start = Instant::now();
        let mut tracker = started(start);
        let events = run(
            &mut tracker,
            start,
            &[
                (0, Some(Working)),
                (20, Some(Working)),
                (21, Some(Finished)),
                (30, Some(FinishedWithBackground)),
                (35, Some(Finished)),
                (40, Some(WaitingOnEvent)),
                (45, Some(Finished)),
                (50, Some(FinishedWithBackground)),
                (70, Some(FinishedWithBackground)),
            ],
        );
        assert_eq!(
            events,
            [
                (K::AgentPhaseChanged, Unknown, Working),
                (K::AgentPhaseChanged, Working, Finished),
                (K::AgentPhaseChanged, Finished, FinishedWithBackground),
            ]
        );
    }

    #[test]
    fn each_real_transition_is_announced_once_with_its_own_kind() {
        use EventKind as K;
        use Phase::*;
        let start = Instant::now();
        let mut tracker = started(start);
        let events = run(
            &mut tracker,
            start,
            &[
                (0, Some(Working)),
                (20, Some(Working)),
                (21, Some(NeedsHuman)),
                (41, Some(NeedsHuman)),
                (50, Some(NeedsHuman)),
                (51, Some(LimitReached)),
                (71, Some(LimitReached)),
                (90, Some(LimitReached)),
                (91, Some(Settling)),
                (111, Some(Finished)),
                (112, Some(Finished)),
                (113, None),
                (114, None),
            ],
        );
        assert_eq!(
            events,
            [
                (K::AgentPhaseChanged, Unknown, Working),
                (K::AgentPhaseChanged, Working, NeedsHuman),
                (K::AgentNeedsHuman, Working, NeedsHuman),
                (K::AgentPhaseChanged, NeedsHuman, LimitReached),
                (K::AgentLimitReached, NeedsHuman, LimitReached),
                (K::AgentPhaseChanged, LimitReached, Finished),
                (K::AgentPhaseChanged, Finished, Gone),
                (K::AgentGone, Finished, Gone),
            ]
        );
    }

    #[test]
    fn a_restart_does_not_repeat_a_phase_and_announces_an_agent_that_did_not_come_back() {
        use Phase::*;
        let start = Instant::now();
        let mut before = started(start);
        let mut log = before.observe(start, WIN, sample(Finished));
        assert_eq!(log.len(), 1);
        let mut after = AgentPhaseTracker::default();
        after.seed_from_events(log.iter());
        assert!(after.observe(start, WIN, sample(Finished)).is_empty());
        let mut lost = AgentPhaseTracker::default();
        lost.seed_from_events(log.iter());
        assert_eq!(
            kinds(lost.retain_present(&HashSet::new(), &HashSet::new())),
            [
                (EventKind::AgentPhaseChanged, Finished, Gone),
                (EventKind::AgentGone, Finished, Gone)
            ]
        );
        // An agent the log saw go is not seeded: one by that id starts fresh.
        log.extend(before.retain_present(&HashSet::new(), &HashSet::new()));
        let mut again = AgentPhaseTracker::default();
        again.seed_from_events(log.iter());
        assert!(again
            .retain_present(&HashSet::new(), &HashSet::new())
            .is_empty());
        assert_eq!(again.observe(start, WIN, sample(Finished)).len(), 1);
    }

    fn status_at(app: &mut crate::app::App, state: crate::detect::AgentState, at: Instant) {
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        app.handle_internal_event(crate::events::AppEvent::StateChanged {
            pane_id,
            agent: Some(crate::detect::Agent::Claude),
            state,
            activity: None,
            provider_limit: None,
            visible_blocker: false,
            visible_idle: false,
            visible_working: false,
            settled_with_background_shells: false,
            background_tasks: Some(0),
            needs_human: None,
            process_exited: false,
            observed_at: at,
        });
    }

    fn status(app: &mut crate::app::App, state: crate::detect::AgentState) {
        status_at(app, state, Instant::now());
    }

    /// An idle turn that ended long ago, so `phase` reads `finished`.
    fn settled_idle(app: &mut crate::app::App) {
        let long_ago = Instant::now() - Duration::from_secs(60);
        status_at(app, crate::detect::AgentState::Idle, long_ago);
    }

    /// A server on `hub`, with one shell pane when `pane_for` names the agent
    /// id its terminal carries (a restored or imported pane), started the
    /// way `run` starts it: with the log as it is now.
    fn server(hub: &crate::api::EventHub, pane_for: Option<&str>) -> crate::app::App {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::new(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            hub.clone(),
        );
        if let Some(agent_id) = pane_for {
            app.state.workspaces = vec![crate::workspace::Workspace::test_new("phases")];
            app.state.ensure_test_terminals();
            for terminal in app.state.terminals.values_mut() {
                terminal.agent_id = crate::terminal::AgentId::from_persisted(agent_id.to_string());
            }
        }
        app.begin_agent_phases(&hub.persisted_events_after(0));
        app
    }

    fn temp_log(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("flock-phases-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("event-log.jsonl")
    }

    /// The derived kinds in the log file as written, in order. Read from the
    /// file, not a hub, so two servers sharing a log in a handoff test are
    /// both counted.
    fn logged(log: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                let value: serde_json::Value = serde_json::from_str(line).ok()?;
                let kind = value["envelope"]["event"].as_str()?.to_string();
                kind.starts_with("agent.").then_some(kind)
            })
            .collect()
    }

    fn count(log: &std::path::Path, kind: &str) -> usize {
        logged(log).iter().filter(|logged| *logged == kind).count()
    }

    /// ADR-0028 §10 E3: real detector status changes, through the server's
    /// `PaneAgentStatusChanged`, end at exactly one durable
    /// `agent.phase_changed` that `events.read`'s kind filter serves, and a
    /// restarted server does not repeat it.
    #[tokio::test]
    async fn a_real_status_sequence_settles_into_one_durable_phase_change() {
        use crate::detect::AgentState as S;
        let log = temp_log("sequence");
        let hub = crate::api::EventHub::with_persistence(log.clone());
        let mut app = server(&hub, Some("agent_seq"));
        let start = Instant::now();
        let tick = |app: &mut crate::app::App, secs: u64| {
            app.tick_agent_phases(start + Duration::from_secs(secs));
        };
        tick(&mut app, 0);
        status(&mut app, S::Working);
        tick(&mut app, 0);
        tick(&mut app, 20);
        // A short idle and a prompt the human answers, both inside the window.
        status(&mut app, S::Idle);
        tick(&mut app, 21);
        status(&mut app, S::Working);
        tick(&mut app, 22);
        status(&mut app, S::Blocked);
        tick(&mut app, 23);
        status(&mut app, S::Working);
        tick(&mut app, 40);
        tick(&mut app, 60);
        let read = |kind: EventKind| hub.read(0, 100, |envelope| envelope.event == kind).events;
        assert!(read(EventKind::PaneAgentStatusChanged).len() >= 5);
        let changes = read(EventKind::AgentPhaseChanged);
        assert_eq!(changes.len(), 1, "{changes:?}");
        let EventData::AgentPhaseChanged(event) = &changes[0].1.data else {
            panic!("{changes:?}");
        };
        assert_eq!((event.from, event.to), (Phase::Unknown, Phase::Working));
        assert!(read(EventKind::AgentNeedsHuman).is_empty());

        let restarted = crate::api::EventHub::with_persistence(log.clone());
        let mut app = server(&restarted, Some("agent_seq"));
        tick(&mut app, 100);
        status(&mut app, S::Working);
        tick(&mut app, 100);
        tick(&mut app, 130);
        assert_eq!(logged(&log), ["agent.phase_changed"]);
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
    }

    /// A hand-launched agent has no name and no launch argv, so after a
    /// handoff it is not in `agent.list` until its detector reports again.
    /// Its pane carries its id the whole time: it is not gone, and its
    /// unchanged phase is not announced again.
    #[tokio::test]
    async fn a_hand_launched_agent_survives_a_handoff_without_gone() {
        let log = temp_log("hand-launched");
        let exporter = crate::api::EventHub::with_persistence(log.clone());
        let mut app = server(&exporter, Some("agent_hand"));
        settled_idle(&mut app);
        let now = Instant::now();
        app.tick_agent_phases(now);
        assert_eq!(logged(&log), ["agent.phase_changed"]);

        let importer = crate::api::EventHub::with_persistence(log.clone());
        let mut app = server(&importer, Some("agent_hand"));
        let terminal = app.state.terminals.values().next().unwrap();
        assert!(!terminal.is_agent_terminal(), "detection has not reported");
        app.tick_agent_phases(now);
        // Something else closes: a rescan, while detection is still silent.
        app.emit_event(EventEnvelope {
            event: EventKind::WorkspaceClosed,
            data: EventData::WorkspaceClosed {
                workspace_id: "w9".into(),
            },
        });
        app.tick_agent_phases(now + Duration::from_secs(1));
        settled_idle(&mut app);
        app.tick_agent_phases(now + Duration::from_secs(2));
        assert_eq!(logged(&log), ["agent.phase_changed"]);
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
    }

    /// An agent the log announced that no restored pane carries is gone,
    /// once, and a later start does not announce it again.
    #[tokio::test]
    async fn a_seeded_agent_whose_pane_is_absent_gets_one_gone() {
        let log = temp_log("absent");
        let hub = crate::api::EventHub::with_persistence(log.clone());
        let mut app = server(&hub, Some("agent_lost"));
        settled_idle(&mut app);
        let now = Instant::now();
        app.tick_agent_phases(now);

        let hub = crate::api::EventHub::with_persistence(log.clone());
        let mut app = server(&hub, None);
        app.tick_agent_phases(now);
        app.tick_agent_phases(now + Duration::from_secs(30));
        let hub = crate::api::EventHub::with_persistence(log.clone());
        let mut app = server(&hub, None);
        app.tick_agent_phases(now);
        assert_eq!(
            logged(&log),
            ["agent.phase_changed", "agent.phase_changed", "agent.gone"]
        );
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
    }

    /// In a live handoff the importing server is built while the exporter
    /// still writes the log. Seeding where the loop starts reads the
    /// exporter's last announcement, so the importer does not repeat it.
    #[tokio::test]
    async fn seeding_after_the_exporters_final_write_does_not_repeat_it() {
        use crate::detect::AgentState as S;
        let log = temp_log("final-write");
        let exporter_hub = crate::api::EventHub::with_persistence(log.clone());
        let mut exporter = server(&exporter_hub, Some("agent_moving"));
        settled_idle(&mut exporter);
        let start = Instant::now();
        exporter.tick_agent_phases(start);

        // The importer's App exists before the exporter's last write.
        let importer_hub = crate::api::EventHub::with_persistence(log.clone());
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut importer = crate::app::App::new(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            importer_hub.clone(),
        );
        importer.state.workspaces = vec![crate::workspace::Workspace::test_new("phases")];
        importer.state.ensure_test_terminals();
        for terminal in importer.state.terminals.values_mut() {
            terminal.agent_id =
                crate::terminal::AgentId::from_persisted("agent_moving".to_string());
        }
        status(&mut exporter, S::Working);
        exporter.tick_agent_phases(start);
        exporter.tick_agent_phases(start + Duration::from_secs(20));
        assert_eq!(count(&log, "agent.phase_changed"), 2);

        importer.begin_agent_phases(&importer_hub.persisted_events_after(0));
        status(&mut importer, S::Working);
        importer.tick_agent_phases(start + Duration::from_secs(30));
        importer.tick_agent_phases(start + Duration::from_secs(50));
        assert_eq!(count(&log, "agent.phase_changed"), 2, "{:?}", logged(&log));
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
    }

    /// An idle server with agents whose phases are announced has no reason
    /// to wake: the tracker adds no loop deadline.
    #[tokio::test]
    async fn an_idle_server_with_settled_agents_has_no_loop_deadline() {
        let hub = crate::api::EventHub::default();
        let mut app = server(&hub, Some("agent_idle"));
        settled_idle(&mut app);
        let now = Instant::now();
        app.tick_agent_phases(now);
        assert_eq!(app.agent_phases.next_deadline(now), None);
        let changes = hub.read(0, 100, |e| e.event == EventKind::AgentPhaseChanged);
        assert_eq!(changes.events.len(), 1);
        app.tick_agent_phases(now + Duration::from_secs(60));
        assert_eq!(app.agent_phases.next_deadline(now), None);
    }
}
