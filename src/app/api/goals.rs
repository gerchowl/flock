//! `goal.arm`, `goal.status`, `goal.pause`, `goal.resume` and `goal.stop`
//! (ADR-0030 §1, G1b #1155): the verbs over the goal record G1a folds
//! (`app::goals`). Every write is one durable `Goal*` event through
//! `record_goal_event`, so the record stays the fold of the log.
//!
//! Who may call: `goal.status` is a read and open to every caller. The four
//! writes are open to the operator and to the goal's own agent, the way
//! goal-s1 let an agent arm, pause and stop its own goal and ADR-0030 §6 has
//! the agent record its own status. Another agent may not change a goal that
//! is not its own (a supervisor arms a delegate's goal through
//! `delegate start --goal`, G6 #965), and a caller this node cannot attest
//! (no readable pid, or relayed from a mesh peer: goals are per host,
//! ADR-0030 §7) is refused. The agent check reads process ancestry, so it is
//! cooperative in the way `owner.bind`'s is (ADR-0035 §6.3).
//!
//! Known limit: an expired pause (`until_ms` in the past) stays in the record
//! until the next pause or resume; the stop ladder (G1d #1157) reads it as
//! over, and `goal.status` reports the stored record as is.

use super::responses::{encode_error, encode_error_body, encode_success};
use super::spawn::SpawnCaller;
use crate::api::schema::{
    EventData, EventEnvelope, EventKind, GoalArmParams, GoalPauseParams, GoalRecord,
    GoalStopParams, GoalTargetParams, ResponseResult,
};
use crate::app::App;

/// The encoded error response a handler returns as is.
type Refusal = String;

impl App {
    pub(super) fn goal_arm(&mut self, id: &str, params: GoalArmParams) -> Result<String, Refusal> {
        let goal = params.goal.trim();
        if goal.is_empty() || params.checks.iter().any(|check| check.trim().is_empty()) {
            return Err(encode_error(
                id.into(),
                "invalid_params",
                "the goal and every check must be non-empty",
            ));
        }
        let agent_id = self.goal_writer(id, &params.target, "goal.arm")?;
        if self.goals.get(&agent_id).is_some_and(|record| !record.met) {
            let message =
                format!("{agent_id} already has an open goal; stop it first with `flk goal stop`");
            return Err(encode_error(id.into(), "goal_already_armed", message));
        }
        let record = GoalRecord {
            goal: goal.to_string(),
            checks: params.checks,
            ..Default::default()
        };
        let data = EventData::GoalArmed {
            agent_id: agent_id.clone(),
            record,
            reissued: false,
        };
        self.write_goal(id, agent_id, EventKind::GoalArmed, data)
    }

    pub(super) fn goal_status(
        &mut self,
        id: &str,
        params: GoalTargetParams,
    ) -> Result<String, Refusal> {
        let agent_id = self.goal_agent_id(id, &params.target)?;
        let record = self
            .goals
            .get(&agent_id)
            .cloned()
            .ok_or_else(|| not_armed(id, &agent_id))?;
        Ok(encode_success(
            id.into(),
            ResponseResult::Goal {
                agent_id,
                goal: record,
            },
        ))
    }

    pub(super) fn goal_pause(
        &mut self,
        id: &str,
        params: GoalPauseParams,
    ) -> Result<String, Refusal> {
        let until_ms = match params.minutes {
            None => None,
            Some(minutes) => Some(
                minutes
                    .checked_mul(60_000)
                    .filter(|ms| *ms > 0)
                    .and_then(|ms| crate::app::notifications::now_ms().checked_add(ms))
                    .ok_or_else(|| {
                        encode_error(
                            id.into(),
                            "invalid_params",
                            "minutes must be a positive count",
                        )
                    })?,
            ),
        };
        let agent_id = self.open_goal_writer(id, &params.target, "goal.pause")?;
        let data = EventData::GoalPaused {
            agent_id: agent_id.clone(),
            until_ms,
        };
        self.write_goal(id, agent_id, EventKind::GoalPaused, data)
    }

    pub(super) fn goal_resume(
        &mut self,
        id: &str,
        params: GoalTargetParams,
    ) -> Result<String, Refusal> {
        let agent_id = self.open_goal_writer(id, &params.target, "goal.resume")?;
        if self
            .goals
            .get(&agent_id)
            .is_some_and(|record| record.paused.is_none())
        {
            let message = format!("the goal of {agent_id} is not paused");
            return Err(encode_error(id.into(), "goal_not_paused", message));
        }
        let data = EventData::GoalResumed {
            agent_id: agent_id.clone(),
        };
        self.write_goal(id, agent_id, EventKind::GoalResumed, data)
    }

