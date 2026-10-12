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
//! candidate and never interrupt one. An agent that leaves the local agent
//! table (`agent.list`) is `gone` at once: there is no agent left to flip
//! back.
//!
//! Restarts and live handoffs. The last phase the log announced for each
//! agent is seeded at start, so it is not repeated. Restore and handoff
//! adoption finish before the server loop runs, so the first tick reads
//! every agent and announces `gone` for each seeded agent that did not come
//! back, such as one stopped with the server and not restored.
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
            });
        tracked.pane = sample.pane;
        tracked.due = None;
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

    /// Announce `gone` for every tracked agent not in `present`.
    pub(crate) fn retain_present(&mut self, present: &HashSet<String>) -> Vec<EventEnvelope> {
        let gone: Vec<String> = self
            .agents
            .keys()
            .filter(|agent_id| !present.contains(*agent_id))
            .cloned()
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
        let local = self.local_agent_panes();
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
        let present = local.into_iter().map(|(agent_id, _)| agent_id).collect();
        events.extend(self.agent_phases.retain_present(&present));
        for event in events {
            self.emit_event(event);
        }
    }

    /// Every local agent's id and pane, from the terminal table alone.
    fn local_agent_panes(&self) -> Vec<(String, (usize, crate::layout::PaneId))> {
        let mut agents = Vec::new();
        for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
            for tab in &ws.tabs {
                for pane_id in tab.layout.pane_ids() {
                    let terminal = ws
                        .pane_state(pane_id)
                        .and_then(|pane| self.state.terminals.get(&pane.attached_terminal_id));
                    if let Some(terminal) = terminal.filter(|t| t.is_agent_terminal()) {
                        agents.push((terminal.agent_id.to_string(), (ws_idx, pane_id)));
                    }
                }
            }
        }
        agents
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
                None => tracker.retain_present(&HashSet::new()),
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
            kinds(lost.retain_present(&HashSet::new())),
            [
                (EventKind::AgentPhaseChanged, Finished, Gone),
                (EventKind::AgentGone, Finished, Gone)
            ]
        );
        // An agent the log saw go is not seeded: one by that id starts fresh.
        log.extend(before.retain_present(&HashSet::new()));
        let mut again = AgentPhaseTracker::default();
        again.seed_from_events(log.iter());
        assert!(again.retain_present(&HashSet::new()).is_empty());
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

    fn app_on(hub: crate::api::EventHub, with_agent_pane: bool) -> crate::app::App {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::new(&crate::config::Config::default(), true, None, rx, hub);
        if with_agent_pane {
            app.state.workspaces = vec![crate::workspace::Workspace::test_new("phases")];
            app.state.ensure_test_terminals();
        }
        app
    }

    fn read(hub: &crate::api::EventHub, kind: EventKind) -> Vec<(u64, EventEnvelope)> {
        hub.read(0, 100, |envelope| envelope.event == kind).events
    }

    /// ADR-0028 §10 E3: real detector status changes, through the server's
    /// `PaneAgentStatusChanged`, end at exactly one durable
    /// `agent.phase_changed` that `events.read`'s kind filter serves. A
    /// restarted server on the same log does not repeat it, and announces
    /// `agent.gone` once when the agent did not come back.
    #[tokio::test]
    async fn a_real_status_sequence_settles_into_one_durable_phase_change() {
        use crate::detect::AgentState as S;
        let log = std::env::temp_dir()
            .join(format!("flock-phases-{}", std::process::id()))
            .join("event-log.jsonl");
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
        let hub = crate::api::EventHub::with_persistence(log.clone());
        let mut app = app_on(hub.clone(), true);
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
        assert!(read(&hub, EventKind::PaneAgentStatusChanged).len() >= 5);
        let changes = read(&hub, EventKind::AgentPhaseChanged);
        assert_eq!(changes.len(), 1, "{changes:?}");
        let EventData::AgentPhaseChanged(event) = &changes[0].1.data else {
            panic!("{changes:?}");
        };
        assert_eq!((event.from, event.to), (Phase::Unknown, Phase::Working));
        assert!(read(&hub, EventKind::AgentNeedsHuman).is_empty());

        // A restart restores the agent's id with its session.
        let agent_id = app
            .state
            .terminals
            .values()
            .next()
            .unwrap()
            .agent_id
            .clone();
        let restarted = crate::api::EventHub::with_persistence(log.clone());
        let mut app = app_on(restarted.clone(), true);
        for terminal in app.state.terminals.values_mut() {
            terminal.agent_id = agent_id.clone();
        }
        status(&mut app, S::Working);
        tick(&mut app, 100);
        tick(&mut app, 130);
        assert_eq!(read(&restarted, EventKind::AgentPhaseChanged).len(), 1);
        assert!(read(&restarted, EventKind::AgentGone).is_empty());

        // The next start restores no pane for it: it is gone, once.
        let emptied = crate::api::EventHub::with_persistence(log.clone());
        let mut app = app_on(emptied.clone(), false);
        tick(&mut app, 200);
        tick(&mut app, 300);
        assert_eq!(read(&emptied, EventKind::AgentGone).len(), 1);
        let restarted = crate::api::EventHub::with_persistence(log.clone());
        let mut app = app_on(restarted.clone(), false);
        tick(&mut app, 400);
        assert_eq!(read(&restarted, EventKind::AgentGone).len(), 1);
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
    }

    /// An idle server with agents whose phases are announced has no reason
    /// to wake: the tracker adds no loop deadline.
    #[tokio::test]
    async fn an_idle_server_with_settled_agents_has_no_loop_deadline() {
        let mut app = app_on(crate::api::EventHub::default(), true);
        let now = Instant::now();
        status_at(
            &mut app,
            crate::detect::AgentState::Idle,
            now - Duration::from_secs(60),
        );
        app.tick_agent_phases(now);
        assert_eq!(app.agent_phases.next_deadline(now), None);
        let changes = read(&app.event_hub, EventKind::AgentPhaseChanged);
        assert_eq!(changes.len(), 1, "{changes:?}");
        app.tick_agent_phases(now + Duration::from_secs(60));
        assert_eq!(app.agent_phases.next_deadline(now), None);
    }
}
