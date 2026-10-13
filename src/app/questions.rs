//! The question policy an agent starts with (#1179, ADR-0034 §2).
//!
//! `agent.start`, `agent.spawn` and `agent.fork` decide it from who is asking
//! before anything is allocated, then stamp it on the new agent's record. The
//! server knows three starters: an operator (who may be watching the pane), an
//! attested agent (which records an edge), and the server itself (nobody
//! watches). Until server-initiated starts land (#1073, #1074), `unattended`
//! is how a start says it is one; they will pass it too.

use crate::api::schema::QuestionPolicy;

use super::{App, SpawnCaller};

/// An agent caller asked for a policy that reaches a human (ADR-0034 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WideningRefused {
    /// What the caller asked for: the explicit policy, or `ask_operator` for
    /// `unattended`.
    pub(crate) asked: QuestionPolicy,
}

impl WideningRefused {
    pub(crate) const CODE: &'static str = "questions_widening_refused";

    pub(crate) fn message(self) -> String {
        format!(
            "an agent may start its delegate with questions route or deny, not {}: a policy that \
             reaches a human, and --unattended, are operator verbs (ADR-0034 §2)",
            self.asked.as_str()
        )
    }
}

/// The policy a start gets. An explicit `asked` wins over the default. An
/// agent caller may only narrow, to `route` or `deny`, and a delegate never
/// chooses its own policy, because it does not exist yet when this runs.
pub(crate) fn resolve(
    caller_is_agent: bool,
    asked: Option<QuestionPolicy>,
    unattended: bool,
) -> Result<QuestionPolicy, WideningRefused> {
    if caller_is_agent {
        if unattended {
            return Err(WideningRefused {
                asked: QuestionPolicy::AskOperator,
            });
        }
        return match asked {
            Some(asked) if asked.reaches_human() => Err(WideningRefused { asked }),
            Some(asked) => Ok(asked),
            None => Ok(QuestionPolicy::Route),
        };
    }
    Ok(asked.unwrap_or(if unattended {
        QuestionPolicy::AskOperator
    } else {
        QuestionPolicy::AskHuman
    }))
}

impl App {
    /// [`resolve`] for an attested caller.
    pub(super) fn admit_questions(
        caller: &SpawnCaller,
        asked: Option<QuestionPolicy>,
        unattended: bool,
    ) -> Result<QuestionPolicy, WideningRefused> {
        resolve(
            matches!(caller, SpawnCaller::Agent { .. }),
            asked,
            unattended,
        )
    }

    /// Stamp the policy on the record of the agent in `pane_id`.
    pub(super) fn record_questions(
        &mut self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        policy: QuestionPolicy,
    ) {
        let terminal = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.terminal_id(pane_id))
            .and_then(|id| self.state.terminals.get_mut(id));
        if let Some(terminal) = terminal {
            terminal.questions = policy;
            self.state.mark_session_dirty();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use QuestionPolicy::*;

    #[test]
    fn the_default_follows_the_starter() {
        assert_eq!(resolve(true, None, false), Ok(Route), "agent started");
        assert_eq!(resolve(false, None, false), Ok(AskHuman), "operator");
        assert_eq!(resolve(false, None, true), Ok(AskOperator), "unattended");
    }

    #[test]
    fn an_operator_may_set_any_policy_and_an_explicit_one_wins_over_unattended() {
        for policy in [AskHuman, AskOperator, Route, Deny] {
            assert_eq!(resolve(false, Some(policy), false), Ok(policy));
            assert_eq!(resolve(false, Some(policy), true), Ok(policy));
        }
    }

    #[test]
    fn an_agent_may_only_narrow() {
        assert_eq!(resolve(true, Some(Route), false), Ok(Route));
        assert_eq!(resolve(true, Some(Deny), false), Ok(Deny));
        for asked in [AskHuman, AskOperator] {
            assert_eq!(
                resolve(true, Some(asked), false),
                Err(WideningRefused { asked })
            );
        }
        assert_eq!(
            resolve(true, Some(Deny), true),
            Err(WideningRefused { asked: AskOperator }),
            "unattended is an operator verb even beside a narrow policy"
        );
    }

    #[test]
    fn the_refusal_exits_13() {
        let code = crate::api::error_code::ErrorCode::from_code(WideningRefused::CODE)
            .expect("registered");
        assert_eq!(code.exit(), crate::cli::exit::POLICY_REFUSED);
        assert_eq!(code.exit(), 13);
    }
}

/// The acceptance criteria of #1179, through `agent.start` on a real pane.
#[cfg(test)]
mod handler_tests {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use crate::api::schema::{AgentStartParams, AgentTarget, Method, QuestionPolicy, Request};
    use crate::api::EventHub;
    use crate::app::delegation::handler_tests::{app_on, caller_is_an_agent, shutdown, temp_log};
    use crate::app::App;

