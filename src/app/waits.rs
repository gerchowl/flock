//! The wait registry (ADR-0028 §5, #1031): what each local agent is waiting
//! for, so `state.waiting` reports it and `phase` reads `waiting_on_event`.
//!
//! A wait is registered in one of two ways, both through the server:
//!
//! - a held `events.read` (`flk events wait`, MCP `flock_events_read` with
//!   `wait_ms`), open while the request is held. A wait run as a harness
//!   background task (Claude `run_in_background`) is attributed the same way
//!   (§4), so that pane reads `waiting_on_event`, not
//!   `finished_with_background`;
//! - `wait.park`, which returns at once and stays open until its mandatory
//!   deadline (#1031 part B).
//!
//! Each is recorded against the attested caller, the agent whose pane the
//! caller's process ancestry runs in (`spawn_caller`, ADR-0014). A caller in
//! no agent pane holds no agent state, so its held read records nothing.
//!
//! A wait still open at its deadline is removed and announced once as a
//! durable `wait.expired`. A held read is released when its request returns,
//! so it expires only when that release never arrived. Opening, closing and
//! expiring a wait each mark the agent's pane for the derived phase events
//! (ADR-0028 §2), so `agent.phase_changed` follows the wait.
//!
//! Known limits. Opening a wait on its predicate, and the wake that follows,
//! are E6 (#1032). The registry is in memory, so a server restart or live
//! handoff drops open waits (#1240). A wait whose agent left stays until its
//! deadline.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::api::agent_state::Waiting;
use crate::api::schema::{EventData, EventEnvelope, EventKind, ResponseResult};
use crate::api::wait_predicate::WaitPredicate;

use super::api::responses::encode_success;
use super::{App, SpawnCaller};

/// How long a held read's wait outlives its hold before the registry gives
/// up on its release: the CLI's own slack over the server-side hold.
const HOLD_GRACE: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenWait {
    pub(crate) agent_id: String,
    /// The agent's pane when the wait opened, to mark for the phase events.
    pane: crate::layout::PaneId,
    pub(crate) predicate: WaitPredicate,
    /// Unix ms.
    pub(crate) since: u64,
    /// Unix ms, for the wire.
    pub(crate) deadline: u64,
    expires: Instant,
}

/// Open waits by id. Pure data, owned by the App loop.
#[derive(Debug, Default)]
pub(crate) struct WaitRegistry {
    waits: BTreeMap<String, OpenWait>,
    minted: u64,
}

impl WaitRegistry {
    /// Open a wait for `agent_id` that expires after `hold`. Its id and
    /// deadline (Unix ms), or `None` when `hold` overflows the clock.
    pub(crate) fn open(
        &mut self,
        agent_id: String,
        pane: crate::layout::PaneId,
        predicate: WaitPredicate,
        hold: Duration,
        now: Instant,
        now_ms: u64,
    ) -> Option<(String, u64)> {
        let expires = now.checked_add(hold)?;
        self.minted += 1;
        let wait_id = format!("w_{now_ms}_{}", self.minted);
        let deadline = now_ms.saturating_add(u64::try_from(hold.as_millis()).unwrap_or(u64::MAX));
        self.waits.insert(
            wait_id.clone(),
            OpenWait {
                agent_id,
                pane,
                predicate,
                since: now_ms,
                deadline,
                expires,
            },
        );
        Some((wait_id, deadline))
    }

    pub(crate) fn close(&mut self, wait_id: &str) -> Option<OpenWait> {
        self.waits.remove(wait_id)
    }

    /// The agent's oldest open wait, as `state.waiting` reports it.
    pub(crate) fn waiting(&self, agent_id: &str) -> Waiting {
        self.waits
            .iter()
            .filter(|(_, wait)| wait.agent_id == agent_id)
            .min_by_key(|(_, wait)| wait.since)
            .map_or(Waiting::None, |(wait_id, wait)| Waiting::On {
                wait_id: wait_id.clone(),
                predicate: wait.predicate.to_string(),
                since: wait.since,
                deadline: wait.deadline,
            })
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.waits.values().map(|wait| wait.expires).min()
    }