    /// Stop drops the record, met or open, and answers with it as it was.
    pub(super) fn goal_stop(
        &mut self,
        id: &str,
        params: GoalStopParams,
    ) -> Result<String, Refusal> {
        let agent_id = self.goal_writer(id, &params.target, "goal.stop")?;
        let response = self.goal_status(
            id,
            GoalTargetParams {
                target: agent_id.clone(),
            },
        )?;
        let reason = params.reason.filter(|reason| !reason.trim().is_empty());
        let data = EventData::GoalDisarmed { agent_id, reason };
        self.record_goal_event(EventEnvelope {
            event: EventKind::GoalDisarmed,
            data,
        });
        Ok(response)
    }

    /// Append one goal event and answer with the record it leaves.
    fn write_goal(
        &mut self,
        id: &str,
        agent_id: String,
        event: EventKind,
        data: EventData,
    ) -> Result<String, Refusal> {
        self.record_goal_event(EventEnvelope { event, data });
        self.goal_status(id, GoalTargetParams { target: agent_id })
    }

    /// [`Self::goal_writer`] for a verb that needs an open goal: one that is
    /// armed and not yet met.
    fn open_goal_writer(&mut self, id: &str, target: &str, verb: &str) -> Result<String, Refusal> {
        let agent_id = self.goal_writer(id, target, verb)?;
        match self.goals.get(&agent_id) {
            Some(record) if !record.met => Ok(agent_id),
            Some(_) => {
                let message = format!(
                    "the goal of {agent_id} is met and closed; arm a new one with `flk goal arm`"
                );
                Err(encode_error(id.into(), "goal_not_armed", message))
            }
            None => Err(not_armed(id, &agent_id)),
        }
    }

    /// The `AgentId` a write is about, once the caller may write it (see the
    /// module doc).
    fn goal_writer(&mut self, id: &str, target: &str, verb: &str) -> Result<String, Refusal> {
        let agent_id = self.goal_agent_id(id, target)?;
        let why = if self.caller_unattested_or_relayed() {
            "this node cannot attest the caller (no readable pid, or relayed from a mesh peer), and goals are per host".to_string()
        } else {
            match self.spawn_caller() {
                SpawnCaller::Agent {
                    agent_id: caller, ..
                } if caller != agent_id => {
                    format!("the caller is agent {caller}, and an agent may change only its own goal; the operator may change any")
                }
                _ => return Ok(agent_id),
            }
        };
        let message = format!("{verb} on the goal of {agent_id}: {why}");
        Err(encode_error(id.into(), "goal_caller_refused", message))
    }

    /// Resolve a goal target to an `AgentId`: `self` (or empty) is the
    /// caller's own agent, an `AgentId` with a goal names it even after its
    /// pane is gone, and anything else resolves as an agent target does.
    fn goal_agent_id(&mut self, id: &str, target: &str) -> Result<String, Refusal> {
        let target = match target.trim() {
            "" => "self",
            target => target,
        };
        if target == "self" {
            return match self.spawn_caller() {
                SpawnCaller::Agent { agent_id, .. } => Ok(agent_id),
                SpawnCaller::Operator { .. } => Err(encode_error(
                    id.into(),
                    "no_caller_pane",
                    "`self` is the agent whose pane runs the caller, and this caller runs in no agent pane: name the agent with --agent",
                )),
            };
        }
        if self.goals.get(target).is_some() {
            return Ok(target.to_string());
        }
        let resolved = self
            .resolve_terminal_target(target)
            .map_err(|err| encode_error_body(id.into(), self.agent_target_error_body(err)))?;
        self.state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|ws| ws.pane_state(resolved.pane_id))
            .and_then(|pane| self.state.terminals.get(&pane.attached_terminal_id))
            .filter(|terminal| terminal.is_agent_terminal())
            .map(|terminal| terminal.agent_id.to_string())
            .filter(|agent_id| !agent_id.is_empty())
            .ok_or_else(|| {
                let message = format!("target {target} runs no agent, so it has no goal");
                encode_error(id.into(), "invalid_agent", message)
            })
    }
}