    /// Start a child that writes the `FLOCK_QUESTIONS` it was launched with
    /// to `hint`, then waits on its terminal.
    fn start(
        app: &mut App,
        hint: &Path,
        questions: Option<QuestionPolicy>,
        unattended: bool,
    ) -> serde_json::Value {
        let name = format!("worker-{}", app.state.terminals.len());
        let script = format!(
            "printf %s \"${{FLOCK_QUESTIONS-unset}}\" > '{}'; read -r line",
            hint.display()
        );
        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: Method::AgentStart(AgentStartParams {
                dry_run: false,
                name,
                cwd: None,
                workspace_id: Some(app.public_workspace_id(0)),
                tab_id: None,
                split: None,
                active: false,
                here: false,
                focus: false,
                argv: vec![crate::test_support::live_program(), "-c".into(), script],
                label: None,
                questions,
                unattended,
            }),
        });
        serde_json::from_str(&response).expect(&response)
    }

    /// `agent.get` on the started agent: the policy is on its record.
    fn policy_of(app: &mut App, started: &serde_json::Value) -> serde_json::Value {
        let target = started["result"]["agent"]["agent_id"]
            .as_str()
            .unwrap_or_else(|| panic!("{started}"))
            .to_string();
        let response = app.handle_api_request(Request {
            id: "get".into(),
            method: Method::AgentGet(AgentTarget { target }),
        });
        let got: serde_json::Value = serde_json::from_str(&response).expect(&response);
        got["result"]["agent"]["questions"].clone()
    }

    /// The child writes the file right after it execs. Its existence is the
    /// fact waited on, bounded so a child that never ran fails the test.
    fn hint_seen(hint: &Path) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(text) = std::fs::read_to_string(hint) {
                if !text.is_empty() {
                    return text;
                }
            }
            assert!(Instant::now() < deadline, "the child never wrote {hint:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn rig(name: &str) -> (App, PathBuf, PathBuf) {
        let log = temp_log(name);
        let dir = log.parent().expect("log dir").to_path_buf();
        std::fs::create_dir_all(&dir).expect("temp dir");
        (
            app_on(EventHub::with_persistence(log)),
            dir.clone(),
            dir.join("hint"),
        )
    }

    #[tokio::test]
    async fn an_agent_started_by_an_agent_reads_route_and_its_pane_carries_the_hint() {
        let (mut app, dir, hint) = rig("questions-agent");
        caller_is_an_agent(&mut app);

        let started = start(&mut app, &hint, None, false);

        assert_eq!(started["result"]["agent"]["questions"], "route");
        assert_eq!(policy_of(&mut app, &started), "route");
        assert_eq!(hint_seen(&hint), "route");
        shutdown(app);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn an_operator_start_reads_ask_human_with_no_hint() {
        let (mut app, dir, hint) = rig("questions-operator");

        let started = start(&mut app, &hint, None, false);

        assert_eq!(policy_of(&mut app, &started), "ask_human");
        assert_eq!(hint_seen(&hint), "unset");
        shutdown(app);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_server_started_agent_reads_ask_operator() {
        let (mut app, dir, hint) = rig("questions-unattended");

        let started = start(&mut app, &hint, None, true);

        assert_eq!(policy_of(&mut app, &started), "ask_operator");
        assert_eq!(hint_seen(&hint), "ask_operator");
        shutdown(app);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Widening is refused before anything is placed: no pane, no agent.
    #[tokio::test]
    async fn an_agent_widening_a_delegate_is_refused_and_nothing_starts() {
        let (mut app, dir, hint) = rig("questions-widen");
        caller_is_an_agent(&mut app);
        let before = app.state.terminals.len();

        for (questions, unattended) in [
            (Some(QuestionPolicy::AskHuman), false),
            (Some(QuestionPolicy::AskOperator), false),
            (None, true),
        ] {
            let refused = start(&mut app, &hint, questions, unattended);
            assert_eq!(
                refused["error"]["code"],
                super::WideningRefused::CODE,
                "{refused}"
            );
            assert_eq!(crate::cli::error_exit(&refused, false), 13);
        }
        assert_eq!(app.state.terminals.len(), before);

        let narrowed = start(&mut app, &hint, Some(QuestionPolicy::Deny), false);
        assert_eq!(policy_of(&mut app, &narrowed), "deny");
        assert_eq!(hint_seen(&hint), "deny");
        shutdown(app);
        let _ = std::fs::remove_dir_all(dir);
    }
}