    /// Remove and return every wait whose deadline has passed.
    pub(crate) fn expire(&mut self, now: Instant) -> Vec<(String, OpenWait)> {
        if self.next_deadline().is_none_or(|due| due > now) {
            return Vec::new();
        }
        let (expired, open) = std::mem::take(&mut self.waits)
            .into_iter()
            .partition(|(_, wait)| wait.expires <= now);
        self.waits = open;
        expired.into_iter().collect()
    }
}

impl App {
    /// A held `events.read` opens its blocking wait. A caller in no agent
    /// pane records nothing.
    pub(super) fn handle_wait_hold(
        &mut self,
        id: String,
        predicate: WaitPredicate,
        hold_ms: u64,
    ) -> String {
        let wait_id = match self.spawn_caller() {
            SpawnCaller::Agent {
                agent_id, pane_id, ..
            } => self
                .open_wait(
                    agent_id,
                    pane_id,
                    predicate,
                    Duration::from_millis(hold_ms).saturating_add(HOLD_GRACE),
                )
                .map(|(wait_id, _)| wait_id),
            SpawnCaller::Operator { .. } => None,
        };
        encode_success(id, ResponseResult::WaitHeld { wait_id })
    }

    /// The held `events.read` returned: its wait closes.
    pub(super) fn handle_wait_release(&mut self, id: String, wait_id: &str) -> String {
        if let Some(wait) = self.waits.close(wait_id) {
            self.touch_agent_phase(wait.pane);
        }
        encode_success(id, ResponseResult::Ok {})
    }

    fn open_wait(
        &mut self,
        agent_id: String,
        pane: crate::layout::PaneId,
        predicate: WaitPredicate,
        hold: Duration,
    ) -> Option<(String, u64)> {
        let opened = self.waits.open(
            agent_id,
            pane,
            predicate,
            hold,
            Instant::now(),
            super::notifications::now_ms(),
        );
        self.touch_agent_phase(pane);
        opened
    }