fn not_armed(id: &str, agent_id: &str) -> String {
    let message = format!("{agent_id} has no goal; arm one with `flk goal arm`");
    encode_error(id.into(), "goal_not_armed", message)
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use crate::api::schema::{
        EventData, EventEnvelope, EventKind, GoalArmParams, GoalPauseParams, GoalRecord,
        GoalStopParams, GoalTargetParams, Method, Request,
    };
    use crate::api::EventHub;
    use crate::app::delegation::handler_tests::{app_on, caller_is_an_agent, shutdown, temp_log};
    use crate::app::App;

    fn call(app: &mut App, method: Method) -> Value {
        let response = app.handle_api_request(Request {
            id: "req".into(),
            method,
        });
        serde_json::from_str(&response).expect("json response")
    }

    fn arm(target: &str, goal: &str) -> Method {
        Method::GoalArm(GoalArmParams {
            target: target.into(),
            goal: goal.into(),
            checks: vec!["just check".into()],
        })
    }

    fn status(target: &str) -> Method {
        Method::GoalStatus(GoalTargetParams {
            target: target.into(),
        })
    }

    fn pause(target: &str, minutes: Option<u64>) -> Method {
        Method::GoalPause(GoalPauseParams {
            target: target.into(),
            minutes,
        })
    }

    fn resume(target: &str) -> Method {
        Method::GoalResume(GoalTargetParams {
            target: target.into(),
        })
    }

    fn stop(target: &str) -> Method {
        Method::GoalStop(GoalStopParams {
            target: target.into(),
            reason: Some("done by hand".into()),
        })
    }

    fn goal_lines(hub: &EventHub) -> Vec<EventData> {
        hub.persisted_events_after(0)
            .into_iter()
            .map(|(_, _, envelope)| envelope.data)
            .filter(|data| {
                matches!(
                    data,
                    EventData::GoalArmed { .. }
                        | EventData::GoalPaused { .. }
                        | EventData::GoalResumed { .. }
                        | EventData::GoalDisarmed { .. }
                )
            })
            .collect()
    }

    fn code(response: &Value) -> &str {
        response["error"]["code"].as_str().unwrap_or("<success>")
    }

    /// The acceptance round trip: an agent arms, pauses and resumes its own
    /// goal through `self`, each write is one durable event, the record
    /// survives a server restart, and stop drops it for good.
    #[tokio::test]
    async fn arm_pause_resume_stop_round_trip_and_survive_a_restart() {
        let log = temp_log("goal-verbs-round-trip");
        let hub = EventHub::with_persistence(log.clone());
        let mut app = app_on(hub.clone());
        let agent = caller_is_an_agent(&mut app);

        let armed = call(&mut app, arm("self", "  ship G1b  "));
        assert_eq!(armed["result"]["type"], "goal", "{armed}");
        assert_eq!(armed["result"]["agent_id"], agent.as_str());
        assert_eq!(armed["result"]["goal"]["goal"], "ship G1b", "trimmed");
        assert_eq!(armed["result"]["goal"]["checks"][0], "just check");

        let before = crate::app::notifications::now_ms();
        let paused = call(&mut app, pause("", Some(30)));
        let until_ms = paused["result"]["goal"]["paused"]["until_ms"]
            .as_u64()
            .expect("timed pause");
        assert!(until_ms >= before + 30 * 60_000, "{paused}");
        assert!(call(&mut app, resume("self"))["result"]["goal"]
            .get("paused")
            .is_none());
        let paused = call(&mut app, pause("self", None));
        assert_eq!(paused["result"]["goal"]["paused"], serde_json::json!({}));
        let read = call(&mut app, status("self"));
        assert_eq!(read["result"], paused["result"]);
        assert_eq!(goal_lines(&hub).len(), 4, "one event per write");
        shutdown(app);

        let hub = EventHub::with_persistence(log.clone());
        let mut app = app_on(hub.clone());
        app.reissue_open_records();
        app.current_api_peer_pid = Some(std::process::id());
        let after_restart = call(&mut app, status(&agent));
        assert_eq!(after_restart["result"], paused["result"], "{after_restart}");

        let stopped = call(&mut app, stop(&agent));
        assert_eq!(stopped["result"]["goal"], paused["result"]["goal"]);
        assert_eq!(
            goal_lines(&hub).last(),
            Some(&EventData::GoalDisarmed {
                agent_id: agent.clone(),
                reason: Some("done by hand".into()),
            })
        );
        shutdown(app);

        let mut app = app_on(EventHub::with_persistence(log.clone()));
        app.reissue_open_records();
        assert_eq!(
            app.goals.get(&agent),
            None,
            "a stopped goal is not refolded"
        );
        shutdown(app);
        let _ = std::fs::remove_dir_all(log.parent().expect("log dir"));
    }

    /// The operator may change any agent's goal, named by agent target, and
    /// an `AgentId` whose goal outlived its pane still names it.
    #[tokio::test]
    async fn the_operator_writes_any_goal_including_one_without_a_pane() {
        let log = temp_log("goal-verbs-operator");
        let hub = EventHub::with_persistence(log.clone());
        let mut app = app_on(hub.clone());
        let agent = caller_is_an_agent(&mut app);
        app.test_pane_child_pids.clear();

        assert_eq!(
            code(&call(&mut app, arm(&agent, "operator goal"))),
            "<success>"
        );
        app.record_goal_event(EventEnvelope {
            event: EventKind::GoalArmed,
            data: EventData::GoalArmed {
                agent_id: "agent_paneless".into(),
                record: GoalRecord {
                    goal: "orphaned".into(),
                    ..Default::default()
                },
                reissued: false,
            },
        });
        for target in [agent.as_str(), "agent_paneless"] {
            for method in [pause(target, Some(5)), resume(target), stop(target)] {
                assert_eq!(code(&call(&mut app, method)), "<success>", "{target}");
            }
        }
        assert_eq!(app.goals.get(&agent), None);
        assert_eq!(app.goals.get("agent_paneless"), None);
        shutdown(app);
        let _ = std::fs::remove_dir_all(log.parent().expect("log dir"));
    }

    /// An agent may change only its own goal, an unattested caller none, and
    /// `self` needs an agent pane. Every refusal writes nothing, and status
    /// stays open to every caller.
    #[tokio::test]
    async fn writes_by_another_agent_or_an_unattested_caller_are_refused() {
        let log = temp_log("goal-verbs-refused");
        let hub = EventHub::with_persistence(log.clone());
        let mut app = app_on(hub.clone());
        let pid = std::process::id();
        app.current_api_peer_pid = Some(pid);
        assert_eq!(code(&call(&mut app, arm("self", "x"))), "no_caller_pane");
        assert_eq!(
            code(&call(&mut app, arm("agent_other", "x"))),
            "agent_not_found"
        );

        caller_is_an_agent(&mut app);
        app.record_goal_event(EventEnvelope {
            event: EventKind::GoalArmed,
            data: EventData::GoalArmed {
                agent_id: "agent_other".into(),
                record: GoalRecord {
                    goal: "not yours".into(),
                    ..Default::default()
                },
                reissued: false,
            },
        });
        let written = goal_lines(&hub).len();
        let writes = |target: &str| {
            [
                arm(target, "take over"),
                pause(target, None),
                resume(target),
                stop(target),
            ]
        };
        for method in writes("agent_other") {
            assert_eq!(code(&call(&mut app, method)), "goal_caller_refused");
        }
        app.current_api_peer_pid = None;
        for method in writes("agent_other") {
            assert_eq!(code(&call(&mut app, method)), "goal_caller_refused");
        }
        assert_eq!(goal_lines(&hub).len(), written, "a refusal writes nothing");

        let read = call(&mut app, status("agent_other"));
        assert_eq!(read["result"]["goal"]["goal"], "not yours", "{read}");
        shutdown(app);
        let _ = std::fs::remove_dir_all(log.parent().expect("log dir"));
    }

    /// The state refusals: arm over an open goal, a blank goal, a zero-minute
    /// pause, resume when not paused, and every verb on no goal or a met one.
    #[tokio::test]
    async fn state_refusals_name_the_state() {
        let log = temp_log("goal-verbs-state");
        let hub = EventHub::with_persistence(log.clone());
        let mut app = app_on(hub.clone());
        let agent = caller_is_an_agent(&mut app);

        for method in [
            status("self"),
            pause("self", None),
            resume("self"),
            stop("self"),
        ] {
            assert_eq!(code(&call(&mut app, method)), "goal_not_armed");
        }
        assert_eq!(code(&call(&mut app, arm("self", "  "))), "invalid_params");
        assert_eq!(code(&call(&mut app, arm("self", "ship"))), "<success>");
        assert_eq!(
            code(&call(&mut app, arm("self", "again"))),
            "goal_already_armed"
        );
        assert_eq!(
            code(&call(&mut app, pause("self", Some(0)))),
            "invalid_params"
        );
        assert_eq!(code(&call(&mut app, resume("self"))), "goal_not_paused");

        app.record_goal_event(EventEnvelope {
            event: EventKind::GoalMet,
            data: EventData::GoalMet {
                agent_id: agent.clone(),
            },
        });
        assert_eq!(code(&call(&mut app, pause("self", None))), "goal_not_armed");
        assert_eq!(code(&call(&mut app, resume("self"))), "goal_not_armed");
        assert_eq!(
            call(&mut app, status("self"))["result"]["goal"]["met"],
            true
        );
        assert_eq!(
            code(&call(&mut app, arm("self", "next"))),
            "<success>",
            "a met goal is closed, so a new one may be armed"
        );
        shutdown(app);
        let _ = std::fs::remove_dir_all(log.parent().expect("log dir"));
    }
}
