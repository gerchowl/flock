//! The question policy an agent starts with (#1179, ADR-0034 §2).
//!
//! `agent.start`, `agent.spawn` and `agent.fork` decide it from who is asking
//! before anything is allocated, then stamp it on the new agent's record. The
//! server knows three starters: an operator (who may be watching the pane), an
//! attested agent (which records an edge), and the server itself (nobody
//! watches). Until server-initiated starts land (#1073, #1074), `unattended`
//! is how a start says it is one; they will pass it too.

use crate::api::error_code::ErrorCode;
use crate::api::schema::{
    HookDecideParams, HookHarness, HookPoint, HookVerdict, QuestionPolicy, ResponseResult,
};

use super::api::responses::{encode_error, encode_success};
use super::{App, SpawnCaller};

/// The reason a `deny` agent's question is refused with (ADR-0034 §3). The
/// fail-closed stub prints the same sentence for `FLOCK_QUESTIONS=deny`.
pub(crate) const DENY_REASON: &str = "Questions are disabled for this agent. Choose the most \
     conservative option, state the assumption in your result, or end your turn with a blocked \
     report.";

/// The reason a `route` or `ask_operator` agent's question is refused with
/// until its question can be routed and answered (#1183). The agent's final
/// message is what its architect, or the operator, reads.
pub(crate) const UNROUTED_REASON: &str =
    "state the question and options in your final message and end your turn";

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

/// The `question` point's verdict (ADR-0034 §3): a pure function of the
/// agent's policy and the tool it is about to call. A tool that is neither the
/// harness's question tool nor in `[questions] human_tools` passes, whatever
/// the policy, so a hook matcher wider than the tool list costs nothing.
pub(crate) fn decide_question(
    policy: QuestionPolicy,
    harness: HookHarness,
    tool_name: Option<&str>,
    human_tools: &[String],
) -> (HookVerdict, Option<&'static str>) {
    let asks = tool_name.is_some_and(|tool| {
        harness.question_tools().contains(&tool) || human_tools.iter().any(|human| human == tool)
    });
    match policy {
        _ if !asks => (HookVerdict::Pass, None),
        QuestionPolicy::AskHuman => (HookVerdict::Pass, None),
        QuestionPolicy::Deny => (HookVerdict::Deny, Some(DENY_REASON)),
        QuestionPolicy::Route | QuestionPolicy::AskOperator => {
            (HookVerdict::Deny, Some(UNROUTED_REASON))
        }
    }
}

impl App {
    /// `hook.decide` (ADR-0034 §1). The pane is the caller's by process
    /// ancestry, and the policy is its agent record's, never the request's or
    /// the pane's `FLOCK_QUESTIONS` hint.
    pub(super) fn handle_hook_decide(&mut self, id: String, params: HookDecideParams) -> String {
        let policy = self.caller_pane().and_then(|(ws_idx, pane_id)| {
            let terminal_id = self.state.workspaces.get(ws_idx)?.terminal_id(pane_id)?;
            Some(self.state.terminals.get(terminal_id)?.questions)
        });
        let Some(policy) = policy else {
            return encode_error(
                id,
                ErrorCode::NoCallerPane.as_str(),
                "hook.decide comes from an agent's hook, and this caller's process ancestry \
                 does not land in a pane of this session",
            );
        };
        let (verdict, reason) = match params.point {
            HookPoint::Question => decide_question(
                policy,
                params.harness,
                params.tool_name.as_deref(),
                &self.state.config.questions.human_tools,
            ),
        };
        encode_success(
            id,
            ResponseResult::HookDecide {
                verdict,
                reason: reason.map(str::to_string),
            },
        )
    }