    /// Announce and remove every wait whose deadline has passed. Mirrored in
    /// both server loops (#25).
    pub(crate) fn tick_waits(&mut self, now: Instant) {
        for (wait_id, wait) in self.waits.expire(now) {
            self.touch_agent_phase(wait.pane);
            self.emit_event(EventEnvelope {
                event: EventKind::WaitExpired,
                data: EventData::WaitExpired {
                    wait_id,
                    agent_id: wait.agent_id,
                    predicate: wait.predicate.to_string(),
                    since: wait.since,
                    deadline: wait.deadline,
                },
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::api::agent_state::{Phase, Waiting, SETTLE_MS};
    use crate::api::schema::{EventData, EventKind, Method, Request};
    use crate::api::wait_predicate::WaitPredicate;
    use crate::app::App;
    use crate::detect::Agent;
    use crate::events::AppEvent;

    /// A Claude pane whose turn ended with a `run_in_background` shell still
    /// running, read from its screen the way the detection task publishes it,
    /// with this test process standing in for the pane's child so the API
    /// caller is attested to it.
    fn settled_claude_with_a_background_shell() -> (App, String) {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("main")];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].focused_pane_id().expect("pane");
        let terminal_id = app.state.workspaces[0]
            .pane_state(pane_id)
            .expect("pane state")
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).expect("terminal");
        terminal.launch_argv = Some(vec!["claude".into()]);
        let agent_id = terminal.agent_id.to_string();
        app.test_pane_child_pids.insert(pane_id, std::process::id());
        app.current_api_peer_pid = Some(std::process::id());
        let screen = concat!(
            "\u{25cf} Started the wait in the background.\n\n",
            "\u{273b} Crunched for 4s \u{b7} done 4:32 PM \u{b7} 1 shell still running\n",
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n",
            "\u{276f} \n",
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n",
            "  \u{23f5}\u{23f5} bypass permissions on \u{b7} 1 shell \u{b7} \u{2190} for agents\n",
        );
        let detection = crate::detect::detect_agent(Some(Agent::Claude), screen);
        app.state.handle_app_event(AppEvent::StateChanged {
            pane_id,
            agent: Some(Agent::Claude),
            state: detection.state,
            activity: detection.activity,
            provider_limit: detection.provider_limit,
            visible_blocker: detection.visible_blocker,
            visible_idle: detection.visible_idle,
            visible_working: detection.visible_working,
            background_tasks: detection.background_tasks,
            needs_human: detection.needs_human,
            settled_with_background_shells: detection.settled_with_background_shells,
            process_exited: false,
            observed_at: Instant::now(),
        });
        let terminal = app.state.terminals.get_mut(&terminal_id).expect("terminal");
        terminal.state_changed_at =
            Instant::now().checked_sub(Duration::from_millis(SETTLE_MS + 1_000));
        (app, agent_id)
    }

    fn call(app: &mut App, method: Method) -> serde_json::Value {
        let response = app.handle_api_request(Request {
            id: "req".into(),
            method,
        });
        serde_json::from_str(&response).expect("json")
    }

    fn state(app: &mut App, agent_id: &str) -> (Phase, serde_json::Value) {
        let got = call(
            app,
            Method::AgentGet(crate::api::schema::AgentTarget {
                target: agent_id.into(),
            }),
        );
        let state = &got["result"]["agent"]["state"];
        let phase = serde_json::from_value(state["phase"].clone()).expect("phase");
        (phase, state["waiting"].clone())
    }

    fn any_events() -> WaitPredicate {
        WaitPredicate::Events {
            kinds: Vec::new(),
            subjects: Vec::new(),
            after: 0,
        }
    }

    /// Acceptance (#1031): `flk events wait` started under Claude
    /// `run_in_background` makes the pane `waiting_on_event`, not
    /// `finished_with_background`, for as long as the read is held.
    #[test]
    fn a_held_read_under_a_background_shell_reads_waiting_on_event() {
        let (mut app, agent_id) = settled_claude_with_a_background_shell();
        assert_eq!(state(&mut app, &agent_id).0, Phase::FinishedWithBackground);

        let predicate = WaitPredicate::Events {
            kinds: vec![EventKind::MessageQueued],
            subjects: Vec::new(),
            after: 7,
        };
        let held = call(&mut app, Method::WaitHold(predicate, 60_000));
        let wait_id = held["result"]["wait_id"].as_str().expect("a wait id");
        let (phase, waiting) = state(&mut app, &agent_id);
        assert_eq!(phase, Phase::WaitingOnEvent);
        assert_eq!(waiting["on"]["wait_id"], wait_id);
        assert_eq!(
            waiting["on"]["predicate"],
            "events after 7 kinds message_queued"
        );

        call(&mut app, Method::WaitRelease(wait_id.into()));
        let (phase, waiting) = state(&mut app, &agent_id);
        assert_eq!(phase, Phase::FinishedWithBackground);
        assert_eq!(waiting, "none");
    }

    /// The derived phase events follow the wait (ADR-0028 §2): opening it
    /// and closing it each mark the pane, and each phase is announced once it
    /// has held for the settle window.
    #[test]
    fn the_derived_phase_events_follow_a_held_read() {
        let (mut app, agent_id) = settled_claude_with_a_background_shell();
        let announced = |app: &App| -> Vec<Phase> {
            app.event_hub
                .events_after(0)
                .into_iter()
                .filter_map(|(_, event)| match event.data {
                    EventData::AgentPhaseChanged(change) if change.agent_id == agent_id => {
                        Some(change.to)
                    }
                    _ => None,
                })
                .collect()
        };
        let start = Instant::now();
        let settle = Duration::from_millis(SETTLE_MS);
        app.tick_agent_phases(start);
        assert_eq!(announced(&app), [Phase::FinishedWithBackground]);

        let held = call(&mut app, Method::WaitHold(any_events(), 60_000));
        app.tick_agent_phases(start);
        app.tick_agent_phases(start + settle);
        assert_eq!(
            announced(&app),
            [Phase::FinishedWithBackground, Phase::WaitingOnEvent]
        );

        let wait_id = held["result"]["wait_id"].as_str().expect("a wait id");
        call(&mut app, Method::WaitRelease(wait_id.into()));
        app.tick_agent_phases(start + settle);
        app.tick_agent_phases(start + settle * 2);
        assert_eq!(
            announced(&app),
            [
                Phase::FinishedWithBackground,
                Phase::WaitingOnEvent,
                Phase::FinishedWithBackground
            ]
        );
    }

    /// Acceptance (#1031): an expired deadline emits `wait.expired` once and
    /// clears the wait.
    /// A held read whose release never arrived (its connection thread died)
    /// expires at its hold plus the grace.
    #[test]
    fn a_held_read_whose_release_is_lost_expires_once() {
        let (mut app, agent_id) = settled_claude_with_a_background_shell();
        let held = call(&mut app, Method::WaitHold(any_events(), 1_000));
        let wait_id = held["result"]["wait_id"].clone();
        let due = app
            .waits
            .next_deadline()
            .expect("the hold is a loop deadline");
        assert!(
            due.saturating_duration_since(Instant::now()) > super::HOLD_GRACE,
            "the hold plus its grace"
        );

        app.tick_waits(due - Duration::from_millis(1));
        assert_eq!(state(&mut app, &agent_id).0, Phase::WaitingOnEvent);
        app.tick_waits(due);
        app.tick_waits(due + Duration::from_secs(1));

        let expired: Vec<_> = app
            .event_hub
            .events_after(0)
            .into_iter()
            .filter(|(_, event)| event.event == EventKind::WaitExpired)
            .collect();
        assert_eq!(expired.len(), 1, "announced once");
        let EventData::WaitExpired {
            wait_id: id,
            agent_id: owner,
            ..
        } = &expired[0].1.data
        else {
            panic!("wrong data");
        };
        assert_eq!((id.as_str(), owner), (wait_id.as_str().unwrap(), &agent_id));
        let (phase, waiting) = state(&mut app, &agent_id);
        assert_eq!(phase, Phase::FinishedWithBackground);
        assert_eq!(waiting, "none");
        assert_eq!(app.waits.next_deadline(), None);
    }

    /// A caller in no agent pane has no agent state to put a wait on.
    #[test]
    fn a_caller_outside_an_agent_pane_holds_nothing() {
        let (mut app, _) = settled_claude_with_a_background_shell();
        app.current_api_peer_pid = None;
        let held = call(&mut app, Method::WaitHold(any_events(), 1));
        assert_eq!(held["result"]["wait_id"], serde_json::Value::Null);
        assert_eq!(app.waits.next_deadline(), None);
    }

    /// With two waits open, the older one is reported.
    #[test]
    fn the_oldest_open_wait_is_reported() {
        let mut registry = super::WaitRegistry::default();
        let now = Instant::now();
        let hold = Duration::from_secs(60);
        let pane = crate::layout::PaneId::from_raw(1);
        let (first, _) = registry
            .open("a".into(), pane, any_events(), hold, now, 10)
            .unwrap();
        registry
            .open("a".into(), pane, any_events(), hold, now, 20)
            .unwrap();
        let Waiting::On { wait_id, .. } = registry.waiting("a") else {
            panic!("a wait is open");
        };
        assert_eq!(wait_id, first);
        assert_eq!(registry.waiting("b"), Waiting::None);
    }
}
