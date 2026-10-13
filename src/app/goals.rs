//! The goal record (ADR-0030 §1, ADR-0031): one goal per agent, keyed by its
//! `AgentId` and never by a pane, so a pane move or a restart cannot detach it.
//!
//! The record is a fold over the durable `Goal*` events, held in memory and
//! never re-folded on a read (ADR-0031 §2). The fold runs once per server
//! start, in the same log read as the delegation edges (§3), and every open
//! goal is re-issued as a `GoalArmed { reissued: true }` copy carrying the
//! whole record, on start and after every log rotation (§4), so rotation
//! cannot drop a goal that has been quiet for a long time.
//!
//! Known limits: verdict history is only as long as the log keeps it (the
//! record holds the latest verdict); a met goal is kept in memory but no
//! longer re-issued, so it can rotate out; and a goal whose agent vanished
//! without a removal (a crash) is kept and re-issued, failing toward keeping
//! state, until the agent is removed or the goal is stopped.

use std::collections::BTreeMap;

use crate::api::schema::{EventData, EventEnvelope, EventKind, GoalRecord};

use super::App;

/// Goals by `AgentId`.
#[derive(Debug, Default)]
pub(crate) struct GoalIndex {
    goals: BTreeMap<String, GoalRecord>,
}

impl GoalIndex {
    /// Apply one event. Every arm sets state rather than accumulating it,
    /// so applying the same event twice leaves the record as applying it
    /// once does. Events for an agent with no goal are ignored.
    pub(crate) fn apply(&mut self, data: &EventData) {
        if let EventData::GoalArmed {
            agent_id, record, ..
        } = data
        {
            self.goals.insert(agent_id.clone(), record.clone());
            return;
        }
        if let EventData::GoalDisarmed { agent_id, .. } = data {
            self.goals.remove(agent_id);
            return;
        }
        let Some(record) = goal_agent(data).and_then(|agent_id| self.goals.get_mut(agent_id))
        else {
            return;
        };
        match data {
            EventData::GoalAmended { goal, checks, .. } => {
                record.goal = goal.clone();
                record.checks = checks.clone();
            }
            EventData::GoalStatusSet { status, .. } => record.status = Some(status.clone()),
            EventData::GoalSubtaskChanged { subtask, .. } => {
                match record.subtasks.iter_mut().find(|s| s.id == subtask.id) {
                    Some(existing) => *existing = subtask.clone(),
                    None => record.subtasks.push(subtask.clone()),
                }
            }
            EventData::GoalVerdict { verdict, .. } => record.last_verdict = Some(verdict.clone()),
            EventData::GoalMet { .. } => record.met = true,
            EventData::GoalPaused { until_ms, .. } => {
                record.paused = Some(crate::api::schema::GoalPause {
                    until_ms: *until_ms,
                });
            }
            EventData::GoalResumed { .. } => record.paused = None,
            _ => {}
        }
    }

    pub(crate) fn get(&self, agent_id: &str) -> Option<&GoalRecord> {
        self.goals.get(agent_id)
    }

    /// The `GoalArmed` copies that keep every open goal in the newest log
    /// file. A met goal is closed and is not re-issued.
    fn reissue_events(&self) -> Vec<EventEnvelope> {
        self.goals
            .iter()
            .filter(|(_, record)| !record.met)
            .map(|(agent_id, record)| EventEnvelope {
                event: EventKind::GoalArmed,
                data: EventData::GoalArmed {
                    agent_id: agent_id.clone(),
                    record: record.clone(),
                    reissued: true,
                },
            })
            .collect()
    }
}

/// The agent a `Goal*` event is about.
fn goal_agent(data: &EventData) -> Option<&str> {
    match data {
        EventData::GoalArmed { agent_id, .. }
        | EventData::GoalAmended { agent_id, .. }
        | EventData::GoalStatusSet { agent_id, .. }
        | EventData::GoalSubtaskChanged { agent_id, .. }
        | EventData::GoalVerdict { agent_id, .. }
        | EventData::GoalMet { agent_id }
        | EventData::GoalPaused { agent_id, .. }
        | EventData::GoalResumed { agent_id }
        | EventData::GoalDisarmed { agent_id, .. } => Some(agent_id),
        _ => None,
    }
}