    /// [`resolve`] for an attested caller.
    pub(super) fn admit_questions(
        caller: &SpawnCaller,
        asked: Option<QuestionPolicy>,
        unattended: bool,
    ) -> Result<QuestionPolicy, WideningRefused> {
        resolve(
            matches!(
                caller,
                SpawnCaller::Agent { .. } | SpawnCaller::Structured { .. }
            ),
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

    /// ADR-0034 §3's verdicts until #1183: `ask_human` passes, `deny` is
    /// refused with its sentence, and `route` and `ask_operator` are told to
    /// put the question in their final message. Only question tools are
    /// judged.
    #[test]
    fn a_question_is_decided_by_the_policy_and_nothing_else_is_judged() {
        use crate::api::schema::{HookHarness, HookVerdict};
        let human = vec!["mcp__tg-mcp__ask".to_string()];
        let deny = (HookVerdict::Deny, Some(DENY_REASON));
        let unrouted = (HookVerdict::Deny, Some(UNROUTED_REASON));
        let pass = (HookVerdict::Pass, None);
        for (harness, tool) in [
            (HookHarness::Claude, "AskUserQuestion"),
            (HookHarness::Claude, "ExitPlanMode"),
            (HookHarness::Claude, "mcp__tg-mcp__ask"),
            (HookHarness::Codex, "request_user_input"),
            (HookHarness::Opencode, "question"),
        ] {
            let decide = |policy| decide_question(policy, harness, Some(tool), &human);
            assert_eq!(decide(AskHuman), pass, "{tool}");
            assert_eq!(decide(Deny), deny, "{tool}");
            assert_eq!(decide(Route), unrouted, "{tool}");
            assert_eq!(decide(AskOperator), unrouted, "{tool}");
        }
        for policy in [AskHuman, AskOperator, Route, Deny] {
            for tool in [
                Some("Read"),
                Some("Bash"),
                Some("mcp__tg-mcp__ask_later"),
                None,
            ] {
                assert_eq!(
                    decide_question(policy, HookHarness::Claude, tool, &human),
                    pass,
                    "{tool:?}"
                );
            }
            // Another harness's question tool is not this one's.
            assert_eq!(
                decide_question(policy, HookHarness::Claude, Some("request_user_input"), &[]),
                pass
            );
        }
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

/// `hook.decide` through the request path: the pane comes from the caller's
/// ancestry and the policy from that pane's record (ADR-0034 §1).
#[cfg(test)]
mod hook_decide_tests {
    use crate::api::schema::{HookDecideParams, HookHarness, HookPoint, Method, Request};
    use crate::api::EventHub;
    use crate::app::delegation::handler_tests::{app_on, caller_is_an_agent, shutdown, temp_log};
    use crate::app::App;

    use super::*;

    fn decide(app: &mut App, tool: &str) -> serde_json::Value {
        let response = app.handle_api_request(Request {
            id: "hd".into(),
            method: Method::HookDecide(HookDecideParams {
                point: HookPoint::Question,
                harness: HookHarness::Claude,
                session_id: None,
                tool_name: Some(tool.into()),
            }),
        });
        serde_json::from_str(&response).expect(&response)
    }

    fn set_policy(app: &mut App, policy: QuestionPolicy) {
        let pane_id = app.state.workspaces[0].focused_pane_id().expect("pane");
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .expect("terminal id");
        app.state
            .terminals
            .get_mut(terminal_id)
            .expect("terminal")
            .questions = policy;
    }

    #[tokio::test]
    async fn the_callers_own_record_decides_and_human_tools_come_from_config() {
        let mut app = app_on(EventHub::with_persistence(temp_log("hook-decide")));
        caller_is_an_agent(&mut app);

        let root = decide(&mut app, "AskUserQuestion");
        assert_eq!(root["result"]["verdict"], "pass", "{root}");
        assert!(root["result"].get("reason").is_none(), "{root}");

        set_policy(&mut app, QuestionPolicy::Route);
        let routed = decide(&mut app, "ExitPlanMode");
        assert_eq!(routed["result"]["type"], "hook_decide", "{routed}");
        assert_eq!(routed["result"]["verdict"], "deny");
        assert_eq!(routed["result"]["reason"], UNROUTED_REASON);
        assert_eq!(decide(&mut app, "mcp__x__ask")["result"]["verdict"], "pass");

        app.state.config.questions.human_tools = vec!["mcp__x__ask".into()];
        set_policy(&mut app, QuestionPolicy::Deny);
        let human = decide(&mut app, "mcp__x__ask");
        assert_eq!(human["result"]["verdict"], "deny");
        assert_eq!(human["result"]["reason"], DENY_REASON);
        shutdown(app);
    }

    /// A caller outside every pane is refused, and the hook then falls back
    /// to its hint (ADR-0034 §4): the server never guesses a policy.
    #[tokio::test]
    async fn an_unattested_caller_gets_no_caller_pane() {
        let mut app = app_on(EventHub::with_persistence(temp_log("hook-decide-none")));
        app.current_api_peer_pid = None;

        let refused = decide(&mut app, "AskUserQuestion");

        assert_eq!(refused["error"]["code"], "no_caller_pane", "{refused}");
        shutdown(app);
    }
}
