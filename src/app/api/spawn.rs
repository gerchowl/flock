//! `agent.spawn` — agent-initiated spawn of a fresh agent (#329, ADR-0014).
//!
//! The narrowed sibling of `agent.start`. Where that verb takes raw argv and
//! any cwd, this one takes a closed [`AgentKind`], a prompt, and a location
//! naming something flock already tracks. The difference is not politeness:
//! `agent.start`'s shape means a constraint applied by one caller leaves the
//! escape hatch open in the TYPE for every future caller.

use crate::api::schema::{AgentSpawnParams, ResponseResult, SpawnLocation};
use crate::app::App;
use crate::spawn::{admit, AgentKind, SpawnCensus, SpawnRefusal, SpawnVerb};

use super::responses::{encode_error, encode_error_with_data, encode_success};

/// Cap on the opening turn. Bounded so a caller cannot push an unbounded
/// body through the socket; generous enough for a real dispatch brief.
const MAX_PROMPT_BYTES: usize = 16 * 1024;

/// Who is on the other end of a spawn-shaped request (#349, ADR-0014 §6).
///
/// The ceiling gates a caller CLASS, not a verb. `fleet.pause` already draws
/// this line for the same reason — it halts what flock initiates and leaves
/// human keystrokes alone — and a cap that stops the operator from opening a
/// worktree is a cap that gets turned off.
///
/// Process ancestry of the API peer is the only evidence either way. A
/// caller-supplied identity would be a claim, and depth is exactly the thing a
/// runaway caller would want to lie about.
pub(super) enum SpawnCaller {
    /// Ancestry attests a live pane that is running an agent. Whatever it
    /// asks for is agent-initiated, whichever verb it reached for.
    Agent {
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        /// The parent stamped on the child, and the key its fanout is counted
        /// under. Carried from the classification rather than re-read later,
        /// so there is no path where a child is admitted against one parent
        /// and recorded against another — or against `Some("")`, which would
        /// pool every such child into ONE fanout counter and make none of
        /// them attributable.
        agent_id: String,
    },
    /// Ancestry attests a pane that is not an agent — an operator's own shell,
    /// inside flock or outside it — or attests nothing at all. Either way
    /// there is no agent to bill a child to.
    ///
    /// The two verbs read this oppositely, and deliberately. `flk agent fork`
    /// typed by a human is a real and supported thing to do, so fork proceeds
    /// unbounded. `agent.spawn` has no CLI surface at all — every caller is an
    /// MCP client — so an unattested one there is a caller the ceiling cannot
    /// bound, and it refuses.
    Operator,
}

/// The lineage a child admitted by the funnel must be stamped with.
///
/// Returned by the gate rather than recomputed at the call site: the numbers
/// the ceiling decided on and the numbers written onto the child are then the
/// same numbers, and a child cannot be admitted at one depth and recorded at
/// another.
#[derive(Debug)]
pub(super) struct SpawnLineage {
    pub parent_agent_id: String,
    pub child_depth: u32,
}

