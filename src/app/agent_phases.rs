//! Derived supervisor events (ADR-0028 §2, #1027): `agent_phase_changed`,
//! and `agent_needs_human`, `agent_limit_reached`, `agent_gone` as kinds of
//! their own, computed once in the server from each local agent's `state`.
//!
//! The server samples every local agent's `state` (the one `agent.get`
//! reports) and announces a phase only once it has held for the settle
//! window (ADR-0028 §1, `SETTLE_MS`). A phase that flips back to the
//! announced one inside the window was never announced, so nothing is
//! emitted. The idle phases (`finished`, `finished_with_background`,
//! `waiting_on_event`) already include the window, because `phase` derives
//! them only once the idle turn has held that long, so they are announced
//! as soon as they are observed.
//!
//! `settling` and `unknown` are not supervisor phases: they never become a
//! candidate and never interrupt one. An agent that leaves the local agent
//! table (`agent.list`, for example because its pane closed) is `gone` at
//! once: there is no agent left to flip back.
//!
//! Known limits. Sampling runs every `SAMPLE_INTERVAL`, so a phase is
//! announced up to one interval after its window closes. The last announced
//! phase per agent is seeded from the durable log at start, so a restart or
//! a live handoff does not repeat it; an agent whose last announcement
//! rotated out of the log is announced again with `from: unknown`. Agents
//! that end together with the server (`flk server stop`) are not announced
//! `gone`, because no sample runs after their panes close.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::api::agent_state::{AgentState, Lifecycle, Phase, SETTLE_MS};
use crate::api::schema::{AgentPhaseEvent, EventData, EventEnvelope, EventKind};

/// How often the server samples agent state for derived events.
pub(crate) const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// The settle window a non-idle phase must hold before it is announced: the
/// one `phase` itself uses. Making it `[events] settle_ms` is #1152.
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
}

#[derive(Default)]
pub(crate) struct AgentPhaseTracker {
    agents: HashMap<String, Tracked>,
    /// The last phase the log announced per agent, consumed when the agent
    /// is first sampled after a start.
    seeded: HashMap<String, Phase>,
    next_sample: Option<Instant>,
}

/// Whether a phase is one a supervisor branches on.
fn is_supervisor_phase(phase: Phase) -> bool {
    !matches!(phase, Phase::Settling | Phase::Unknown)
}

/// How long `phase` must hold before it is announced.
fn window(phase: Phase, settle: Duration) -> Duration {
    match phase {
        Phase::Finished | Phase::FinishedWithBackground | Phase::WaitingOnEvent => Duration::ZERO,
        _ => settle,
    }
}