impl App {
    /// The one write path for goal state: append the event, then fold it
    /// into the live record, so the record is always the fold of the log.
    pub(crate) fn record_goal_event(&mut self, event: EventEnvelope) {
        self.goals.apply(&event.data);
        self.emit_event(event);
    }

    /// Server start: fold the log read the delegation pass already made and
    /// re-issue every open goal into the newest file.
    pub(super) fn reissue_open_goals(&mut self, restored: &[(u64, u64, EventEnvelope)]) {
        let mut index = GoalIndex::default();
        for (_, _, envelope) in restored {
            index.apply(&envelope.data);
        }
        self.goals = index;
        self.emit_goal_reissues();
    }

    pub(super) fn emit_goal_reissues(&mut self) {
        for event in self.goals.reissue_events() {
            self.emit_event(event);
        }
    }

    /// An `AgentId` is retired on removal and never comes back, so its goal
    /// is closed durably here, whether or not the node logs `AgentRemoved`.
    pub(super) fn disarm_removed_agent_goal(&mut self, agent_id: &str) {
        if self.goals.get(agent_id).is_some() {
            self.record_goal_event(EventEnvelope {
                event: EventKind::GoalDisarmed,
                data: EventData::GoalDisarmed {
                    agent_id: agent_id.to_string(),
                    reason: Some("agent removed".into()),
                },
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{GoalDecision, GoalPause, GoalSubtask, GoalVerdictRecord};
    use crate::api::EventHub;
    use crate::app::delegation::handler_tests::{app_on, caller_is_an_agent, shutdown, temp_log};

    const AGENT: &str = "agent_goal";

    fn event(event: EventKind, data: EventData) -> EventEnvelope {
        EventEnvelope { event, data }
    }

    fn armed(agent_id: &str) -> EventEnvelope {
        event(
            EventKind::GoalArmed,
            EventData::GoalArmed {
                agent_id: agent_id.into(),
                record: GoalRecord {
                    goal: "ship G1a".into(),
                    checks: vec!["just check".into()],
                    ..Default::default()
                },
                reissued: false,
            },
        )
    }

    fn subtask(id: &str, done: bool, owner: Option<&str>) -> EventEnvelope {
        event(
            EventKind::GoalSubtaskChanged,
            EventData::GoalSubtaskChanged {
                agent_id: AGENT.into(),
                subtask: GoalSubtask {
                    id: id.into(),
                    text: format!("step {id}"),
                    done,
                    owner: owner.map(str::to_string),
                },
            },
        )
    }

    fn verdict() -> GoalVerdictRecord {
        GoalVerdictRecord {
            decision: GoalDecision::Allow,
            rung: 1,
            reason: Some("paused".into()),
        }
    }

    /// Every kind but met and disarm, in the order a goal lives through them.
    fn open_script() -> Vec<EventEnvelope> {
        vec![
            armed(AGENT),
            event(
                EventKind::GoalAmended,
                EventData::GoalAmended {
                    agent_id: AGENT.into(),
                    goal: "ship G1a and G1b".into(),
                    checks: vec!["just check".into(), "just lint".into()],
                },
            ),
            event(
                EventKind::GoalStatusSet,
                EventData::GoalStatusSet {
                    agent_id: AGENT.into(),
                    status: "fold written".into(),
                },
            ),
            subtask("1", false, None),
            subtask("2", false, Some("agent_delegate")),
            subtask("1", true, None),
            event(
                EventKind::GoalVerdict,
                EventData::GoalVerdict {
                    agent_id: AGENT.into(),
                    verdict: verdict(),
                },
            ),
            event(
                EventKind::GoalResumed,
                EventData::GoalResumed {
                    agent_id: AGENT.into(),
                },
            ),
            event(
                EventKind::GoalPaused,
                EventData::GoalPaused {
                    agent_id: AGENT.into(),
                    until_ms: Some(5_000),
                },
            ),
        ]
    }

    fn expected_open_record() -> GoalRecord {
        GoalRecord {
            goal: "ship G1a and G1b".into(),
            checks: vec!["just check".into(), "just lint".into()],
            status: Some("fold written".into()),
            subtasks: vec![
                GoalSubtask {
                    id: "1".into(),
                    text: "step 1".into(),
                    done: true,
                    owner: None,
                },
                GoalSubtask {
                    id: "2".into(),
                    text: "step 2".into(),
                    done: false,
                    owner: Some("agent_delegate".into()),
                },
            ],
            paused: Some(GoalPause {
                until_ms: Some(5_000),
            }),
            met: false,
            last_verdict: Some(verdict()),
        }
    }

    fn fold(events: &[EventEnvelope]) -> GoalIndex {
        let mut index = GoalIndex::default();
        for envelope in events {
            index.apply(&envelope.data);
        }
        index
    }

    fn goal_events(hub: &EventHub) -> Vec<EventData> {
        hub.persisted_events_after(0)
            .into_iter()
            .map(|(_, _, envelope)| envelope.data)
            .filter(|data| goal_agent(data).is_some())
            .collect()
    }

    #[test]
    fn each_kind_folds_into_the_record() {
        assert_eq!(
            fold(&open_script()).get(AGENT),
            Some(&expected_open_record())
        );

        let mut events = open_script();
        events.push(event(
            EventKind::GoalResumed,
            EventData::GoalResumed {
                agent_id: AGENT.into(),
            },
        ));
        events.push(event(
            EventKind::GoalMet,
            EventData::GoalMet {
                agent_id: AGENT.into(),
            },
        ));
        let index = fold(&events);
        let record = index.get(AGENT).expect("a met goal stays in the record");
        assert!(record.met && record.paused.is_none());
        assert!(index.reissue_events().is_empty(), "a met goal is closed");

        events.push(event(
            EventKind::GoalDisarmed,
            EventData::GoalDisarmed {
                agent_id: AGENT.into(),
                reason: None,
            },
        ));
        assert_eq!(fold(&events).get(AGENT), None);
    }

    #[test]
    fn events_for_an_agent_without_a_goal_change_nothing() {
        let mut events = open_script();
        events.remove(0);
        assert_eq!(fold(&events).get(AGENT), None);
    }

    /// A duplicated line, from a retried write or a re-issue next to its
    /// original, must not change the record.
    #[test]
    fn a_duplicated_event_folds_to_the_same_record() {
        let script = open_script();
        for duplicated in 0..script.len() {
            let mut events = script.clone();
            events.insert(duplicated, script[duplicated].clone());
            assert_eq!(
                fold(&events).get(AGENT),
                Some(&expected_open_record()),
                "duplicating event {duplicated}"
            );
        }
        let mut with_reissue = script.clone();
        with_reissue.extend(fold(&script).reissue_events());
        with_reissue.extend(fold(&script).reissue_events());
        assert_eq!(
            fold(&with_reissue).get(AGENT),
            Some(&expected_open_record())
        );
    }

    /// A restart (or a live handoff) folds the record back from the log and
    /// re-issues it, in the same log read as the delegation edges.
    #[tokio::test]
    async fn a_server_start_refolds_the_record_and_reissues_it() {
        let log = temp_log("goal-restart");
        let hub = EventHub::with_persistence(log.clone());
        let mut app = app_on(hub);
        for envelope in open_script() {
            app.record_goal_event(envelope);
        }
        assert_eq!(app.goals.get(AGENT), Some(&expected_open_record()));
        shutdown(app);

        let hub = EventHub::with_persistence(log.clone());
        let mut app = app_on(hub.clone());
        assert_eq!(
            app.goals.get(AGENT),
            None,
            "folded at start, not in App::new"
        );
        app.reissue_open_records();
        assert_eq!(app.goals.get(AGENT), Some(&expected_open_record()));
        assert_eq!(
            goal_events(&hub).last(),
            Some(&EventData::GoalArmed {
                agent_id: AGENT.into(),
                record: expected_open_record(),
                reissued: true,
            })
        );
        shutdown(app);
        let _ = std::fs::remove_dir_all(log.parent().expect("log dir"));
    }

    /// Each start re-issues every open goal exactly once, so a second
    /// restart in a row adds one more copy per open goal, not one per copy
    /// already in the log. A met goal is not re-issued.
    #[tokio::test]
    async fn two_restarts_in_a_row_each_reissue_one_copy_per_open_goal() {
        const MET: &str = "agent_met";
        let log = temp_log("goal-two-restarts");
        let mut app = app_on(EventHub::with_persistence(log.clone()));
        for envelope in open_script() {
            app.record_goal_event(envelope);
        }
        app.record_goal_event(armed("agent_second"));
        app.record_goal_event(armed(MET));
        app.record_goal_event(event(
            EventKind::GoalMet,
            EventData::GoalMet {
                agent_id: MET.into(),
            },
        ));
        shutdown(app);

        let reissued = |hub: &EventHub, agent: &str| {
            goal_events(hub)
                .iter()
                .filter(|data| {
                    matches!(
                        data,
                        EventData::GoalArmed {
                            agent_id,
                            reissued: true,
                            ..
                        } if agent_id == agent
                    )
                })
                .count()
        };
        for restart in 1..=2 {
            let hub = EventHub::with_persistence(log.clone());
            let mut app = app_on(hub.clone());
            app.reissue_open_records();
            assert_eq!(reissued(&hub, AGENT), restart, "restart {restart}");
            assert_eq!(reissued(&hub, "agent_second"), restart, "restart {restart}");
            assert_eq!(reissued(&hub, MET), 0, "a met goal is closed");
            assert_eq!(app.goals.get(AGENT), Some(&expected_open_record()));
            shutdown(app);
        }
        let _ = std::fs::remove_dir_all(log.parent().expect("log dir"));
    }

    /// ADR-0031's acceptance probe, pane move: moving an agent's pane into
    /// another workspace re-scopes its pane id but keeps its `AgentId`, so
    /// the goal stays with it, writes nothing, and refolds after a restart.
    #[tokio::test]
    async fn a_pane_move_keeps_the_goal_with_its_agent() {
        let log = temp_log("goal-pane-move");
        let hub = EventHub::with_persistence(log.clone());
        let mut app = app_on(hub.clone());
        app.state
            .workspaces
            .push(crate::workspace::Workspace::test_new("other"));
        app.state.ensure_test_terminals();
        let agent = caller_is_an_agent(&mut app);
        app.record_goal_event(armed(&agent));
        let record = app.goals.get(&agent).cloned().expect("armed");
        let written = goal_events(&hub).len();

        let pane = app.state.workspaces[0].tabs[0].root_pane;
        let pane_id = app.pane_info(0, pane).expect("pane info").pane_id;
        let tab_id = app.public_tab_id(1, 0).expect("target tab");
        let response = app.handle_api_request(crate::api::schema::Request {
            id: "req".into(),
            method: crate::api::schema::Method::PaneMove(crate::api::schema::PaneMoveParams {
                pane_id: pane_id.clone(),
                destination: crate::api::schema::PaneMoveDestination::Tab {
                    tab_id,
                    target_pane_id: None,
                    split: crate::api::schema::SplitDirection::Right,
                    ratio: None,
                },
                focus: false,
            }),
        });
        let response: serde_json::Value = serde_json::from_str(&response).expect("json");
        let moved = &response["result"]["move_result"]["pane"];
        assert_ne!(moved["pane_id"], serde_json::json!(pane_id), "{response}");
        let terminal = app
            .state
            .terminals
            .iter()
            .find(|(id, _)| serde_json::json!(id.to_string()) == moved["terminal_id"])
            .map(|(_, terminal)| terminal.agent_id.to_string());
        assert_eq!(
            terminal,
            Some(agent.clone()),
            "the moved pane is the same agent"
        );

        assert_eq!(app.goals.get(&agent), Some(&record));
        assert_eq!(
            goal_events(&hub).len(),
            written,
            "a move writes no goal event"
        );
        shutdown(app);

        let mut app = app_on(EventHub::with_persistence(log.clone()));
        app.reissue_open_records();
        assert_eq!(app.goals.get(&agent), Some(&record));
        shutdown(app);
        let _ = std::fs::remove_dir_all(log.parent().expect("log dir"));
    }

    /// ADR-0031's downgrade recovery: a 1.x server quarantined the active
    /// file over its goal lines, then numbered on above them and rotated its
    /// fresh file to `.1`. Putting the quarantined lines in front of the
    /// active file brings every event back in sequence order, and the goal is
    /// live in the record and re-issued after the next start.
    #[tokio::test]
    async fn a_log_a_1x_server_quarantined_is_recovered_with_its_goals_live() {
        let log = temp_log("goal-downgrade-recovery");
        let mut app = app_on(EventHub::with_persistence(log.clone()));
        for envelope in open_script() {
            app.record_goal_event(envelope);
        }
        shutdown(app);
        let goal_lines = open_script().len() as u64;

        let quarantined = log.with_file_name("event-log.corrupt.jsonl");
        std::fs::rename(&log, &quarantined).expect("quarantine");
        let line = |seq: u64, workspace_id: &str| {
            let envelope = event(
                EventKind::WorkspaceClosed,
                EventData::WorkspaceClosed {
                    workspace_id: workspace_id.into(),
                },
            );
            format!(
                "{}\n",
                serde_json::json!({ "seq": seq, "ts_ms": 1, "envelope": envelope })
            )
        };
        std::fs::write(
            log.with_file_name("event-log.1.jsonl"),
            line(goal_lines + 1, "before-rotation"),
        )
        .expect("rotated 1.x file");
        let fresh = line(goal_lines + 2, "after-rotation");

        let mut merged = std::fs::read_to_string(&quarantined).expect("quarantined lines");
        merged.push_str(&fresh);
        std::fs::write(&log, merged).expect("merged");
        std::fs::remove_file(&quarantined).expect("remove quarantined");

        let hub = EventHub::with_persistence(log.clone());
        let seqs: Vec<u64> = hub
            .persisted_events_after(0)
            .into_iter()
            .map(|(seq, _, _)| seq)
            .collect();
        assert_eq!(seqs, (1..=goal_lines + 2).collect::<Vec<_>>());
        let mut app = app_on(hub.clone());
        app.reissue_open_records();
        assert_eq!(app.goals.get(AGENT), Some(&expected_open_record()));
        assert_eq!(
            goal_events(&hub).last(),
            Some(&EventData::GoalArmed {
                agent_id: AGENT.into(),
                record: expected_open_record(),
                reissued: true,
            })
        );
        assert_eq!(hub.current_sequence(), goal_lines + 3, "no seq reused");
        shutdown(app);
        let _ = std::fs::remove_dir_all(log.parent().expect("log dir"));
    }

    /// ADR-0031 §4: a goal that writes nothing (parked) outlives more
    /// rotations than the log keeps files, because each rotation re-issues it.
    #[tokio::test]
    async fn an_open_goal_survives_rotating_past_every_kept_file() {
        let log = temp_log("goal-rotation");
        let hub = EventHub::with_persistence(log.clone());
        let mut app = app_on(hub.clone());
        for envelope in open_script() {
            app.record_goal_event(envelope);
        }
        // One more rotation than the log keeps files (MAX_ROTATED = 4), so
        // every line written before the first one is gone.
        for round in 0..6 {
            hub.rotate_on_next_push();
            app.emit_event(event(
                EventKind::WorkspaceClosed,
                EventData::WorkspaceClosed {
                    workspace_id: format!("filler-{round}"),
                },
            ));
            app.reissue_records_after_rotation();
        }
        assert_eq!(hub.rotations(), 6);
        assert!(
            !goal_events(&hub).iter().any(|data| matches!(
                data,
                EventData::GoalArmed {
                    reissued: false,
                    ..
                }
            )),
            "the original arm has rotated out"
        );
        shutdown(app);

        let mut app = app_on(EventHub::with_persistence(log.clone()));
        app.reissue_open_records();
        assert_eq!(app.goals.get(AGENT), Some(&expected_open_record()));
        shutdown(app);
        let _ = std::fs::remove_dir_all(log.parent().expect("log dir"));
    }

    /// An `AgentId` is retired on removal, so its goal is disarmed durably
    /// and not re-issued by the next server.
    #[tokio::test]
    async fn removing_an_agent_disarms_its_goal() {
        let log = temp_log("goal-removed");
        let hub = EventHub::with_persistence(log.clone());
        let mut app = app_on(hub.clone());
        let agent = caller_is_an_agent(&mut app);
        app.record_goal_event(armed(&agent));
        let pane_id = app.state.workspaces[0]
            .focused_pane_id()
            .expect("workspace has a pane");
        app.remove_agent_for_pane(0, pane_id, crate::app::agent_removal::RemovalEvent::Kill);

        assert_eq!(app.goals.get(&agent), None);
        assert!(matches!(
            goal_events(&hub).last(),
            Some(EventData::GoalDisarmed { agent_id, .. }) if *agent_id == agent
        ));
        shutdown(app);
        let _ = std::fs::remove_dir_all(log.parent().expect("log dir"));
    }
}