impl App {
    pub(super) fn handle_agent_spawn(&mut self, id: String, params: AgentSpawnParams) -> String {
        // Node-level gates first: whether this node offers the verb at all,
        // and whether the fleet is paused, do not depend on WHO is asking.
        // Resolving the caller first made a disabled node report
        // `spawn_caller_unknown`, which sends an operator debugging identity
        // when the real answer is "you never turned this on".
        // `admit` re-checks both below. That duplication is deliberate: the
        // gate stays the single source of truth for the ORDER of refusals,
        // while these two run before caller attestation so the answer to
        // "why did nothing happen" is the real reason rather than an
        // identity puzzle. Neither can drift, because both read the same
        // config.
        let fleet = self.state.config.fleet;
        if !fleet.agent_spawn_enabled {
            let refusal = SpawnRefusal::NotEnabled;
            return encode_error_with_data(
                id,
                refusal.code(),
                refusal.message(),
                refusal_data(&refusal),
            );
        }
        if self.fleet_pause.paused {
            let refusal = SpawnRefusal::FleetPaused;
            return encode_error_with_data(
                id,
                refusal.code(),
                refusal.message(),
                refusal_data(&refusal),
            );
        }

        let Some(kind) = AgentKind::parse(params.agent.trim()) else {
            return encode_error_with_data(
                id,
                "unknown_agent_kind",
                format!("unknown agent kind {:?}", params.agent),
                serde_json::json!({
                    "refusal": "unknown_agent_kind",
                    "retryable": false,
                    "requested": params.agent,
                    "supported": AgentKind::supported(),
                }),
            );
        };

        let prompt = params.prompt.trim();
        if prompt.is_empty() {
            return encode_error(id, "invalid_request", "prompt is required");
        }
        if prompt.len() > MAX_PROMPT_BYTES {
            return encode_error(
                id,
                "invalid_request",
                format!("prompt exceeds {MAX_PROMPT_BYTES} bytes"),
            );
        }

        // Who is asking, and whether the ceiling lets them. Both answered by
        // the shared funnel `agent.fork` also goes through, so a limit cannot
        // be enforced on one verb and skipped by the next.
        //
        // Fail CLOSED on an operator caller: `agent.spawn` has no CLI surface,
        // so "not attested as an agent" here means ancestry told us nothing at
        // all — and without a caller we can bound neither depth nor fanout.
        // An unbounded spawn verb is the thing this whole path exists to
        // prevent, which is why fork's answer to the same class is the
        // opposite one.
        let caller = self.spawn_caller();
        let lineage = match self.admit_spawn(SpawnVerb::Spawn, &caller, None) {
            Ok(Some(lineage)) => lineage,
            Ok(None) => {
                return encode_error_with_data(
                    id,
                    "spawn_caller_unknown",
                    "agent.spawn could not attest which agent is calling; refusing to spawn unbounded",
                    serde_json::json!({ "refusal": "spawn_caller_unknown", "retryable": false }),
                );
            }
            Err(refusal) => {
                return encode_error_with_data(
                    id,
                    refusal.code(),
                    refusal.message(),
                    refusal_data(&refusal),
                );
            }
        };

        let Some(target_ws) = self.resolve_spawn_location(&params.location) else {
            return encode_error_with_data(
                id,
                "spawn_location_unresolved",
                "the requested location is not a checkout this server has open",
                serde_json::json!({
                    "refusal": "spawn_location_unresolved",
                    "retryable": false,
                }),
            );
        };
        let Some(cwd) = self
            .state
            .workspaces
            .get(target_ws)
            .map(|ws| ws.identity_cwd.clone())
        else {
            return encode_error(id, "internal_error", "resolved workspace has no cwd");
        };

        // #359: the child is exec'd from argv with no shell in between, so the
        // agent profile the REQUESTER is on has to be carried explicitly or the
        // child silently starts against the default one — which may be a
        // different authenticated account, not merely a logged-out one.
        // Resolved before any state is mutated so a refusal costs nothing.
        let argv = kind.argv(prompt);
        let spawn_env = match self.resolve_spawn_env(&argv, self.current_api_peer_pid, None) {
            Ok(vars) => vars,
            Err(unresolved) => {
                let refusal = SpawnRefusal::ProfileUnresolved(unresolved);
                return encode_error_with_data(
                    id,
                    refusal.code(),
                    refusal.message(),
                    refusal_data(&refusal),
                );
            }
        };

        let run_id = crate::app::worktrees::generated_fork_run_id();
        self.install_run_trailer_if_enabled(&cwd);
        // Guard, not a bare set: a spawn failure below must disarm the id, or
        // the next unrelated pane inherits it and `flk revert-run` reverts
        // that pane's work instead.
        let _run_id_guard = crate::integration::set_pending_run_id(run_id.clone());
        // #347 / ADR-0014 §3: the child is asked for by an AGENT, so it starts
        // from a scrubbed baseline and inherits only what its own CLI needs.
        // For an operator-started pane inheriting the lot is correct — it is
        // their shell — which is exactly why this is armed here and not
        // globally. Resolved from the same argv that picked the profile keys,
        // so both halves of the child's environment answer for one agent.
        let _allowlist_guard = crate::integration::set_pending_spawn_allowlist(
            crate::spawn::allowlist::for_argv(&argv),
        );
        // Armed after the allowlist, and dropped with it: the profile the
        // requester runs under is handed down deliberately, and the server
        // never had it to allow through in the first place.
        let _spawn_env_guard = crate::integration::set_pending_spawn_env(spawn_env);

        let (rows, cols) = self.state.estimate_pane_size();
        let spawned = self.spawn_agent_workspace(cwd, rows, cols, &argv, params.focus);
        let (ws_idx, pane_id) = match spawned {
            Ok((ws_idx, _, pane_id)) => (ws_idx, pane_id),
            Err(err) => {
                let body = self.agent_start_error_body(err);
                return encode_error_with_data(
                    id,
                    "spawn_failed",
                    body.message,
                    serde_json::json!({ "refusal": "spawn_failed", "retryable": false }),
                );
            }
        };

        // Stamp the child's lineage BEFORE anything can read it. The run id
        // is what its commits will carry; depth and parent are what the
        // ceiling reads on the child's own next spawn attempt.
        self.record_spawn_run_id(ws_idx, pane_id, &run_id);
        self.record_spawn_lineage(
            ws_idx,
            pane_id,
            lineage.child_depth,
            &lineage.parent_agent_id,
        );

        if let Some(name) = params
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
        {
            self.set_spawned_agent_name(ws_idx, pane_id, name);
        }

        let Some(agent) = self.agent_info(ws_idx, pane_id) else {
            return encode_error(id, "internal_error", "spawned pane is not an agent");
        };
        encode_success(id, ResponseResult::AgentSpawned { run_id, agent })
    }