impl AgentPhaseTracker {
    /// Seed the last announced phase per agent from the durable log, so a
    /// restart neither repeats it nor announces a phase it already did.
    pub(crate) fn seed_from_events<'a>(&mut self, events: impl Iterator<Item = &'a EventEnvelope>) {
        for envelope in events {
            if let EventData::AgentPhaseChanged(event) = &envelope.data {
                if event.to == Phase::Gone {
                    self.seeded.remove(&event.agent_id);
                } else {
                    self.seeded.insert(event.agent_id.clone(), event.to);
                }
            }
        }
    }

    /// Whether a sample is due at `now`. Arms the next one when it is.
    pub(crate) fn sample_due(&mut self, now: Instant) -> bool {
        if self.next_sample.is_some_and(|at| now < at) {
            return false;
        }
        self.next_sample = Some(now + SAMPLE_INTERVAL);
        true
    }

    /// When the loop must wake for the next sample: only while an agent is
    /// tracked, so a server without agents stays quiet.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.next_sample.filter(|_| !self.agents.is_empty())
    }

    /// Fold one sample of every local agent and return the events it settles.
    pub(crate) fn observe(
        &mut self,
        now: Instant,
        settle: Duration,
        samples: Vec<Sample>,
    ) -> Vec<EventEnvelope> {
        let mut events = Vec::new();
        let mut present = std::collections::HashSet::new();
        for sample in samples {
            present.insert(sample.agent_id.clone());
            let phase = sample.state.phase;
            let tracked = self
                .agents
                .entry(sample.agent_id.clone())
                .or_insert_with(|| Tracked {
                    pane: sample.pane.clone(),
                    announced: self.seeded.remove(&sample.agent_id).unwrap_or_default(),
                    candidate: Phase::Unknown,
                    since: now,
                    state: sample.state.clone(),
                });
            tracked.pane = sample.pane;
            if !is_supervisor_phase(phase) {
                continue;
            }
            tracked.state = sample.state;
            if phase != tracked.candidate {
                tracked.candidate = phase;
                tracked.since = now;
            }
            let held = now.saturating_duration_since(tracked.since);
            if tracked.candidate != tracked.announced && held >= window(phase, settle) {
                events.extend(announce(&sample.agent_id, tracked, phase, held));
            }
        }
        let gone: Vec<String> = self
            .agents
            .keys()
            .filter(|agent_id| !present.contains(*agent_id))
            .cloned()
            .collect();
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

/// The `agent_phase_changed` for a settled phase, followed by the kind of
/// its own that `needs_human`, `limit_reached` and `gone` also carry.
fn announce(
    agent_id: &str,
    tracked: &mut Tracked,
    to: Phase,
    held: Duration,
) -> Vec<EventEnvelope> {
    // An idle phase is first seen as its window closes: its own `held_ms`
    // knows how long it held before this sample did.
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
    /// Sample every local agent and emit the derived events that settled.
    /// Mirrored in the headless and TUI loops (#25).
    pub(crate) fn tick_agent_phases(&mut self, now: Instant) {
        if !self.agent_phases.sample_due(now) {
            return;
        }
        let samples = self
            .collect_agent_infos()
            .into_iter()
            .filter_map(|info| {
                Some(Sample {
                    agent_id: info.agent_id,
                    pane: info.pane_id,
                    state: info.state?,
                })
            })
            .collect();
        for event in self.agent_phases.observe(now, SETTLE, samples) {
            self.emit_event(event);
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

    /// Feed `(seconds after start, phase)` samples of one agent, `None` for
    /// a sample without it, and return the kinds emitted with their `to`.
    fn run(
        tracker: &mut AgentPhaseTracker,
        steps: &[(u64, Option<Phase>)],
    ) -> Vec<(EventKind, Phase, Phase)> {
        let start = Instant::now();
        let mut out = Vec::new();
        for (at, phase) in steps {
            let samples = phase
                .map(|phase| Sample {
                    agent_id: "agent_a".into(),
                    pane: "w1:p1".into(),
                    state: state(phase),
                })
                .into_iter()
                .collect();
            for envelope in tracker.observe(start + Duration::from_secs(*at), WIN, samples) {
                let (EventData::AgentPhaseChanged(event)
                | EventData::AgentNeedsHuman(event)
                | EventData::AgentLimitReached(event)
                | EventData::AgentGone(event)) = envelope.data
                else {
                    panic!("unexpected {:?}", envelope.event);
                };
                out.push((envelope.event, event.from, event.to));
            }
        }
        out
    }

    #[test]
    fn a_phase_that_flips_back_inside_the_window_emits_nothing() {
        use Phase::*;
        let mut tracker = AgentPhaseTracker::default();
        let first = run(&mut tracker, &[(0, Some(Working)), (20, Some(Working))]);
        assert_eq!(first, [(EventKind::AgentPhaseChanged, Unknown, Working)]);
        // A prompt answered within the window, a short idle, an unknown read.
        let flips = run(
            &mut tracker,
            &[
                (0, Some(NeedsHuman)),
                (19, Some(NeedsHuman)),
                (19, Some(Working)),
                (30, Some(Settling)),
                (45, Some(Unknown)),
                (60, Some(Working)),
                (90, Some(Working)),
            ],
        );
        assert_eq!(flips, []);
    }

    #[test]
    fn each_real_transition_is_announced_once_with_its_own_kind() {
        use EventKind as K;
        use Phase::*;
        let mut tracker = AgentPhaseTracker::default();
        let events = run(
            &mut tracker,
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
                // An idle phase already includes the window.
                (K::AgentPhaseChanged, LimitReached, Finished),
                (K::AgentPhaseChanged, Finished, Gone),
                (K::AgentGone, Finished, Gone),
            ]
        );
    }

    fn status(app: &mut crate::app::App, state: crate::detect::AgentState) {
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
            observed_at: Instant::now(),
        });
    }

    fn app_on(hub: crate::api::EventHub) -> crate::app::App {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::new(&crate::config::Config::default(), true, None, rx, hub);
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("phases")];
        app.state.ensure_test_terminals();
        app
    }

    /// ADR-0028 §10 E3: real detector status changes, through the server's
    /// `PaneAgentStatusChanged` and the loop's sample, end at exactly one
    /// durable `agent_phase_changed` that `events.read`'s kind filter serves,
    /// and a restarted server on the same log does not repeat it.
    #[tokio::test]
    async fn a_real_status_sequence_settles_into_one_durable_phase_change() {
        use crate::detect::AgentState as S;
        let log = std::env::temp_dir()
            .join(format!("flock-phases-{}", std::process::id()))
            .join("event-log.jsonl");
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
        let hub = crate::api::EventHub::with_persistence(log.clone());
        let mut app = app_on(hub.clone());
        let start = Instant::now();
        let tick = |app: &mut crate::app::App, secs: u64| {
            app.tick_agent_phases(start + Duration::from_secs(secs));
        };
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
        let read = |hub: &crate::api::EventHub, kind: EventKind| {
            hub.read(0, 100, |envelope| envelope.event == kind).events
        };
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
        let mut app = app_on(restarted.clone());
        for terminal in app.state.terminals.values_mut() {
            terminal.agent_id = agent_id.clone();
        }
        status(&mut app, S::Working);
        tick(&mut app, 100);
        tick(&mut app, 130);
        assert_eq!(read(&restarted, EventKind::AgentPhaseChanged).len(), 1);
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
    }

    #[test]
    fn a_restart_does_not_repeat_the_phase_the_log_announced() {
        use Phase::*;
        let mut before = AgentPhaseTracker::default();
        let start = Instant::now();
        let sample = |phase| Sample {
            agent_id: "agent_a".into(),
            pane: "w1:p1".into(),
            state: state(phase),
        };
        let mut log = before.observe(start, WIN, vec![sample(Finished)]);
        assert_eq!(log.len(), 1);
        let mut after = AgentPhaseTracker::default();
        after.seed_from_events(log.iter());
        assert!(after.observe(start, WIN, vec![sample(Finished)]).is_empty());
        // An agent the log saw go is not seeded: a new one by that id starts fresh.
        log.extend(before.observe(start, WIN, vec![]));
        let mut again = AgentPhaseTracker::default();
        again.seed_from_events(log.iter());
        assert_eq!(again.observe(start, WIN, vec![sample(Finished)]).len(), 1);
    }
}