    /// Classify the caller of a spawn-shaped request (#349).
    ///
    /// An AGENT is a caller whose process ancestry lands in a live pane that
    /// is running one. Anything else — a shell pane, an operator's terminal
    /// outside flock, a caller ancestry cannot place at all — is the operator,
    /// because none of them is an agent whose children the ceiling is counting.
    pub(super) fn spawn_caller(&mut self) -> SpawnCaller {
        let Some((ws_idx, pane_id)) = self.parse_pane_id_or_peer("", self.current_api_peer_pid)
        else {
            return SpawnCaller::Operator;
        };
        let agent_id = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.pane_state(pane_id))
            .and_then(|pane| self.state.terminals.get(&pane.attached_terminal_id))
            .filter(|terminal| terminal.is_agent_terminal())
            .map(|terminal| terminal.agent_id.to_string())
            .filter(|agent_id| !agent_id.is_empty());
        match agent_id {
            Some(agent_id) => SpawnCaller::Agent {
                ws_idx,
                pane_id,
                agent_id,
            },
            None => SpawnCaller::Operator,
        }
    }

    /// The shared spawn funnel: may this caller put another agent on the node,
    /// and under whose lineage (ADR-0014 §6, #349).
    ///
    /// Every agent-initiated path asks HERE. The ceiling first shipped inside
    /// `agent.spawn`'s handler, which meant an agent refused by
    /// `at_agent_capacity` could call `agent.fork` instead — same fleet cost,
    /// no ceiling at all. A constraint applied at one call site is one refactor
    /// from gone; this one did not even need the refactor.
    ///
    /// `Ok(None)` is an operator caller: unbounded, and nothing to stamp. Each
    /// verb decides what that means — see [`SpawnCaller::Operator`].
    ///
    /// `inherits_depth_from` is the pane whose CONVERSATION the child
    /// continues, when that is not the caller's own — a fork of somebody
    /// else's session. The child sits below both, so it takes the deeper: a
    /// caller already at the depth limit must not be able to launder its depth
    /// away by forking something shallow.
    pub(super) fn admit_spawn(
        &mut self,
        verb: SpawnVerb,
        caller: &SpawnCaller,
        inherits_depth_from: Option<(usize, crate::layout::PaneId)>,
    ) -> Result<Option<SpawnLineage>, SpawnRefusal> {
        let SpawnCaller::Agent {
            ws_idx,
            pane_id,
            agent_id,
        } = caller
        else {
            return Ok(None);
        };
        let mut census = self.spawn_census(*ws_idx, *pane_id);
        if let Some((ws_idx, pane_id)) = inherits_depth_from {
            census.child_depth = census
                .child_depth
                .max(self.pane_spawn_depth(ws_idx, pane_id).saturating_add(1));
        }
        admit(
            &self.state.config.fleet,
            verb,
            census,
            self.fleet_pause.paused,
        )?;
        Ok(Some(SpawnLineage {
            parent_agent_id: agent_id.clone(),
            child_depth: census.child_depth,
        }))
    }

    /// How deep in the spawn tree a pane's terminal already sits. An untracked
    /// pane reads as 0 — the operator's own root, which is what a pane with no
    /// stamp is.
    fn pane_spawn_depth(&self, ws_idx: usize, pane_id: crate::layout::PaneId) -> u32 {
        self.state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.pane_state(pane_id))
            .and_then(|pane| self.state.terminals.get(&pane.attached_terminal_id))
            .map_or(0, |terminal| terminal.spawn_depth)
    }

    /// Live counts the ceiling needs, read from terminal state.
    fn spawn_census(&self, caller_ws: usize, caller_pane: crate::layout::PaneId) -> SpawnCensus {
        let caller_terminal = self
            .state
            .workspaces
            .get(caller_ws)
            .and_then(|ws| ws.pane_state(caller_pane))
            .and_then(|pane| self.state.terminals.get(&pane.attached_terminal_id));
        let caller_depth = caller_terminal.map_or(0, |t| t.spawn_depth);
        let caller_agent_id = caller_terminal.map(|t| t.agent_id.to_string());

        SpawnCensus {
            // Agent-STARTED agents only. An operator's own panes are not
            // counted: the cap bounds what agents do, and counting the
            // operator's work against it would make a busy human shrink the
            // fleet's headroom for no safety gain.
            // "Live" means present in `state.terminals`, which includes a
            // HIBERNATED agent: its pane is parked but resumable, and its
            // worktree still exists. Counting it is the conservative read —
            // a fleet that forgot about parked agents would admit past the
            // cap and then find them all resumed at once.
            live_agent_started: self
                .state
                .terminals
                .values()
                .filter(|t| t.spawned_by.is_some())
                .count(),
            parent_live_children: caller_agent_id
                .as_deref()
                .map(|parent| {
                    self.state
                        .terminals
                        .values()
                        .filter(|t| t.spawned_by.as_deref() == Some(parent))
                        .count()
                })
                .unwrap_or(0),
            child_depth: caller_depth.saturating_add(1),
        }
    }

    pub(super) fn record_spawn_lineage(
        &mut self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        depth: u32,
        parent_agent_id: &str,
    ) {
        let terminal_id = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.pane_state(pane_id))
            .map(|pane| pane.attached_terminal_id.clone());
        if let Some(terminal) = terminal_id.and_then(|tid| self.state.terminals.get_mut(&tid)) {
            terminal.spawn_depth = depth;
            terminal.spawned_by = Some(parent_agent_id.to_string());
        }
    }

    fn set_spawned_agent_name(
        &mut self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        name: &str,
    ) {
        let terminal_id = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.pane_state(pane_id))
            .map(|pane| pane.attached_terminal_id.clone());
        if let Some(terminal) = terminal_id.and_then(|tid| self.state.terminals.get_mut(&tid)) {
            terminal.set_agent_name(name.to_string());
        }
    }

    /// Resolve a [`SpawnLocation`] to an OPEN workspace index.
    ///
    /// Every variant names something this server already tracks — that is the
    /// whole reason the location is a union rather than a path string.
    fn resolve_spawn_location(&self, location: &SpawnLocation) -> Option<usize> {
        match location {
            SpawnLocation::WorktreePath { path } => {
                let canonical = crate::worktree::canonical_or_original(std::path::Path::new(path));
                self.open_workspace_idx_for_checkout(&canonical)
            }
            SpawnLocation::WorkspaceId { workspace_id } => self.parse_workspace_id(workspace_id),
            // Creating a worktree from inside the spawn path means owning the
            // whole `git worktree add` + membership-stamping flow that
            // `agent.fork` runs on a thread. Refused explicitly rather than
            // half-done: a caller can create the checkout first and spawn
            // into it by path.
            SpawnLocation::NewBranch { .. } => None,
        }
    }
}

pub(super) fn refusal_data(refusal: &SpawnRefusal) -> serde_json::Value {
    let mut data = serde_json::json!({
        "refusal": refusal.code(),
        "retryable": refusal.retryable(),
    });
    let map = data.as_object_mut().expect("json object");
    match refusal {
        SpawnRefusal::AtCapacity { current, limit } | SpawnRefusal::AtFanout { current, limit } => {
            map.insert("current".into(), (*current).into());
            map.insert("limit".into(), (*limit).into());
        }
        SpawnRefusal::AtDepth { depth, limit } => {
            map.insert("depth".into(), (*depth).into());
            map.insert("limit".into(), (*limit).into());
        }
        SpawnRefusal::ProfileUnresolved(unresolved) => {
            // Name the key the operator has to fix. Without it the caller
            // learns only that "the profile" is wrong, which is exactly the
            // mysterious-startup-break failure ADR-0014 §3 warns an allowlist
            // produces.
            if let crate::spawn::env::ProfileUnresolved::NoSuchProfile { key, value } = unresolved {
                map.insert("env_key".into(), (*key).into());
                map.insert("env_value".into(), value.clone().into());
            }
        }
        SpawnRefusal::FleetPaused | SpawnRefusal::NotEnabled => {}
    }
    data
}

#[cfg(test)]
mod tests {
    use crate::api::schema::{AgentSpawnParams, Method, Request, SpawnLocation};

    fn spawn_request(agent: &str) -> Request {
        Request {
            id: "req".into(),
            method: Method::AgentSpawn(AgentSpawnParams {
                agent: agent.into(),
                prompt: "review #42 against the ADRs".into(),
                location: SpawnLocation::WorktreePath {
                    path: "/nonexistent/checkout".into(),
                },
                name: None,
                focus: false,
            }),
        }
    }

    /// Make workspace 0's focused pane look like a live agent the API peer is
    /// running inside: an agent terminal, with this test process standing in
    /// for the pane's child so the ancestry walk lands on it.
    fn caller_pane_is_an_agent(app: &mut crate::app::App) -> (crate::layout::PaneId, String) {
        let pane_id = app.state.workspaces[0]
            .focused_pane_id()
            .expect("workspace has a pane");
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
        (pane_id, agent_id)
    }

    fn app_with_one_pane() -> crate::app::App {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("main")];
        app.state.ensure_test_terminals();
        app
    }

    fn error_of(raw: &str) -> serde_json::Value {
        serde_json::from_str::<serde_json::Value>(raw).expect("json")["error"].clone()
    }

    fn test_app() -> crate::app::App {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        crate::app::App::new(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        )
    }

    /// Off by default. Exposing a spawn verb to agents is the same
    /// "don't autopilot until the operator opts in" surface `[checks.reap]`
    /// models, and it ships the same way.
    #[tokio::test]
    async fn agent_spawn_is_refused_until_an_operator_enables_it() {
        let mut app = test_app();
        let error = error_of(&app.handle_api_request(spawn_request("claude")));
        assert_eq!(error["code"], "agent_spawn_disabled");
        assert_eq!(error["data"]["refusal"], "agent_spawn_disabled");
        assert_eq!(
            error["data"]["retryable"], false,
            "a disabled node will not become enabled by polling"
        );
    }

    /// An unknown kind must not fall through to anything, and the refusal
    /// carries the supported set so a caller learns it without a docs
    /// round-trip.
    #[tokio::test]
    async fn an_unknown_agent_kind_is_refused_with_the_supported_set() {
        let mut app = test_app();
        app.state.config.fleet.agent_spawn_enabled = true;
        let error = error_of(&app.handle_api_request(spawn_request("bash")));
        assert_eq!(error["code"], "unknown_agent_kind");
        assert_eq!(error["data"]["retryable"], false);
        assert_eq!(
            error["data"]["supported"],
            serde_json::json!(["claude"]),
            "the refusal must name what IS accepted"
        );
    }

    /// Without an attested caller the ceiling cannot bound depth or fanout,
    /// and an unbounded spawn verb is the thing this path exists to prevent.
    /// So it fails CLOSED rather than assuming depth 0.
    #[tokio::test]
    async fn a_caller_it_cannot_attest_is_refused_rather_than_assumed_to_be_root() {
        let mut app = test_app();
        app.state.config.fleet.agent_spawn_enabled = true;
        // No API peer pid is set, so ancestry attests nothing.
        let error = error_of(&app.handle_api_request(spawn_request("claude")));
        assert_eq!(error["code"], "spawn_caller_unknown");
        assert_eq!(error["data"]["retryable"], false);
    }

    /// Pause deliberately exempts human keystrokes. An agent is not a human,
    /// and a paused fleet that still spawns agents is not paused. Pause is
    /// checked BEFORE the caller is resolved, so it holds even for a caller
    /// flock cannot attest.
    #[tokio::test]
    async fn a_paused_fleet_refuses_before_it_does_any_spawn_work() {
        let mut app = test_app();
        app.state.config.fleet.agent_spawn_enabled = true;
        app.fleet_pause.paused = true;
        let before = app.state.workspaces.len();
        let error = error_of(&app.handle_api_request(spawn_request("claude")));
        assert_eq!(error["code"], "fleet_paused");
        assert_eq!(
            error["data"]["retryable"], true,
            "an operator can resume; backing off is the right response"
        );
        assert_eq!(
            app.state.workspaces.len(),
            before,
            "a refused spawn must create nothing"
        );
    }

    /// The funnel classifies by caller, not by verb. An MCP client runs
    /// inside an agent's pane, so ancestry lands on an agent terminal.
    #[tokio::test]
    async fn a_caller_running_inside_an_agent_pane_is_an_agent() {
        let mut app = app_with_one_pane();
        let (pane_id, agent_id) = caller_pane_is_an_agent(&mut app);
        match app.spawn_caller() {
            super::SpawnCaller::Agent {
                ws_idx,
                pane_id: resolved,
                agent_id: resolved_id,
            } => {
                assert_eq!(ws_idx, 0);
                assert_eq!(resolved, pane_id);
                assert_eq!(resolved_id, agent_id);
            }
            super::SpawnCaller::Operator => panic!("an agent pane must attest as an agent"),
        }
    }

    /// An operator's own shell — inside flock or outside it — is not an agent,
    /// and neither is a caller ancestry cannot place at all. Both read as the
    /// operator, because neither is an agent whose children the cap counts.
    #[tokio::test]
    async fn an_operators_shell_and_an_unattested_caller_are_both_the_operator() {
        let mut app = app_with_one_pane();
        assert!(
            matches!(app.spawn_caller(), super::SpawnCaller::Operator),
            "no peer pid attests nothing"
        );

        let pane_id = app.state.workspaces[0]
            .focused_pane_id()
            .expect("workspace has a pane");
        app.test_pane_child_pids.insert(pane_id, std::process::id());
        app.current_api_peer_pid = Some(std::process::id());
        assert!(
            matches!(app.spawn_caller(), super::SpawnCaller::Operator),
            "a shell pane is the operator's, not an agent's"
        );
    }

    /// The ceiling never applies to an operator, whichever verb they used.
    /// `agent.spawn` refuses one anyway — it has no CLI surface, so an
    /// unattested caller there is one the ceiling cannot bound.
    #[tokio::test]
    async fn an_operator_caller_is_never_gated_by_the_ceiling() {
        let mut app = app_with_one_pane();
        app.state.config.fleet.max_concurrent_agents = 0;
        app.state.config.fleet.max_spawn_fanout = 0;
        app.state.config.fleet.max_spawn_depth = 0;
        app.fleet_pause.paused = true;

        let caller = app.spawn_caller();
        let admitted = app
            .admit_spawn(crate::spawn::SpawnVerb::Fork, &caller, None)
            .expect("an operator is never refused by the ceiling");
        assert!(
            admitted.is_none(),
            "an operator fork has no parent to bill the child to"
        );
    }

    /// #349: an agent at the depth limit must not be able to launder its own
    /// depth by forking a shallower conversation. The child sits below both,
    /// so it takes the deeper of the two.
    #[tokio::test]
    async fn a_fork_child_takes_the_deeper_of_caller_and_forked_conversation() {
        let mut app = app_with_one_pane();
        app.state
            .workspaces
            .push(crate::workspace::Workspace::test_new("target"));
        app.state.ensure_test_terminals();
        let (_caller_pane, caller_agent_id) = caller_pane_is_an_agent(&mut app);

        // The forked conversation is three deep; the caller is at the root.
        let target_pane = app.state.workspaces[1]
            .focused_pane_id()
            .expect("target pane");
        let target_terminal = app.state.workspaces[1]
            .pane_state(target_pane)
            .expect("pane state")
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&target_terminal)
            .expect("terminal")
            .spawn_depth = 3;
        app.state.config.fleet.max_spawn_depth = 9;

        let caller = app.spawn_caller();
        let lineage = app
            .admit_spawn(
                crate::spawn::SpawnVerb::Fork,
                &caller,
                Some((1, target_pane)),
            )
            .expect("within the widened depth limit")
            .expect("an agent caller is stamped");
        assert_eq!(lineage.child_depth, 4);
        assert_eq!(
            lineage.parent_agent_id, caller_agent_id,
            "fanout is counted against whoever ASKED, not whoever was forked"
        );

        // And the limit binds on that deeper number, not on the caller's own.
        app.state.config.fleet.max_spawn_depth = 3;
        let refusal = app
            .admit_spawn(
                crate::spawn::SpawnVerb::Fork,
                &caller,
                Some((1, target_pane)),
            )
            .expect_err("depth 4 exceeds a limit of 3");
        assert_eq!(refusal.code(), "at_lineage_depth");
    }
}
